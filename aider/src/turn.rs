//! One turn of Aider, run by this driver's own binary in its turn mode, and its lines.
//!
//! `steery-driver-aider turn <aider> …` runs `aider …` with this process's standard input,
//! the words. The plan names the history file (`--chat-history-file`), and the file is the
//! conversation, so the mode writes `{"named": <id>}` to standard output at once. Aider
//! prints its screen to standard output, so the mode keeps only its last lines, for an
//! error. Once Aider exits, the mode reads how the turn ended from the history file and
//! writes `{"ended": …}`. Where Aider's last line names a model it cannot reach, the mode
//! writes `{"refused": "model-missing"}` to standard error.
//!
//! Aider ends a model call on Ctrl-C and logs `^C again to exit`, and with `--message-file`
//! it then exits (`base_coder.py`, `keyboard_interrupt`). So a signal that reaches the mode
//! reaches Aider as SIGINT.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use serde::{Deserialize, Serialize};
use steery_sdk::wire::harness::{Ended, TurnState};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;
use tokio::signal::unix::{SignalKind, signal};

use crate::drive::{last, refusal_of};
use crate::{history, home};

/// The word that starts the turn mode, as `start`'s plan runs this binary.
pub const MODE: &str = "turn";

/// The lines of Aider's screen the mode keeps for an error.
const KEPT: usize = 16;

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

fn failed(error: String) -> Line {
    Line {
        ended: Some(Ended {
            state: TurnState::Failed,
            error: Some(error),
        }),
        ..Line::default()
    }
}

/// The turn mode, `args` after its own word.
pub async fn run(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(binary) = args.next() else {
        eprintln!("the turn names no aider executable");
        return ExitCode::from(2);
    };
    let args: Vec<String> = args.collect();
    let Some(file) = args
        .iter()
        .position(|arg| arg == "--chat-history-file")
        .and_then(|at| args.get(at + 1))
        .map(PathBuf::from)
    else {
        eprintln!("the turn names no chat history file");
        return ExitCode::from(2);
    };
    say(&Line {
        named: Some(home::id(&file)),
        ..Line::default()
    });
    match turn(Path::new(&binary), &file, &args).await {
        Ok(code) => code,
        Err(why) => {
            say(&failed(why));
            ExitCode::FAILURE
        }
    }
}

async fn turn(binary: &Path, file: &Path, args: &[String]) -> Result<ExitCode, String> {
    let began = history::length(file).unwrap_or(0);
    let mut aider = Command::new(binary)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("aider did not start: {e}"))?;
    let pid = aider.id().ok_or("aider ended as it started")?;
    let start = steery_driver_common::kernel::started(pid);
    let mut out =
        BufReader::new(aider.stdout.take().ok_or("aider has no standard output")?).lines();
    let mut err = BufReader::new(aider.stderr.take().ok_or("aider has no standard error")?).lines();
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    let mut terminate = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut kept: Vec<String> = Vec::new();
    let mut keep = |line: String| {
        if !line.trim().is_empty() {
            kept.push(line.trim().to_owned());
            if kept.len() > KEPT {
                kept.remove(0);
            }
        }
    };
    let (mut out_open, mut err_open) = (true, true);
    let status = loop {
        tokio::select! {
            line = out.next_line(), if out_open => match line {
                Ok(Some(line)) => keep(line),
                _ => out_open = false,
            },
            line = err.next_line(), if err_open => match line {
                Ok(Some(line)) => keep(line),
                _ => err_open = false,
            },
            _ = interrupt.recv() => cancel(pid, start),
            _ = terminate.recv() => cancel(pid, start),
            status = aider.wait() => break status.map_err(|e| e.to_string())?,
        }
    };
    while let Ok(Some(line)) = out.next_line().await {
        keep(line);
    }
    while let Ok(Some(line)) = err.next_line().await {
        keep(line);
    }
    let events = match history::read(file, None, None, true) {
        Ok(read) => read.events,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", file.display())),
    };
    let ours = events
        .iter()
        .any(|event| matches!(event, steery_sdk::wire::harness::Event::TurnBegan { turn: Some(turn), .. } if turn.parse::<u64>().is_ok_and(|at| at >= began)));
    let ended = match last(&events) {
        Some((_, _, state, error)) if ours => Ended { state, error },
        // Aider stopped before it wrote the words: its last line says why.
        _ => Ended {
            state: if status.code().is_none() {
                TurnState::Interrupted
            } else {
                TurnState::Failed
            },
            error: Some(kept.last().cloned().unwrap_or_else(|| {
                format!(
                    "aider exited with {status} and wrote no turn to {}",
                    file.display()
                )
            })),
        },
    };
    if ended.state == TurnState::Failed
        && let Some(word) = ended.error.as_deref().and_then(refusal_of)
    {
        eprintln!(
            "{}",
            serde_json::to_string(&Line {
                refused: Some(word.into()),
                ..Line::default()
            })
            .unwrap_or_default()
        );
    }
    let code = if ended.state == TurnState::Finished {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    };
    say(&Line {
        ended: Some(ended),
        ..Line::default()
    });
    Ok(code)
}

/// End Aider's model call as Ctrl-C does.
fn cancel(pid: u32, start: Option<u64>) {
    if let Some(start) = start {
        steery_driver_common::kernel::signal(&[(pid, start)], libc::SIGINT);
    }
}
