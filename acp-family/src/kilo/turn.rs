//! A Kilo turn, run by this driver's own binary in its turn mode: the words on standard
//! input, Kilo's own ACP server (`kilo acp`) spoken to — `session/new`, `session/resume` or
//! `session/fork`, the model and the effort set as the session's config options, then
//! `session/prompt` with the words — every message Kilo writes and every request this turn
//! sends it written verbatim to standard output, one a line, and out once Kilo has answered
//! each prompt. What runs is the conversation Kilo's own clients run: the person's
//! configuration, MCP servers, rules and skills.
//!
//! While the turn runs, more words told on its inbox (a unix socket in the turn's folder) go
//! in as one more `session/prompt`, which Kilo queues behind the one running; the caller
//! reads back `heard`. A permission Kilo asks for goes to Steery's hook (`--hook`, setup's
//! command line), run as `<hook> session/request_permission` with the request's `params` on
//! standard input: its standard output is Kilo's answer ([`super::hooks`]). Without a hook,
//! or where the hook fails or says nothing, the call is rejected, as Kilo's own `kilo run`
//! rejects one with no person at it. SIGTERM or SIGINT cancels the turn (`session/cancel`),
//! so Kilo ends its message as aborted instead of leaving it unfinished, and every
//! permission still asked is answered cancelled; a second signal ends the turn at once. A
//! request Kilo refuses before the words go in is the one line `<refusal word>: <Kilo's
//! line>` on standard error, and the turn fails.

use std::collections::HashSet;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use steery_driver_common::kernel;
use steery_sdk::refusal;
use steery_sdk::wire::harness::HEARD;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinSet;

use super::hooks::{ASKS_WAITS, EVENT};

/// How long a caller at the inbox has to finish writing its words.
const WORDS_WITHIN: Duration = Duration::from_secs(10);

/// How long Kilo has to answer a cancel before the turn ends without its answer.
const CANCELLED_WITHIN: Duration = Duration::from_secs(15);

/// How long Kilo has to end once its standard input closes.
const CLOSED_WITHIN: Duration = Duration::from_secs(5);

/// What a turn is asked to do, as its command line says it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Asked {
    pub kilo: PathBuf,
    pub act: String,
    pub id: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub inbox: Option<PathBuf>,
    /// Steery's hook command line, which answers the permissions Kilo asks for.
    pub hook: Option<String>,
}

impl Asked {
    /// The arguments after `turn`.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--kilo".to_owned(),
            self.kilo.to_string_lossy().into_owned(),
            self.act.clone(),
        ];
        args.extend(self.id.clone());
        for (flag, value) in [("--model", &self.model), ("--thinking", &self.thinking)] {
            if let Some(value) = value {
                args.push(flag.into());
                args.push(value.clone());
            }
        }
        if let Some(inbox) = &self.inbox {
            args.push("--inbox".into());
            args.push(inbox.to_string_lossy().into_owned());
        }
        if let Some(hook) = &self.hook {
            args.push("--hook".into());
            args.push(hook.clone());
        }
        args
    }

    pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut asked = Self::default();
        while let Some(arg) = args.next() {
            let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
            match arg.as_str() {
                "--kilo" => asked.kilo = PathBuf::from(value("--kilo")?),
                "--model" => asked.model = Some(value("--model")?),
                "--thinking" => asked.thinking = Some(value("--thinking")?),
                "--inbox" => asked.inbox = Some(PathBuf::from(value("--inbox")?)),
                "--hook" => asked.hook = Some(value("--hook")?),
                _ if asked.act.is_empty() => asked.act = arg,
                _ if asked.id.is_none() => asked.id = Some(arg),
                other => return Err(format!("unexpected argument {other}")),
            }
        }
        let named = asked.id.is_some();
        let fits = match asked.act.as_str() {
            "new" => !named,
            "continue" | "fork" => named,
            _ => false,
        };
        if asked.kilo.as_os_str().is_empty() || !fits {
            return Err(
                "usage: turn --kilo <binary> <new | continue <id> | fork <id>> \
                        [--model m] [--thinking t] [--inbox path] [--hook command]"
                    .into(),
            );
        }
        Ok(asked)
    }
}

/// The turn mode's entry: run the turn its arguments ask for, and say on standard error why
/// it failed.
pub async fn main(args: impl Iterator<Item = String>) -> std::io::Result<()> {
    let outcome = match Asked::parse(args) {
        Ok(asked) => run(asked).await,
        Err(usage) => Err(format!("{}: {usage}", refusal::START_FAILED)),
    };
    if let Err(line) = outcome {
        eprintln!("{line}");
        std::process::exit(1);
    }
    Ok(())
}

/// Run one turn; the error is the line for standard error.
pub async fn run(asked: Asked) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("{}: {e}", refusal::START_FAILED);
    let mut words = String::new();
    tokio::io::stdin()
        .read_to_string(&mut words)
        .await
        .map_err(failed)?;
    let cwd = std::env::current_dir().map_err(failed)?;
    let mut terms = signal(SignalKind::terminate()).map_err(failed)?;
    let mut interrupts = signal(SignalKind::interrupt()).map_err(failed)?;
    let mut kilo = Acp::open(&asked.kilo).await.map_err(failed)?;
    let outcome = async {
        kilo.call(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await?;
        let cwd = cwd.to_string_lossy();
        let session = match (asked.act.as_str(), asked.id.as_deref()) {
            ("continue", Some(id)) => {
                let params = json!({"sessionId": id, "cwd": cwd, "mcpServers": []});
                kilo.call("session/resume", params).await?;
                id.to_owned()
            }
            (act, id) => {
                let (method, params) = match id {
                    Some(id) => (
                        "session/fork",
                        json!({"sessionId": id, "cwd": cwd, "mcpServers": []}),
                    ),
                    None => ("session/new", json!({"cwd": cwd, "mcpServers": []})),
                };
                let opened = kilo.call(method, params).await?;
                opened
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{}: Kilo's {act} named no session", refusal::REFUSED))?
            }
        };
        for (option, value) in [("model", &asked.model), ("effort", &asked.thinking)] {
            if let Some(value) = value {
                let params = json!({"sessionId": session, "configId": option, "value": value});
                kilo.call("session/set_config_option", params).await?;
            }
        }
        let mut prompts = HashSet::from([kilo.prompt(&session, &words).await?]);
        // Bound once the session is named: words heard before that would have no turn to
        // go into. A turn that cannot bind runs with no inbox.
        let heard = asked
            .inbox
            .as_ref()
            .and_then(|path| UnixListener::bind(path).ok());
        let mut cancelled: Option<tokio::time::Instant> = None;
        // The permissions that wait on the hook, by their request ids.
        let mut asking = JoinSet::new();
        let mut waiting: Vec<Value> = Vec::new();
        while !prompts.is_empty() {
            for request in std::mem::take(&mut kilo.asks) {
                let Some(id) = request.get("id").cloned() else {
                    continue;
                };
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                match (&asked.hook, cancelled) {
                    (Some(hook), None) => {
                        waiting.push(id.clone());
                        asking.spawn(ask(hook.clone(), id, params));
                    }
                    (None, None) => {
                        let rejection = json!({"outcome": rejected(params.get("options"))});
                        kilo.answer(&id, rejection).await?;
                    }
                    (_, Some(_)) => kilo.answer(&id, CANCELLED.clone()).await?,
                }
            }
            let deadline = async {
                match cancelled {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                message = kilo.next() => {
                    if let Some(id) = message?.as_ref().and_then(|m| m.get("id")).and_then(Value::as_u64) {
                        prompts.remove(&id);
                    }
                }
                Some(Ok((id, answer))) = asking.join_next() => {
                    waiting.retain(|waits| *waits != id);
                    kilo.answer(&id, answer).await?;
                }
                Ok((stream, _)) = accept(heard.as_ref()) => {
                    if let Some(id) = hear(&mut kilo, &session, stream).await {
                        prompts.insert(id);
                    }
                }
                _ = terms.recv() => match cancelled {
                    Some(_) => break,
                    None => {
                        cancelled = Some(kilo.cancel(&session).await?);
                        abandon(&mut kilo, &mut asking, &mut waiting).await?;
                    }
                },
                _ = interrupts.recv() => match cancelled {
                    Some(_) => break,
                    None => {
                        cancelled = Some(kilo.cancel(&session).await?);
                        abandon(&mut kilo, &mut asking, &mut waiting).await?;
                    }
                },
                () = deadline => break,
            }
        }
        // The inbox closes before Kilo does: words told from here on are told to a turn
        // that is ending.
        drop(heard);
        Ok(())
    }
    .await;
    if let Some(inbox) = &asked.inbox {
        let _ = std::fs::remove_file(inbox);
    }
    kilo.close().await;
    outcome
}

/// The answer to a permission asked in a turn that is cancelled.
static CANCELLED: std::sync::LazyLock<Value> =
    std::sync::LazyLock::new(|| json!({"outcome": {"outcome": "cancelled"}}));

/// End the hooks still running, and answer their permissions cancelled, as ACP asks of a
/// client once it cancels.
async fn abandon(
    kilo: &mut Acp,
    asking: &mut JoinSet<(Value, Value)>,
    waiting: &mut Vec<Value>,
) -> Result<(), String> {
    asking.abort_all();
    for id in waiting.drain(..) {
        kilo.answer(&id, CANCELLED.clone()).await?;
    }
    Ok(())
}

/// Run Steery's hook at a permission Kilo asks for, the request's `params` on its standard
/// input. Kilo's answer is the hook's standard output where it is an answer with an
/// `outcome`, else a rejection: a hook that fails, says nothing, or runs past
/// [`ASKS_WAITS`] lets no call run.
async fn ask(hook: String, id: Value, params: Value) -> (Value, Value) {
    let answered = async {
        let mut child = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("{hook} {EVENT}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        stdin.write_all(params.to_string().as_bytes()).await.ok()?;
        drop(stdin);
        let waits = Duration::from_secs(ASKS_WAITS.into());
        let output = tokio::time::timeout(waits, child.wait_with_output())
            .await
            .ok()?
            .ok()?;
        let answer: Value = serde_json::from_slice(&output.stdout).ok()?;
        let outcome = answer.get("outcome").is_some_and(Value::is_object);
        (output.status.success() && outcome).then_some(answer)
    };
    let answer = answered
        .await
        .unwrap_or_else(|| json!({"outcome": rejected(params.get("options"))}));
    (id, answer)
}

async fn accept(
    heard: Option<&UnixListener>,
) -> std::io::Result<(UnixStream, tokio::net::unix::SocketAddr)> {
    match heard {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

/// Words told on the inbox, prompted into the session, where Kilo queues them behind the
/// prompt it runs. The caller hears `heard`, or why they did not go in; the id is the new
/// prompt's.
async fn hear(kilo: &mut Acp, session: &str, mut stream: UnixStream) -> Option<u64> {
    let mut words = String::new();
    let read = tokio::time::timeout(WORDS_WITHIN, stream.read_to_string(&mut words)).await;
    let (prompt, said) = match read {
        Ok(Ok(_)) if !words.is_empty() => match kilo.prompt(session, &words).await {
            Ok(id) => (Some(id), HEARD.to_owned()),
            Err(line) => (None, line),
        },
        Ok(Ok(_)) => (None, "no words came".to_owned()),
        Ok(Err(e)) => (None, format!("the words did not read: {e}")),
        Err(_) => (None, "the words did not come in time".to_owned()),
    };
    let _ = stream.write_all(format!("{said}\n").as_bytes()).await;
    let _ = stream.shutdown().await;
    prompt
}

/// A running `kilo acp`: JSON-RPC 2.0, one message a line, on its standard input and output.
struct Acp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    next: u64,
    /// Kilo's permission requests that the turn has yet to answer, whole.
    asks: Vec<Value>,
}

impl Acp {
    /// Start `kilo acp` in a process group of its own, so a signal meant for the turn
    /// reaches Kilo only as the cancel this turn sends; its standard error is Kilo's log,
    /// dropped.
    async fn open(kilo: &std::path::Path) -> std::io::Result<Self> {
        let mut command = std::process::Command::new(kilo);
        command
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("kilo acp has no standard output"))?;
        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next: 0,
            asks: Vec::new(),
        })
    }

    /// Write one message to Kilo, and the same line to standard output.
    async fn send(&mut self, message: &Value) -> Result<(), String> {
        let line = self.write(message).await?;
        out(&line).await
    }

    /// Write one message to Kilo alone; the line written.
    async fn write(&mut self, message: &Value) -> Result<String, String> {
        let line = format!("{message}\n");
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| format!("{}: Kilo's input is closed", refusal::REFUSED))?;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("{}: Kilo did not take a message: {e}", refusal::REFUSED))?;
        Ok(line)
    }

    /// Send a request; its id.
    async fn request(&mut self, method: &str, params: Value) -> Result<u64, String> {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        Ok(id)
    }

    /// The next message from Kilo, written to standard output first; none where it is a
    /// permission Kilo asks for, which then waits in `asks` for the turn to answer. Any
    /// other request of Kilo's is answered here as a method this client does not have. No
    /// answer of this client's is written out, so every answer on standard output is Kilo's.
    async fn next(&mut self) -> Result<Option<Value>, String> {
        loop {
            let line = self
                .lines
                .next_line()
                .await
                .map_err(|e| format!("{}: Kilo's output did not read: {e}", refusal::REFUSED))?
                .ok_or_else(|| format!("{}: Kilo ended before it answered", refusal::REFUSED))?;
            if line.trim().is_empty() {
                continue;
            }
            out(&format!("{line}\n")).await?;
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let (Some(method), Some(id)) = (
                message.get("method").and_then(Value::as_str),
                message.get("id"),
            ) {
                if method == "session/request_permission" {
                    self.asks.push(message);
                    return Ok(None);
                }
                let reply = json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": format!("no {method} here")}});
                self.write(&reply).await?;
                continue;
            }
            return Ok(Some(message));
        }
    }

    /// Call `method` and wait for its answer; Kilo's refusal is the line for standard
    /// error, with the refusal word it means.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.request(method, params).await?;
        loop {
            let Some(message) = self.next().await? else {
                continue;
            };
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let said = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("refused");
                return Err(format!("{}: Kilo's {method}: {said}", word(error)));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Prompt the session with the words; the prompt's id. Kilo answers it when the turn
    /// those words began ends.
    async fn prompt(&mut self, session: &str, words: &str) -> Result<u64, String> {
        let params = json!({"sessionId": session, "prompt": [{"type": "text", "text": words}]});
        self.request("session/prompt", params).await
    }

    /// Ask Kilo to cancel the session's turn; when the turn ends without Kilo's answer.
    async fn cancel(&mut self, session: &str) -> Result<tokio::time::Instant, String> {
        self.send(&json!({"jsonrpc": "2.0", "method": "session/cancel",
            "params": {"sessionId": session}}))
            .await?;
        Ok(tokio::time::Instant::now() + CANCELLED_WITHIN)
    }

    /// Answer one of Kilo's requests.
    async fn answer(&mut self, id: &Value, result: Value) -> Result<(), String> {
        self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
            .map(drop)
    }

    /// Close Kilo's input, which ends it, and end whatever of it still runs after
    /// [`CLOSED_WITHIN`].
    async fn close(mut self) {
        drop(self.stdin.take());
        let running = self.child.id().map(|pid| {
            let mut running = kernel::descendants(pid);
            running.extend(kernel::started(pid).map(|start| (pid, start)));
            running
        });
        if tokio::time::timeout(CLOSED_WITHIN, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.start_kill();
        }
        if let Some(running) = running {
            kernel::signal(&running, libc::SIGKILL);
        }
    }
}

/// One line on standard output, whole.
async fn out(line: &str) -> Result<(), String> {
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(line.as_bytes())
        .await
        .and(stdout.flush().await)
        .map_err(|e| format!("{}: the turn's output is closed: {e}", refusal::REFUSED))
}

/// The answer that rejects a permission once: Kilo's `reject` option, else cancelled.
fn rejected(options: Option<&Value>) -> Value {
    options
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|option| option.get("kind").and_then(Value::as_str) == Some("reject_once"))
        .and_then(|option| option.get("optionId"))
        .map_or_else(
            || json!({"outcome": "cancelled"}),
            |id| json!({"outcome": "selected", "optionId": id}),
        )
}

/// The refusal word a Kilo error means, by what its data names (Kilo's ACP errors,
/// `acp/error.ts`): a session it does not have, a model, an effort or a config option it
/// does not know.
pub(super) fn word(error: &Value) -> &'static str {
    let data = error.get("data");
    let names = |key: &str| data.and_then(|data| data.get(key)).is_some();
    if names("sessionId") {
        refusal::NOT_FOUND
    } else if names("modelId") {
        refusal::MODEL_MISSING
    } else if names("effort") || names("configId") {
        refusal::SELECTION_UNSUPPORTED
    } else {
        refusal::REFUSED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_s_command_line_reads_back_as_it_was_asked() {
        let asked = Asked {
            kilo: "/opt/homebrew/bin/kilo".into(),
            act: "fork".into(),
            id: Some("ses_1".into()),
            model: Some("kilo/anthropic/claude-opus-4.5".into()),
            thinking: Some("high".into()),
            inbox: Some("/tmp/t/inbox".into()),
            hook: Some("'/opt/steery/bin/steery' hook kilo".into()),
        };
        assert_eq!(Asked::parse(asked.args().into_iter()), Ok(asked));
        let new = |args: &[&str]| Asked::parse(args.iter().map(|arg| (*arg).to_owned()));
        assert!(new(&["--kilo", "kilo", "new"]).is_ok());
        assert!(new(&["--kilo", "kilo", "new", "ses_1"]).is_err());
        assert!(new(&["--kilo", "kilo", "continue"]).is_err());
        assert!(new(&["--kilo", "kilo", "hand", "ses_1"]).is_err());
    }

    #[test]
    fn kilo_s_errors_mean_their_refusal_words() {
        let error = |data: Value| json!({"code": -32602, "message": "x", "data": data});
        assert_eq!(
            word(&error(json!({"sessionId": "ses_x"}))),
            refusal::NOT_FOUND
        );
        assert_eq!(
            word(&error(json!({"providerId": "kilo", "modelId": "nope"}))),
            refusal::MODEL_MISSING
        );
        assert_eq!(
            word(&error(json!({"effort": "max"}))),
            refusal::SELECTION_UNSUPPORTED
        );
        assert_eq!(
            word(&json!({"code": -32000, "message": "auth"})),
            refusal::REFUSED
        );
    }

    #[test]
    fn a_permission_is_rejected_once() {
        let options = json!([
            {"optionId": "once", "kind": "allow_once"},
            {"optionId": "reject", "kind": "reject_once"},
        ]);
        assert_eq!(
            rejected(Some(&options)),
            json!({"outcome": "selected", "optionId": "reject"})
        );
        assert_eq!(rejected(None), json!({"outcome": "cancelled"}));
    }
}
