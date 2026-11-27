//! One turn of the canvas, run by this driver's own binary in its turn mode.
//!
//! `steery-driver-canvas turn <canvas> turn …` runs `canvas turn …` with this process's
//! standard input, the words, and passes each line it prints on at once: its `named` first,
//! its `line`s and `tokens`, and its `ended` last. A canvas that exits with no `ended` (it
//! crashed, or was killed) gets one here: `interrupted` where a signal ended it, else
//! `failed` with its last line of standard error.
//!
//! A signal that reaches the mode reaches the canvas as SIGINT: the canvas then ends the
//! turn as `interrupted`. Its launcher `exec`s Bun, so the canvas's pid is Bun's.

use std::path::Path;
use std::process::{ExitCode, Stdio};

use steery_sdk::wire::harness::{Ended, TurnState};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;
use tokio::signal::unix::{SignalKind, signal};

use crate::drive::Said;

/// The word that starts the turn mode, as `start`'s plan runs this binary.
pub const MODE: &str = "turn";

/// The end of a turn, as the canvas writes it: a failure only where it failed.
fn say(ended: &Ended) {
    let failure = (ended.state == TurnState::Failed)
        .then(|| serde_json::json!({"kind": "other", "line": ended.error}));
    println!(
        "{}",
        serde_json::json!({ "ended": { "state": ended.state, "failure": failure } })
    );
}

/// The turn mode, `args` after its own word.
pub async fn run(mut args: impl Iterator<Item = String>) -> ExitCode {
    let Some(binary) = args.next() else {
        eprintln!("the turn names no canvas executable");
        return ExitCode::from(2);
    };
    let args: Vec<String> = args.collect();
    let ended = turn(Path::new(&binary), &args)
        .await
        .unwrap_or_else(|error| {
            let ended = Ended {
                state: TurnState::Failed,
                error: Some(error),
            };
            say(&ended);
            ended
        });
    match ended.state {
        TurnState::Finished => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

/// Run the canvas, pass its lines on, and answer how the turn ended.
async fn turn(binary: &Path, args: &[String]) -> Result<Ended, String> {
    let mut canvas = Command::new(binary)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("canvas did not start: {e}"))?;
    let pid = canvas.id().ok_or("canvas ended as it started")?;
    let start = steery_driver_common::kernel::started(pid);
    let mut out =
        BufReader::new(canvas.stdout.take().ok_or("canvas has no standard output")?).lines();
    let mut err =
        BufReader::new(canvas.stderr.take().ok_or("canvas has no standard error")?).lines();
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    let mut terminate = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut ended: Option<Ended> = None;
    let mut last_error: Option<String> = None;
    let pass_out = |line: String, ended: &mut Option<Ended>| {
        println!("{line}");
        if let Ok(Said {
            ended: Some(ending),
            ..
        }) = serde_json::from_str::<Said>(&line)
        {
            *ended = Some(ending.ended());
        }
    };
    let mut pass_err = |line: String| {
        eprintln!("{line}");
        if !line.trim().is_empty() {
            last_error = Some(line.trim().to_owned());
        }
    };
    let (mut out_open, mut err_open) = (true, true);
    let status = loop {
        tokio::select! {
            line = out.next_line(), if out_open => match line {
                Ok(Some(line)) => pass_out(line, &mut ended),
                _ => out_open = false,
            },
            line = err.next_line(), if err_open => match line {
                Ok(Some(line)) => pass_err(line),
                _ => err_open = false,
            },
            _ = interrupt.recv() => cancel(pid, start),
            _ = terminate.recv() => cancel(pid, start),
            status = canvas.wait() => break status.map_err(|e| e.to_string())?,
        }
    };
    while let Ok(Some(line)) = out.next_line().await {
        pass_out(line, &mut ended);
    }
    while let Ok(Some(line)) = err.next_line().await {
        pass_err(line);
    }
    if let Some(ended) = ended {
        return Ok(ended);
    }
    let ended = Ended {
        state: match status.code() {
            None => TurnState::Interrupted,
            Some(_) => TurnState::Failed,
        },
        error: Some(
            last_error.unwrap_or_else(|| format!("canvas exited with {status} and wrote no end")),
        ),
    };
    say(&ended);
    Ok(ended)
}

/// End the canvas's turn as Ctrl-C does.
fn cancel(pid: u32, start: Option<u64>) {
    if let Some(start) = start {
        steery_driver_common::kernel::signal(&[(pid, start)], libc::SIGINT);
    }
}
