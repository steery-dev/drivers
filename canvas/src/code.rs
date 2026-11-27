//! A plan's side of Claude access (API.md §6.0, §6.1): `claude auth status` for the check,
//! and one warm `claude -p` per drawer in stream-json, started as its line opens so the
//! message does not wait for it: thinking off, no tools, the drawer's prompt as the system
//! prompt. Never `--bare`: it refuses a plan's sign-in.
//!
//! Its environment lacks the variables that would tie it to the session that started the
//! turn. A message that gets no `stream_event` within [`FIRST_BYTE`] fails with Claude
//! Code's last `api_retry` and its standard error's last lines.

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};
use steery_driver_common::program;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::claude::{CHECK, CLAUDE, FIRST_BYTE, Failure, Reading, Usage, kind_of};

/// The lines of standard error kept, and how many a failure says.
const KEPT: usize = 20;
const SAID: usize = 3;

/// Whether `claude auth status` says `loggedIn` within [`CHECK`].
pub async fn signed_in(claude: &Path) -> bool {
    let mut command = program::helper(claude);
    command.args(["auth", "status"]).stdout(Stdio::piped());
    let Ok(Ok(output)) = tokio::time::timeout(CHECK, command.output()).await else {
        return false;
    };
    serde_json::from_slice::<Value>(&output.stdout).is_ok_and(|status| status["loggedIn"] == true)
}

/// One drawer's `claude`.
pub struct Code {
    child: Child,
    stdin: ChildStdin,
    out: Lines<BufReader<ChildStdout>>,
    err: Arc<Mutex<VecDeque<String>>>,
}

impl Code {
    pub fn start(claude: &Path, model: &str, system: &str, cwd: &Path) -> Result<Self, Failure> {
        let mut command = Command::new(claude);
        for (name, _) in std::env::vars_os() {
            if let Some(name) = name.to_str()
                && (matches!(name, "STEERY_LEAD" | "STEERY_STARTED" | "OMPCODE" | "CLAUDECODE")
                    || name.starts_with("PI_"))
            {
                command.env_remove(name);
            }
        }
        command
            .args([
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--no-session-persistence",
                "--strict-mcp-config",
                "--include-partial-messages",
                "--system-prompt",
                system,
                "--tools",
                "",
                "--model",
                model,
                "--settings",
                r#"{"alwaysThinkingEnabled":false}"#,
            ])
            .env("MAX_THINKING_TOKENS", "0")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| Failure::other(CLAUDE, format!("Claude Code did not start: {e}")))?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(Failure::other(CLAUDE, "Claude Code gave no pipes".into()));
        };
        let err = Arc::new(Mutex::new(VecDeque::new()));
        tokio::spawn({
            let err = Arc::clone(&err);
            async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut kept = err.lock().unwrap_or_else(PoisonError::into_inner);
                    kept.push_back(line);
                    if kept.len() > KEPT {
                        kept.pop_front();
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            out: BufReader::new(stdout).lines(),
            err,
        })
    }

    /// Its standard error's last lines, in its own words.
    fn stderr(&self) -> String {
        let kept = self.err.lock().unwrap_or_else(PoisonError::into_inner);
        let skip = kept.len().saturating_sub(SAID);
        kept.iter().skip(skip).cloned().collect::<Vec<_>>().join(" | ")
    }

    /// One message: its text to `text` as it streams, until Claude Code's `result`.
    pub async fn ask(
        &mut self,
        message: &str,
        text: &mut dyn FnMut(&str),
    ) -> Result<Option<Usage>, Failure> {
        let user = json!({"type": "user", "message": {"role": "user", "content": message}});
        let sent = async {
            self.stdin.write_all(format!("{user}\n").as_bytes()).await?;
            self.stdin.flush().await
        };
        if let Err(e) = sent.await {
            return Err(Failure::other(
                CLAUDE,
                format!("Claude Code took no message: {e}"),
            ));
        }
        let mut reading = Reading::new(CLAUDE);
        let mut deadline = Some(tokio::time::Instant::now() + FIRST_BYTE);
        let mut retry: Option<Value> = None;
        loop {
            let line = match deadline {
                Some(deadline) => {
                    match tokio::time::timeout_at(deadline, self.out.next_line()).await {
                        Ok(line) => line,
                        Err(_) => return Err(self.silent(retry.as_ref())),
                    }
                }
                None => self.out.next_line().await,
            };
            let Ok(Some(line)) = line else {
                return Err(Failure::other(
                    CLAUDE,
                    format!("Claude Code exited: {}", self.exit_words().await),
                ));
            };
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match frame["type"].as_str() {
                Some("system") if frame["subtype"] == "api_retry" => retry = Some(frame),
                Some("stream_event") if frame["event"].is_object() => {
                    deadline = None;
                    reading.event(&frame["event"], text)?;
                }
                Some("result") if frame["is_error"] == true => {
                    let words = [&frame["result"], &frame["subtype"]]
                        .into_iter()
                        .find_map(|said| said.as_str().filter(|said| !said.is_empty()))
                        .unwrap_or("the turn failed");
                    return Err(Failure {
                        kind: kind_of(None, words),
                        service: CLAUDE.into(),
                        line: format!("Claude Code: {words}"),
                    });
                }
                Some("result") => return Ok(reading.spent),
                _ => {}
            }
        }
    }

    /// Why no reply came within [`FIRST_BYTE`].
    fn silent(&self, retry: Option<&Value>) -> Failure {
        let plain = |value: &Value| match value {
            Value::String(text) => text.clone(),
            Value::Null => "none".into(),
            other => other.to_string(),
        };
        let tried = retry.map_or(String::new(), |retry| {
            let error = retry["error"].as_str().unwrap_or("no words");
            format!(
                " It retried {} of {} times: {error}, HTTP status {}.",
                plain(&retry["attempt"]),
                plain(&retry["max_retries"]),
                plain(&retry["error_status"])
            )
        });
        let stderr = self.stderr();
        let said = match stderr.is_empty() {
            true => String::new(),
            false => format!(" Its stderr: {stderr}"),
        };
        Failure::other(
            CLAUDE,
            format!(
                "Claude Code sent no reply in {} s.{tried}{said}",
                FIRST_BYTE.as_secs()
            ),
        )
    }

    /// How it ended, in its own words where it said any.
    async fn exit_words(&mut self) -> String {
        let status = tokio::time::timeout(std::time::Duration::from_secs(1), self.child.wait())
            .await
            .ok()
            .and_then(Result::ok);
        // Its last words may still be on their way from its standard error.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let said = self.stderr();
        match (said.is_empty(), status) {
            (false, _) => said,
            (true, Some(status)) => format!("{status}"),
            (true, None) => "it closed its output".into(),
        }
    }
}
