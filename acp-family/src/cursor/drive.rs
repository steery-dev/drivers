//! Drive: Steery starts Cursor's turns as `cursor-agent -p --output-format stream-json`,
//! the words on standard input, and reads its lines. Its first line, `system` `init`, names
//! the chat, a new one or the one it resumes, so the plan runs `cursor-agent` itself. Its
//! last line, `result`, says how the turn ended. A turn that Cursor refuses writes one line
//! of its own words instead, on standard error, and exits 1.
//!
//! The turn runs without `--force`. Steery's `preToolUse` hook answers each call that asks.
//!
//! The other methods read Cursor's stores: who holds a chat, how its last turn ended, the
//! receipt of words, and the command that opens it in a terminal.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use steery_driver_common::kernel;
use steery_driver_common::program::helper;
use steery_sdk::wire::harness::{
    Act, Ended, Event, Execution, Held, HeldBy, Holder, HolderStop, Launch, Look, Looked, Marked,
    Model, Models, Read, Receipt, ReceiptFind, ReceiptMark, Sample, Sampled, Start, Stdin, Stop,
    Stopped, Stream, Terminal, TerminalCommand, TurnLine, TurnRead, TurnState,
};
use steery_sdk::{Error, refusal};

use super::sessions::Store;
use super::store;
use crate::turn::drive as shared;

const LABEL: &str = "Cursor";

/// How long `cursor-agent models` has to answer; it asks Cursor's service.
const MODELS_WITHIN: Duration = Duration::from_secs(20);

/// The bytes that `idle` reads of a chat's last turn.
const IDLE_TAIL: u64 = 256 * 1024;

/// `models`: what `cursor-agent models` prints, `<id> - <label>` on each line, the default
/// marked `(current, default)` or `(default)`. Cursor has no thinking setting of its own: a
/// level is part of a model's id. Signed out, it prints `Error: Authentication required`
/// and exits 1. Cursor keeps its sign-in in `cli-config.json`, beside the model it last used.
pub async fn models(binary: Option<PathBuf>, home: &Path, root: &Path) -> Result<Models, Error> {
    let binary = binary
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "cursor-agent is not installed here"))?;
    let mut command = helper(&binary);
    command
        .arg("models")
        .env("HOME", home)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = tokio::time::timeout(MODELS_WITHIN, command.output())
        .await
        .map_err(|_| {
            Error::refused(
                refusal::REFUSED,
                format!(
                    "cursor-agent models did not answer within {} s",
                    MODELS_WITHIN.as_secs()
                ),
            )
        })?
        .map_err(|e| {
            Error::refused(
                refusal::REFUSED,
                format!("cursor-agent models did not start: {e}"),
            )
        })?;
    let watch = vec![root.join("cli-config.json")];
    let printed = String::from_utf8_lossy(&output.stdout);
    let said = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        return Ok(Models {
            models: listed(&printed),
            unlisted: None,
            watch,
            signed_in: Some(true),
        });
    }
    if [&printed, &said]
        .iter()
        .any(|text| text.contains("Authentication required"))
    {
        return Ok(Models {
            models: Vec::new(),
            unlisted: None,
            watch,
            signed_in: Some(false),
        });
    }
    let last = said
        .lines()
        .chain(printed.lines())
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    Err(Error::refused(
        refusal::REFUSED,
        format!(
            "cursor-agent models exited with {}: {}",
            output.status,
            last.trim()
        ),
    ))
}

/// Each `<id> - <label>` line, the default first.
fn listed(out: &str) -> Vec<Model> {
    let mut models: Vec<(bool, Model)> = out
        .lines()
        .filter_map(|line| {
            let (id, label) = line.trim().split_once(" - ")?;
            let id = id.trim();
            if id.is_empty() || id.contains(char::is_whitespace) {
                return None;
            }
            let (label, default) = match label.trim().rsplit_once(" (") {
                Some((name, marks)) if marks.ends_with(')') => {
                    let marks = &marks[..marks.len() - 1];
                    (name, marks.split(", ").any(|mark| mark == "default"))
                }
                _ => (label.trim(), false),
            };
            Some((
                default,
                Model {
                    provider: None,
                    model: id.to_owned(),
                    label: Some(label.to_owned()),
                    thinking: Vec::new(),
                    cost: None,
                },
            ))
        })
        .collect();
    models.sort_by_key(|(default, _)| !default);
    models.into_iter().map(|(_, model)| model).collect()
}

/// `start`: the plan of `cursor-agent -p --output-format stream-json` in `cwd`. A continue
/// names a chat that Cursor keeps here: `--resume` of an id it does not know starts a new
/// chat under that id. Cursor checks the model itself, on standard error.
pub fn start(binary: Option<PathBuf>, store: &Store, start: Start) -> Result<Launch, Error> {
    let binary = binary
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "cursor-agent is not installed here"))?;
    if !start.cwd.is_absolute() {
        return Err(Error::invalid("the folder is not an absolute path"));
    }
    if start.worktree {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Steery does not start Cursor in a worktree of its own",
        ));
    }
    if let Some(thinking) = &start.thinking {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("Cursor has no thinking level {thinking} apart from its model"),
        ));
    }
    let mut argv = vec![
        text_of(binary)?,
        "-p".into(),
        "--output-format".into(),
        "stream-json".into(),
    ];
    match start.act {
        Act::New => {}
        Act::Continue => {
            let id = start
                .external_id
                .ok_or_else(|| Error::invalid("a continue must name its chat"))?;
            if !plain_id(&id) || store.transcript(&id).is_err() {
                return Err(Error::refused(
                    refusal::NOT_FOUND,
                    format!("Cursor keeps no chat {id} here"),
                ));
            }
            if !start.cwd.is_dir() {
                return Err(Error::refused(
                    refusal::NO_PLACE,
                    format!(
                        "{} is gone, and Cursor resumes the chat there",
                        start.cwd.display()
                    ),
                ));
            }
            argv.extend(["--resume".into(), id]);
        }
        Act::Fork | Act::Hand => {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Steery starts only new and continued Cursor turns",
            ));
        }
    }
    // `--model` also makes the model the person's default: Cursor writes it into
    // `cli-config.json` (`model`, `selectedModel`, `modelParameters`), so their next chat
    // starts with it too.
    if let Some(model) = start.model {
        argv.extend(["--model".into(), model]);
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

/// Each turn's state between its lines, by the runtime's name of the turn.
#[derive(Default)]
pub struct Turns {
    turns: Mutex<HashMap<String, Run>>,
}

/// One turn as its lines so far left it.
#[derive(Default)]
struct Run {
    named: Option<String>,
    ended: bool,
}

impl Turns {
    /// `turn/read`. Its lines carry no events: a call's input and result in them are not
    /// those of the store, so they are not the same fact. The runtime reads the store.
    pub fn read(&self, read: TurnRead) -> TurnLine {
        let mut turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
        let run = turns.entry(read.turn.clone()).or_default();
        let mut line = TurnLine::default();
        if run.ended {
            return line;
        }
        let Ok(record) = serde_json::from_str::<Value>(&read.line) else {
            if let Some(word) = refusal_of(&read.line) {
                run.ended = true;
                line.ended = Some(Ended {
                    state: TurnState::Failed,
                    error: Some(read.line.trim().to_owned()),
                });
                line.refused = (read.stream == Stream::Stderr).then(|| word.to_owned());
            }
            return line;
        };
        let text = |key: &str| record.get(key).and_then(Value::as_str);
        if let Some(named) = text("session_id")
            && run.named.as_deref() != Some(named)
        {
            run.named = Some(named.to_owned());
            line.named = Some(named.to_owned());
            line.took = Some(false);
        }
        match text("type").unwrap_or_default() {
            // The words on standard input, which Cursor took.
            "user" => line.took = Some(true),
            "result" => {
                run.ended = true;
                let failed = record.get("is_error").and_then(Value::as_bool) != Some(false)
                    || text("subtype") != Some("success");
                line.ended = Some(match failed {
                    false => Ended {
                        state: TurnState::Finished,
                        error: None,
                    },
                    true => Ended {
                        state: TurnState::Failed,
                        error: Some(
                            text("result")
                                .filter(|said| !said.trim().is_empty())
                                .map_or_else(|| "Cursor's turn failed".to_owned(), str::to_owned),
                        ),
                    },
                });
            }
            _ => {}
        }
        line
    }
}

/// The refusal that one of Cursor's own lines means, where it ends the turn (2026.11.05).
/// Cursor words a refusal of its service `ActionRequiredError: ...` and its own `Error: ...`.
/// It asks about an untrusted folder in several lines; the first that names it ends the
/// turn.
fn refusal_of(line: &str) -> Option<&'static str> {
    let line = line.trim();
    if line.contains("Workspace Trust Required") || line.contains("Pass --trust") {
        Some(refusal::UNTRUSTED)
    } else if line.starts_with("Cannot use this model:") {
        Some(refusal::MODEL_MISSING)
    } else if line.starts_with("ActionRequiredError:") || line.starts_with("Error:") {
        Some(refusal::REFUSED)
    } else {
        None
    }
}

/// `held`: the process that holds SQLite's lock on the chat's store. Cursor opens the store
/// for as long as it has the chat open, a turn of `-p` or a person's terminal. Where none
/// does, the runtime looks.
pub fn held(store: &Store, held: Held) -> Result<HeldBy, Error> {
    let transcript = store.transcript(&held.external_id)?;
    if let Some(pid) = store::holder(&transcript)
        && let Some(process_start) = kernel::started(pid)
    {
        return Ok(HeldBy::Holder(Holder::Person {
            door: None,
            stop: HolderStop::Process {
                pid,
                process_start,
                certain: true,
                how: "in Cursor".into(),
            },
        }));
    }
    Ok(HeldBy::Look(Looked {
        look: Look {
            transcripts: vec![transcript],
            strays: true,
            beyond: Vec::new(),
        },
    }))
}

/// `idle`: how the chat's last turn ended, where it did. It stands while nobody holds the
/// store.
pub fn idle(store: &Store, held: Held) -> Result<Sampled, Error> {
    let read = store.read(Read {
        external_id: held.external_id,
        sub_agent: None,
        cursor: None,
        tail: Some(IDLE_TAIL),
    })?;
    Ok(Sampled {
        sample: sample(&read.events),
    })
}

/// The sample of the last turn in `events`, where it ended.
fn sample(events: &[Event]) -> Option<Sample> {
    let mut started_at = None;
    let mut ended = None;
    for event in events {
        match event {
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
        observed_at: shared::now(),
        started_at,
        ended_at: Some(ended_at),
        model: None,
        thinking: None,
        error,
        vacant: false,
        if_unheld: true,
    })
}

/// `stop`: Cursor has no way to end a turn from outside its process; the runtime's signals
/// end the turn's own process. They never reach Cursor's `worker-server`, which outlives
/// the turn in a process group of its own.
pub fn stop(stop: &Stop) -> Result<Stopped, Error> {
    shared::stop(LABEL, stop)
}

/// `receipt/mark`: how many turns the chat has.
pub fn mark(store: &Store, mark: ReceiptMark) -> Result<Marked, Error> {
    Ok(Marked {
        mark: store.said(&mark.external_id)?.len().to_string(),
    })
}

/// `receipt/find`: whether exactly one person's words after the mark carry the delivery.
pub fn find(store: &Store, find: ReceiptFind) -> Result<Receipt, Error> {
    let after: usize = find
        .mark
        .parse()
        .map_err(|_| Error::invalid(format!("mark {} is not a mark of Cursor's", find.mark)))?;
    let said = store.said(&find.external_id)?;
    shared::find(said.get(after..).unwrap_or_default(), &find.delivery)
}

/// `terminal`: `cd '<cwd>' && cursor-agent --resume <id>`.
pub fn terminal(store: &Store, terminal: Terminal) -> Result<TerminalCommand, Error> {
    let id = &terminal.external_id;
    if !plain_id(id) || store.transcript(id).is_err() {
        return Err(Error::refused(
            refusal::NOT_FOUND,
            format!("Cursor keeps no chat {id} here"),
        ));
    }
    if !terminal.cwd.is_absolute() {
        return Err(Error::invalid("the folder is not an absolute path"));
    }
    let cwd = terminal
        .cwd
        .to_str()
        .ok_or_else(|| Error::invalid(format!("{} is not text", terminal.cwd.display())))?;
    let line = match &terminal.holder {
        Some(Holder::Person {
            stop: HolderStop::Process { pid, .. },
            ..
        }) => Some(format!(
            "Cursor has it open already (pid {pid}). This command opens it a second time."
        )),
        Some(Holder::Person {
            stop: HolderStop::Beyond(how),
            ..
        }) => Some(how.clone()),
        _ => None,
    };
    Ok(TerminalCommand {
        command: format!("cd {} && cursor-agent --resume {id}", quoted(cwd)),
        app: None,
        line,
    })
}

/// Whether an id is one plain word that a command and a path take as it is.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What `cursor-agent models` printed (2026.11.05), its default first.
    #[test]
    fn reads_each_line_as_a_model() {
        let out = "Available models\n\ngpt-5.2 - GPT-5.2\nauto - Auto (current, default)\n\
                   grok-4.7-high - Grok 4.7 High (current)\n";
        let read: Vec<(String, Option<String>)> = listed(out)
            .into_iter()
            .map(|model| (model.model, model.label))
            .collect();
        assert_eq!(
            read,
            [
                ("auto".into(), Some("Auto".into())),
                ("gpt-5.2".into(), Some("GPT-5.2".into())),
                ("grok-4.7-high".into(), Some("Grok 4.7 High".into())),
            ]
        );
    }
}
