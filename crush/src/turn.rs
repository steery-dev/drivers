//! One turn of Crush, run by this driver's own binary in its turn mode, and its lines.
//!
//! `steery-driver-crush turn <home> <crush> run -q -v …` runs `crush run -q -v …` with this
//! process's standard input, the words. `crush run` prints only the answer's text, so the
//! mode drops it and reads Crush's log on standard error (`-v`): Crush names the session it
//! created or continues there (`app.go`, `RunNonInteractive`), and the mode writes
//! `{"named": <id>}` to standard output at once. Once Crush exits, the mode reads how the
//! turn ended from the project's store and writes `{"ended": …}`. What Crush refused, in
//! its own error box, goes to standard error as `{"refused": <word>}`, and the turn ends
//! `failed` with Crush's words.
//!
//! Crush cancels a running turn on SIGINT within a second or two and writes it `canceled`;
//! on SIGTERM it ends only once the answer is written (Crush 0.100.0, `cmd/run.go`). So a
//! signal that reaches the mode reaches Crush as SIGINT.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use serde::{Deserialize, Serialize};
use steery_sdk::wire::harness::{Ended, Read, TurnState};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;
use tokio::signal::unix::{SignalKind, signal};

use crate::drive::{self, failure, last, refusal_of};
use crate::store::Store;

/// The word that starts the turn mode, as `start`'s plan runs this binary.
pub const MODE: &str = "turn";

/// The bytes of a session's end that the mode reads for the turn's end.
const TAIL: u64 = 256 * 1024;

/// The lines of Crush's log the mode keeps for its error box.
const KEPT: usize = 64;

/// What Crush logs as it names the session of a run.
const NAMES: [&str; 2] = [
    "Created session for non-interactive run",
    "Continuing session for non-interactive run",
];

/// One line of the turn mode.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<Ended>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
}

fn say(line: &Line) {
    println!("{}", serde_json::to_string(line).unwrap_or_default());
}

/// The session a line of Crush's log names, where it names one.
fn named(line: &str) -> Option<&str> {
    if !NAMES.iter().any(|names| line.contains(names)) {
        return None;
    }
    let id = line.split("session_id=").nth(1)?;
    let id = id.split_whitespace().next()?.trim_matches('"');
    (!id.is_empty()).then_some(id)
}

/// The turn mode, `args` after its own word.
pub async fn run(mut args: impl Iterator<Item = String>) -> ExitCode {
    let (Some(home), Some(binary)) = (args.next(), args.next()) else {
        eprintln!("the turn names no home and no crush executable");
        return ExitCode::from(2);
    };
    let (home, binary) = (PathBuf::from(home), PathBuf::from(binary));
    match turn(&home, &binary, args.collect()).await {
        Ok(code) => code,
        Err(why) => {
            say(&Line {
                ended: Some(Ended {
                    state: TurnState::Failed,
                    error: Some(why),
                }),
                ..Line::default()
            });
            ExitCode::FAILURE
        }
    }
}

async fn turn(home: &Path, binary: &Path, args: Vec<String>) -> Result<ExitCode, String> {
    // Crush stamps its messages in whole seconds.
    let began = drive::now() / 1000 * 1000;
    let mut crush = Command::new(binary);
    // Steery's hook trusts `STEERY_STARTED` only from Crush's parent, which is this turn: it
    // passes on what the runtime started it as, under its own pid.
    match started() {
        Some(what) => crush.env("STEERY_STARTED", format!("{}:{what}", std::process::id())),
        None => crush.env_remove("STEERY_STARTED"),
    };
    let mut crush = crush
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("crush did not start: {e}"))?;
    let pid = crush.id().ok_or("crush ended as it started")?;
    let start = steery_driver_common::kernel::started(pid);
    let mut log = BufReader::new(crush.stderr.take().ok_or("crush has no standard error")?).lines();
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    let mut terminate = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut session: Option<String> = None;
    let mut kept: Vec<String> = Vec::new();
    let mut open = true;
    let status = loop {
        tokio::select! {
            line = log.next_line(), if open => match line {
                Ok(Some(line)) => {
                    if session.is_none()
                        && let Some(id) = named(&line)
                    {
                        session = Some(id.to_owned());
                        say(&Line { named: session.clone(), ..Line::default() });
                    }
                    kept.push(line);
                    if kept.len() > KEPT {
                        kept.remove(0);
                    }
                }
                _ => open = false,
            },
            _ = interrupt.recv() => cancel(pid, start),
            _ = terminate.recv() => cancel(pid, start),
            status = crush.wait() => break status.map_err(|e| e.to_string())?,
        }
    };
    // What Crush wrote after the last read, before it exited.
    while let Ok(Some(line)) = log.next_line().await {
        kept.push(line);
    }
    if !status.success()
        && let Some(message) = failure(kept.iter().map(String::as_str))
    {
        if let Some(word) = refusal_of(&message) {
            eprintln!(
                "{}",
                serde_json::to_string(&Line {
                    refused: Some(word.into()),
                    ..Line::default()
                })
                .unwrap_or_default()
            );
        }
        say(&Line {
            ended: Some(Ended {
                state: TurnState::Failed,
                error: Some(message),
            }),
            ..Line::default()
        });
        return Ok(ExitCode::FAILURE);
    }
    let Some(id) = session else {
        return Err(format!(
            "crush run exited with {status} and named no session"
        ));
    };
    let events = Store::new(home)
        .read(
            Read {
                external_id: id.clone(),
                sub_agent: None,
                cursor: None,
                tail: Some(TAIL),
            },
            |_| Vec::new(),
        )
        .map_err(|e| format!("cannot read session {id}: {e}"))?
        .events;
    let ended = match last(&events) {
        Some((Some(at), _, ended)) if at >= began => ended,
        // No answer of this run ended: Crush stopped before it wrote one.
        _ => Ended {
            state: if status.success() {
                TurnState::Failed
            } else {
                TurnState::Interrupted
            },
            error: status
                .success()
                .then(|| "Crush wrote no answer to the words".to_owned()),
        },
    };
    say(&Line {
        ended: Some(ended),
        ..Line::default()
    });
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// What the runtime started this turn as (`STEERY_STARTED=<pid>:<what>`), where the pid is
/// this process's parent, as PROTOCOL.md §3.3 trusts it.
fn started() -> Option<String> {
    let value = std::env::var("STEERY_STARTED").ok()?;
    let (pid, what) = value.split_once(':')?;
    let pid: u32 = pid.parse().ok()?;
    (pid > 1 && pid == std::os::unix::process::parent_id()).then(|| what.to_owned())
}

/// Cancel Crush's turn as Ctrl-C does.
fn cancel(pid: u32, start: Option<u64>) {
    if let Some(start) = start {
        steery_driver_common::kernel::signal(&[(pid, start)], libc::SIGINT);
    }
}
