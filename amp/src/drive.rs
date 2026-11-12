//! Amp's own way in: a turn is `amp -x --stream-json`, run by the runtime, the words on
//! standard input. Amp keeps its threads on ampcode.com, so what the driver knows of a thread
//! it reads through `amp threads export`, and who holds it the runtime looks for itself.
//!
//! Amp picks no model: it runs a mode (`-m`), which picks the model, its effort, the system
//! prompt and the tools. A thread keeps the mode of its first message for good
//! (<https://ampcode.com/docs/the-dial>), so `models` lists Amp's modes, and a continue
//! takes no other.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use steery_sdk::wire::harness::{
    Act, Event, Execution, Held, HeldBy, Holder, HolderStop, Launch, Look, Looked, Marked,
    Model, Models, Reader, ReaderPlan, Receipt, ReceiptFind, ReceiptMark, Sample, Sampled,
    Start, Stdin, Stop, Stopped, Terminal, TerminalCommand, TurnState,
};
use steery_sdk::{Error, refusal};

use crate::{events, home, sign_in, threads};

/// Amp's built-in modes, its default first, as `-m` spells them (Amp 0.0.1794225648's
/// `amp --help`; <https://ampcode.com/docs/the-dial>: "`medium` is the default for most
/// work"). A plain `amp -x` here ran `medium`. `claude-code` is left out: it needs a linked
/// Claude subscription. Plugin modes are not listed: Amp names them nowhere a driver reads.
const MODES: [(&str, &str); 4] = [
    ("medium", "Medium"),
    ("low", "Low"),
    ("high", "High"),
    ("ultra", "Ultra"),
];

/// Amp's site, where each thread has its page.
const THREADS: &str = "https://ampcode.com/threads/";

/// `models`: Amp's modes, and whether Amp is signed in.
pub async fn models(home: &Path) -> Result<Models, Error> {
    let binary = home::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "Amp is not installed here"))?;
    Ok(Models {
        models: MODES
            .iter()
            .map(|(mode, label)| Model {
                provider: None,
                model: (*mode).into(),
                label: Some(format!("Amp {label}")),
                thinking: Vec::new(),
                cost: None,
            })
            .collect(),
        unlisted: None,
        watch: vec![home::settings(home), home::accounts(home)],
        signed_in: sign_in::signed_in(&binary).await,
    })
}

/// `start`: the plan of a turn that the runtime runs as Amp itself, the words on standard
/// input. A new thread runs in the mode asked, else Amp's own, and stays unarchived, as
/// `amp -x` archives the thread it made otherwise. A continue names a thread that `amp
/// threads export` gives, and runs in the thread's own mode.
pub async fn start(start: Start) -> Result<Launch, Error> {
    let binary = home::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "Amp is not installed here"))?;
    if start.worktree {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Steery does not start Amp in a worktree of its own",
        ));
    }
    if let Some(thinking) = &start.thinking {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("Amp takes no thinking level ({thinking}): its mode sets the effort"),
        ));
    }
    if let Some(model) = &start.model
        && !MODES.iter().any(|(mode, _)| mode == model)
    {
        return Err(Error::refused(
            refusal::MODEL_MISSING,
            format!("Amp has no mode {model}"),
        ));
    }
    let amp = text(&binary)?;
    let argv = match (start.act, start.external_id.as_deref()) {
        (Act::New, _) => {
            let mut argv = vec![
                amp,
                "-x".into(),
                "--stream-json".into(),
                "--no-archive-after-execute".into(),
            ];
            if let Some(model) = start.model {
                argv.extend(["-m".into(), model]);
            }
            argv
        }
        (Act::Continue, Some(id)) => {
            let thread = threads::export(&binary, id).await?;
            if let Some(model) = &start.model
                && mode(&thread) != Some(model.as_str())
            {
                return Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    format!(
                        "{id} runs in Amp's {} mode, and a thread keeps the mode it began in",
                        mode(&thread).unwrap_or("own")
                    ),
                ));
            }
            vec![
                amp,
                "threads".into(),
                "continue".into(),
                id.to_owned(),
                "-x".into(),
                "--stream-json".into(),
            ]
        }
        (Act::Continue, None) => {
            return Err(Error::invalid("a continue must name its thread"));
        }
        (Act::Fork | Act::Hand, _) => {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Amp has no fork or hand over that a turn runs",
            ));
        }
    };
    Ok(Launch {
        argv,
        env: BTreeMap::new(),
        cwd: start.cwd,
        stdin: Stdin::Words,
        inbox: false,
    })
}

/// The mode a thread runs in, as Amp names it on the thread.
fn mode(thread: &Value) -> Option<&str> {
    thread.get("agentMode").and_then(Value::as_str)
}

fn text(path: &Path) -> Result<String, Error> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::invalid(format!("{} is not text", path.display())))
}

/// `held`: where the runtime looks. Amp keeps no file of a thread here and no registry of
/// who has one open, so every `amp` process that nothing of Steery's answers for may hold it.
pub fn held(_: Held) -> HeldBy {
    HeldBy::Look(Looked {
        look: Look {
            transcripts: Vec::new(),
            strays: true,
            beyond: Vec::new(),
        },
    })
}

/// `idle`: the last turn of the thread, where it ended; nothing where it did not, or where
/// Amp does not give the thread.
pub async fn idle(held: Held) -> Sampled {
    let Some(binary) = home::binary() else {
        return Sampled { sample: None };
    };
    let sample = threads::export(&binary, &held.external_id)
        .await
        .ok()
        .and_then(|thread| sample(&events::fold(&thread, None, None).events));
    Sampled { sample }
}

/// The sample of the last turn in `events`, where it ended.
fn sample(events: &[Event]) -> Option<Sample> {
    let mut model = None;
    let mut started_at = None;
    let mut ended = None;
    for event in events {
        match event {
            Event::Model { model: named, .. } => model = Some(named.clone()),
            Event::TurnBegan { at, .. } => {
                started_at = Some(*at);
                ended = None;
            }
            Event::TurnEnded {
                at, state, failure, ..
            } => ended = Some((*at, *state, failure.as_ref().map(|f| f.line.clone()))),
            _ => {}
        }
    }
    let (ended_at, state, error) = ended?;
    Some(Sample {
        state: match state {
            TurnState::Finished => Execution::Finished,
            TurnState::Failed => Execution::Failed,
            TurnState::Interrupted => Execution::Interrupted,
        },
        observed_at: now(),
        started_at,
        ended_at: Some(ended_at),
        model,
        thinking: None,
        error,
        vacant: false,
        if_unheld: true,
    })
}

/// Milliseconds since 1970.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `stop`: Amp has no graceful stop of its own for an `amp -x` run; the runtime's signals
/// end it. A stop of Steery's own words cannot tell its turn.
pub fn stop(stop: Stop) -> Result<Stopped, Error> {
    if stop.say.is_some() {
        return Err(Error::refused(
            refusal::OTHER_TURN,
            "Amp cannot tell which words began the turn that runs",
        ));
    }
    Ok(Stopped {
        stopped: false,
        proof: Some(
            "An `amp -x` turn has no graceful stop of its own; the runtime's signals end it."
                .into(),
        ),
    })
}

/// `receipt/mark`: no door, so no point to look after.
pub fn mark(_: ReceiptMark) -> Result<Marked, Error> {
    Err(Error::refused(
        refusal::SELECTION_UNSUPPORTED,
        "Amp has no door whose words need a receipt",
    ))
}

/// `receipt/find`: never asked, as `mark` gives no mark.
pub fn find(_: ReceiptFind) -> Result<Receipt, Error> {
    Err(Error::refused(
        refusal::REFUSED,
        "Amp gives no mark to look after",
    ))
}

/// `reader`: refused. `amp -x` saves every run as a thread on ampcode.com and has no flag
/// that turns its tools off, so it cannot be a sealed one-shot reader.
pub fn reader(_: Reader) -> Result<ReaderPlan, Error> {
    Err(Error::refused(
        refusal::SELECTION_UNSUPPORTED,
        "Amp saves every run as a thread and cannot run without its tools",
    ))
}

/// `terminal`: `cd '<cwd>' && amp threads continue <id>`, and the thread's page on
/// ampcode.com as its app.
pub async fn terminal(terminal: Terminal) -> Result<TerminalCommand, Error> {
    let id = &terminal.external_id;
    let binary = home::binary().ok_or_else(|| {
        Error::refused(
            refusal::NOT_FOUND,
            "Amp is not installed here, so no thread is",
        )
    })?;
    threads::export(&binary, id).await?;
    if !plain_id(id) {
        return Err(Error::refused(
            refusal::NOT_FOUND,
            format!("Amp has no thread `{id}`"),
        ));
    }
    if !terminal.cwd.is_absolute() {
        return Err(Error::invalid("the folder is not an absolute path"));
    }
    let line = match &terminal.holder {
        Some(Holder::Person {
            stop: HolderStop::Process { pid, .. },
            ..
        }) => Some(format!(
            "Amp may have it open already (pid {pid}); Amp refuses a second one that runs it."
        )),
        Some(Holder::Person {
            stop: HolderStop::Beyond(how),
            ..
        }) => Some(how.clone()),
        _ => None,
    };
    Ok(TerminalCommand {
        command: format!(
            "cd {} && amp threads continue {id}",
            quoted(&text(&terminal.cwd)?)
        ),
        app: Some(format!("{THREADS}{id}")),
        line,
    })
}

/// Whether an id is one plain word that a command takes as it is, and not a flag.
fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}
