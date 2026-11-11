//! A turn the runtime runs. `start` plans this driver's own binary in its turn mode
//! ([`main`]), which names the conversation and then becomes `copilot --output-format json`,
//! the words on standard input. `turn/read` reads Copilot's lines: the events of
//! `events.jsonl` under the same ids, the ephemeral ones Copilot keeps off disk, and a last
//! `result`.
//!
//! Copilot's output names a new conversation in its first line, `session.start`, but a
//! resumed one nowhere until its last (`session.resume` has no id). So the turn mode writes
//! one line of its own first for a continue or a fork. A fork goes through Copilot's own
//! server, `sessions.fork`, which copies the conversation's history and runs no model turn;
//! the turn then resumes the copy.
//!
//! The turn runs without `--allow-all-tools`. Copilot in a run without a terminal denies every
//! call that its own rules and Steery's `permissionRequest` hook do not allow.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    Act, Ended, Event, Launch, Start, Stdin, Stream, TurnLine, TurnRead, TurnState,
};
use steery_sdk::{Error, refusal};

use super::events::{self, Carried, End, Starts, Turn, text};
use super::{home, server, transcripts::Store};

/// The first argument of this driver's turn mode.
pub const MODE: &str = "turn";

/// The key of the turn mode's own line, `{"steery": {"named": <id>}}`.
const OWN: &str = "steery";

/// Copilot's levels of `--reasoning-effort`, as its command line lists them (1.0.108).
const EFFORT: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// A thinking pick, where it is one of Copilot's efforts.
pub fn effort(thinking: Option<&str>) -> Result<(), Error> {
    match thinking {
        Some(thinking) if !EFFORT.contains(&thinking) => Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("Copilot has no reasoning effort {thinking}"),
        )),
        _ => Ok(()),
    }
}

/// `start`: the plan of a turn of this driver's own turn mode. A continue or a fork names a
/// conversation Copilot keeps here; a continue runs in the folder it ran in; a thinking pick
/// is one of Copilot's efforts. Copilot itself checks the model, on standard error.
pub fn start(store: &Store, start: Start) -> Result<Launch, Error> {
    let copilot = home::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "copilot is not installed here"))?;
    if !start.cwd.is_absolute() {
        return Err(Error::invalid("the folder is not an absolute path"));
    }
    if start.worktree {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Steery does not start Copilot in a worktree of its own",
        ));
    }
    let act = match start.act {
        Act::New => "new",
        Act::Continue => "continue",
        Act::Fork => "fork",
        Act::Hand => {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Copilot does not run a hand-over as a turn",
            ));
        }
    };
    effort(start.thinking.as_deref())?;
    let program = std::env::current_exe().map_err(|e| {
        Error::refused(
            refusal::START_FAILED,
            format!("cannot find this driver's own path: {e}"),
        )
    })?;
    let mut argv = vec![text_of(program)?, MODE.into(), act.into()];
    if start.act != Act::New {
        let id = start
            .external_id
            .ok_or_else(|| Error::invalid(format!("a {act} must name its conversation")))?;
        store.locate(steery_sdk::wire::harness::Locate {
            external_id: id.clone(),
        })?;
        if start.act == Act::Continue && !start.cwd.is_dir() {
            return Err(Error::refused(
                refusal::NO_PLACE,
                format!(
                    "{} is gone, and Copilot resumes the conversation there",
                    start.cwd.display()
                ),
            ));
        }
        argv.push(id);
    }
    argv.extend([
        "--".into(),
        text_of(copilot)?,
        "--output-format".into(),
        "json".into(),
        "--no-ask-user".into(),
    ]);
    if let Some(model) = start.model {
        argv.extend(["--model".into(), model]);
    }
    if let Some(thinking) = start.thinking {
        argv.extend(["--reasoning-effort".into(), thinking]);
    }
    for read in start.reads {
        argv.extend(["--add-dir".into(), text_of(read)?]);
    }
    Ok(Launch {
        argv,
        env: BTreeMap::new(),
        cwd: start.cwd,
        stdin: Stdin::Words,
        inbox: false,
    })
}

fn text_of(path: PathBuf) -> Result<String, Error> {
    path.into_os_string()
        .into_string()
        .map_err(|path| Error::invalid(format!("{} is not text", path.display())))
}

/// The turn mode: `turn new -- <copilot...>`, `turn continue <id> -- <copilot...>` or
/// `turn fork <id> -- <copilot...>`. It writes its own line that names the conversation,
/// where Copilot's output would not name it first, and then becomes Copilot, which reads
/// the words on the standard input it inherits.
pub async fn main(mut args: impl Iterator<Item = String>) -> std::io::Result<()> {
    let usage = || std::io::Error::other("usage: turn new|continue <id>|fork <id> -- <copilot...>");
    let act = args.next().ok_or_else(usage)?;
    let id = match act.as_str() {
        "new" => None,
        "continue" | "fork" => Some(args.next().ok_or_else(usage)?),
        _ => return Err(usage()),
    };
    if args.next().as_deref() != Some("--") {
        return Err(usage());
    }
    let copilot = PathBuf::from(args.next().ok_or_else(usage)?);
    let mut command = std::process::Command::new(&copilot);
    command.args(args);
    if let Some(mut id) = id {
        if act == "fork" {
            id = server::fork(&copilot, &id)
                .await
                .map_err(|why| std::io::Error::other(format!("{FORK_FAILED} {id}: {why}")))?;
        }
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{}", json!({OWN: {"named": id}}))?;
        stdout.flush()?;
        command.arg(format!("--resume={id}"));
    }
    Err(command.exec())
}

/// How the turn mode begins the line it writes where Copilot did not fork.
const FORK_FAILED: &str = "Copilot did not fork";

/// Each turn's state between its lines, by the runtime's name of the turn.
#[derive(Default)]
pub struct Turns {
    turns: Mutex<HashMap<String, Run>>,
}

/// One turn as its lines so far left it.
struct Run {
    named: Option<String>,
    starts: Starts,
    carried: Carried,
    /// Records whose end a later record decides.
    held: Vec<Option<Value>>,
    ended: bool,
}

impl Turns {
    /// `turn/read`.
    pub fn read(&self, store: &Store, read: TurnRead) -> Result<TurnLine, Error> {
        if read.stream == Stream::Stderr {
            return Ok(TurnLine {
                refused: refusal_of(&read.line).map(str::to_owned),
                ..TurnLine::default()
            });
        }
        let Ok(record) = serde_json::from_str::<Value>(&read.line) else {
            return Ok(TurnLine::default());
        };
        let mut turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
        let run = turns.entry(read.turn.clone()).or_insert_with(|| Run {
            named: None,
            starts: Starts::default(),
            carried: Carried {
                turn: Turn::Closed,
                id: None,
            },
            held: Vec::new(),
            ended: false,
        });
        let mut line = TurnLine::default();
        if run.ended {
            return Ok(line);
        }
        let data = record.get("data").unwrap_or(&Value::Null);
        let kind = text(&record, "type").unwrap_or_default();
        let named = match kind {
            "session.start" => text(data, "sessionId"),
            "result" => text(&record, "sessionId"),
            _ => record
                .pointer(&format!("/{OWN}/named"))
                .and_then(Value::as_str),
        };
        if let Some(named) = named
            && run.named.as_deref() != Some(named)
        {
            run.named = Some(named.to_owned());
            run.starts = store.starts(named);
            line.named = Some(named.to_owned());
            line.took = Some(false);
        }
        match kind {
            "user.message" => {
                if let Some(delivery) = &read.delivery
                    && text(data, "content").is_some_and(|said| said.contains(delivery.as_str()))
                {
                    line.took = Some(true);
                }
            }
            "session.title_changed" => line.title = text(data, "title").map(str::to_owned),
            _ => {}
        }
        let last = kind == "result";
        if run.named.is_some() && !last {
            run.held.push(Some(record.clone()));
        }
        let said = events::read(
            None,
            &run.starts,
            &run.held,
            run.carried.clone(),
            if last { End::Decided } else { End::Open },
        );
        run.held.drain(..said.consumed);
        run.carried = said.carried;
        for event in &said.events {
            if let Event::TurnEnded { state, failure, .. } = event {
                line.ended = Some(Ended {
                    state: *state,
                    error: failure.as_ref().map(|failure| failure.line.clone()),
                });
            }
        }
        line.events = said.events;
        if last {
            let code = record.get("exitCode").and_then(Value::as_i64);
            if line.ended.is_none() && code != Some(0) {
                line.ended = Some(Ended {
                    state: TurnState::Failed,
                    error: code.map(|code| format!("Copilot exited with code {code}")),
                });
            }
            run.ended = true;
        }
        Ok(line)
    }
}

/// The refusal a line of Copilot's standard error means, as Copilot words it (1.0.108).
fn refusal_of(line: &str) -> Option<&'static str> {
    if line.starts_with("Error: Model ") && line.ends_with("is not available.") {
        Some(refusal::MODEL_MISSING)
    } else if line.starts_with("Error: No session, task, or name matched") {
        Some(refusal::NOT_FOUND)
    } else if line.starts_with("error: invalid value") && line.contains("--reasoning-effort") {
        Some(refusal::SELECTION_UNSUPPORTED)
    } else if line.contains(FORK_FAILED) {
        Some(refusal::START_FAILED)
    } else {
        None
    }
}
