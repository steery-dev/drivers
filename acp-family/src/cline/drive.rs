//! Cline at drive: Steery starts Cline's turns itself. A turn is this driver's own binary in
//! its turn mode, speaking ACP to `cline --acp` ([`crate::turn`]) with its session backend
//! local, so the turn's own process runs the conversation. Cline marks a conversation
//! `running` while a turn of it runs and `pending` while queued words wait, with the pid of
//! the process that runs it; a process that dies in a turn leaves its mark behind, so a mark
//! counts only while its process lives.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    Act, Execution, Held, HeldBy, Holder, HolderStop, Launch, Marked, Model, Models, Receipt,
    ReceiptFind, ReceiptMark, Sample, Sampled, Start, Terminal, TerminalCommand, TurnState,
};
use steery_sdk::{Error, refusal};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::events;
use super::place::Place;
use super::store::{Row, Store};
use crate::turn::drive::{self as shared, now};

/// How long `cline --acp` has to open a session that lists Cline's models.
const LISTED_WITHIN: Duration = Duration::from_secs(30);

/// How long the newest words of a conversation may wait for an answer with no live mark
/// before the turn takes it that Cline runs nothing more of it.
const UNANSWERED_WITHIN: Duration = Duration::from_secs(5);

/// ACP's refusal of a session to a client that is not signed in.
const AUTH_REQUIRED: i64 = -32000;

/// The environment of Cline's ACP server: the session runs in the server's own process.
const ENV: [(&str, &str); 1] = [("CLINE_SESSION_BACKEND_MODE", "local")];

/// Why `start` refuses where Cline is signed out.
pub(crate) const SIGNED_OUT: &str = "Cline is signed out: run `cline auth`";

/// Cline, to a turn this driver runs ([`crate::turn`]): words told mid-turn go in as one
/// more prompt, which Cline queues behind the one running.
pub struct Turns {
    /// Cline's folders, where the turn has a home to find them in.
    place: Option<Place>,
}

impl Turns {
    /// Cline's folders under `$HOME`, as the runtime starts the turn.
    pub fn new() -> Self {
        Self {
            place: std::env::var_os("HOME").map(|home| Place::new(Path::new(&home))),
        }
    }
}

impl crate::turn::Host for Turns {
    fn harness(&self) -> crate::Harness {
        crate::Harness::Cline
    }

    fn told(&self) -> crate::turn::Told {
        crate::turn::Told::Prompt
    }

    fn serves(&self) -> &'static [&'static str] {
        &["--acp"]
    }

    fn env(&self) -> &'static [(&'static str, &'static str)] {
        &ENV
    }

    fn opens(&self) -> crate::turn::Opens {
        crate::turn::Opens::Load
    }

    /// Cline marks the conversation running or pending in a live process, or in this
    /// turn's own server, whose resumed conversation keeps the pid of the process that first
    /// ran it; or words it queued are not yet among the person's, or its newest words have
    /// no answer yet, as between a turn and the queued words' turn after it, for at most
    /// [`UNANSWERED_WITHIN`] with no mark.
    fn runs(
        &self,
        acp: Option<u32>,
        session: &str,
        queued: &[String],
        unmarked: &mut Option<tokio::time::Instant>,
    ) -> bool {
        let Some(place) = &self.place else {
            return false;
        };
        let db = place.db();
        let Ok(Some(row)) = Store::new(&db).row(session) else {
            return false;
        };
        if row.live().is_some()
            || (acp.is_some() && matches!(row.status.as_str(), "running" | "pending"))
        {
            *unmarked = None;
            return true;
        }
        let waits = Store::messages(&row).is_ok_and(|messages| {
            messages.last().is_some_and(|last| last["role"] == "user")
                || queued.iter().any(|words| {
                    let words = words.trim();
                    !messages
                        .iter()
                        .filter(|message| message["role"] == "user")
                        .flat_map(|message| message["content"].as_array().into_iter().flatten())
                        .filter_map(|block| block["text"].as_str())
                        .any(|text| events::person(text).contains(words))
                })
        });
        waits
            && unmarked
                .get_or_insert_with(tokio::time::Instant::now)
                .elapsed()
                < UNANSWERED_WITHIN
    }
}

/// `held`: the process Cline's mark names while it runs a turn of the conversation or
/// queued words wait. Cline marks every turn it runs, whatever runs it, so with no live mark,
/// or no such conversation, nobody runs one.
pub fn held(place: &Place, held: &Held) -> Result<HeldBy, Error> {
    let db = place.db();
    let Some(row) = Store::new(&db).row(&held.external_id)? else {
        return Ok(HeldBy::Holder(Holder::Nobody {
            proof: "Cline has no such conversation".into(),
        }));
    };
    Ok(HeldBy::Holder(match row.live() {
        Some((pid, process_start)) => Holder::Person {
            door: None,
            stop: HolderStop::Process {
                pid,
                process_start,
                certain: true,
                how: "in Cline, running a turn".into(),
            },
        },
        None => Holder::Nobody {
            proof: "Cline's mark of the conversation (sessions.status) names no live process \
                    running or waiting to run a turn of it"
                .into(),
        },
    }))
}

/// `idle`: running while Cline's mark lives; else how its last turn ended, which stands
/// while nobody holds it.
pub fn idle(place: &Place, held: &Held) -> Sampled {
    let db = place.db();
    Sampled {
        sample: Store::new(&db)
            .row(&held.external_id)
            .ok()
            .flatten()
            .map(|row| sample(&row, now())),
    }
}

/// Where a conversation stands by Cline's mark of it.
fn sample(row: &Row, now: u64) -> Sample {
    let mut sample = Sample {
        state: Execution::Running,
        observed_at: now,
        started_at: row.started,
        ended_at: None,
        model: (!row.model.is_empty()).then(|| row.model.clone()),
        thinking: None,
        error: None,
        vacant: false,
        if_unheld: false,
    };
    if row.live().is_some() {
        return sample;
    }
    sample.vacant = true;
    sample.if_unheld = true;
    sample.ended_at = Some(row.updated);
    sample.state = match row.ended() {
        // Cline marks a cancelled turn idle too; its words then have no answer.
        TurnState::Finished
            if Store::messages(row).is_ok_and(|messages| {
                messages.last().is_some_and(|last| last["role"] == "user")
            }) =>
        {
            Execution::Interrupted
        }
        TurnState::Finished => Execution::Finished,
        TurnState::Failed => {
            sample.error = Store::messages(row)
                .ok()
                .and_then(|messages| failure(&messages));
            Execution::Failed
        }
        TurnState::Interrupted => Execution::Interrupted,
    };
    sample
}

/// The words of Cline's newest error message.
fn failure(messages: &[Value]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|message| message.pointer("/metadata/displayRole") == Some(&json!("error")))?
        ["content"]
        .as_array()?
        .iter()
        .find_map(|block| block["text"].as_str().map(str::to_owned))
}

/// `models`: the models of the session that `cline --acp` opens, its default first. The
/// session is never prompted, so Cline keeps nothing of it. Where Cline refuses a session
/// for want of a sign-in, it lists nothing and says so.
pub async fn models(place: &Place) -> Result<Models, Error> {
    let cline = super::place::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "Cline is not installed"))?;
    let watch = vec![place.providers()];
    let opened = tokio::time::timeout(LISTED_WITHIN, open(&cline))
        .await
        .map_err(|_| {
            Error::refused(
                refusal::REFUSED,
                format!(
                    "`cline --acp` opened no session within {} s",
                    LISTED_WITHIN.as_secs()
                ),
            )
        })??;
    if opened.pointer("/error/code").and_then(Value::as_i64) == Some(AUTH_REQUIRED) {
        return Ok(Models {
            models: Vec::new(),
            unlisted: None,
            watch,
            signed_in: Some(false),
        });
    }
    if let Some(error) = opened.get("error") {
        let said = error["message"].as_str().unwrap_or("refused");
        return Err(Error::refused(
            refusal::REFUSED,
            format!("`cline --acp` opened no session: {said}"),
        ));
    }
    Ok(Models {
        models: listed(&opened["result"]),
        unlisted: None,
        watch,
        signed_in: Some(true),
    })
}

/// Start `cline --acp` in a folder of its own, initialize it, and ask for a new session;
/// Cline's answer to that.
async fn open(cline: &Path) -> Result<Value, Error> {
    let failed = |e: std::io::Error| {
        Error::refused(refusal::REFUSED, format!("`cline --acp` did not run: {e}"))
    };
    let cwd = std::env::temp_dir().join(format!("steery-cline-models-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).map_err(failed)?;
    let mut child = tokio::process::Command::new(cline)
        .arg("--acp")
        .envs(ENV)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(failed)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| failed(std::io::ErrorKind::BrokenPipe.into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failed(std::io::ErrorKind::BrokenPipe.into()))?;
    let mut lines = BufReader::new(stdout).lines();
    let requests = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": cwd.to_string_lossy(), "mcpServers": []}}),
    ];
    let mut answer = Value::Null;
    for request in requests {
        let id = request["id"].clone();
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(failed)?;
        answer = loop {
            let Some(line) = lines.next_line().await.map_err(failed)? else {
                return Err(Error::refused(
                    refusal::REFUSED,
                    "`cline --acp` closed before it answered",
                ));
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("method").is_none() && message["id"] == id {
                break message;
            }
        };
        if answer.get("error").is_some() {
            break;
        }
    }
    drop(stdin);
    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&cwd);
    Ok(answer)
}

/// The models of a session's answer: `models.availableModels`, `currentModelId` first. Cline
/// offers no thinking levels through ACP.
fn listed(session: &Value) -> Vec<Model> {
    let current = session
        .pointer("/models/currentModelId")
        .and_then(Value::as_str);
    let mut models: Vec<Model> = session
        .pointer("/models/availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            Some(Model {
                model: model["modelId"].as_str()?.to_owned(),
                provider: None,
                label: model["name"].as_str().map(str::to_owned),
                thinking: Vec::new(),
                cost: None,
            })
        })
        .collect();
    if let Some(at) = current.and_then(|current| models.iter().position(|m| m.model == current)) {
        let first = models.remove(at);
        models.insert(0, first);
    }
    models
}

/// `start`: the launch plan of a turn ([`shared::launch`]). Cline has no fork, no hand-over
/// and no thinking level a client can pick; a model its last list (`listed`) does not have
/// is refused before anything runs.
pub fn start(
    place: &Place,
    listed: &[Model],
    signed_out: bool,
    start: Start,
) -> Result<Launch, Error> {
    if matches!(start.act, Act::Fork | Act::Hand) {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Cline continues a conversation or starts one; it has no fork or hand-over",
        ));
    }
    if start.thinking.is_some() {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Cline takes no thinking level from its ACP client",
        ));
    }
    if signed_out {
        return Err(Error::refused(refusal::REFUSED, SIGNED_OUT));
    }
    shared::offered("Cline", listed, start.model.as_deref(), None)?;
    let hook = shared::hook(&super::setup::hook_file(&place.dir));
    let db = place.db();
    let store = Store::new(&db);
    shared::launch(
        "Cline",
        super::place::binary().as_deref(),
        start,
        |id| store.directory(id),
        hook,
        BTreeMap::new(),
    )
}

/// `receipt/mark`: how many messages the conversation has.
pub fn mark(place: &Place, mark: &ReceiptMark) -> Result<Marked, Error> {
    let db = place.db();
    Ok(Marked {
        mark: Store::new(&db).count(&mark.external_id)?.to_string(),
    })
}

/// `receipt/find`: whether exactly one of the person's words after the mark carries the
/// marker.
pub fn find(place: &Place, find: &ReceiptFind) -> Result<Receipt, Error> {
    let db = place.db();
    let said = Store::new(&db).said_after(&find.external_id, &find.mark)?;
    shared::find(&said, &find.delivery)
}

/// `terminal`: Cline's own window on the conversation, in the folder it ran in.
pub fn terminal(place: &Place, terminal: &Terminal) -> Result<TerminalCommand, Error> {
    let db = place.db();
    let cwd = Store::new(&db)
        .directory(&terminal.external_id)?
        .unwrap_or_else(|| terminal.cwd.clone());
    Ok(TerminalCommand {
        command: format!(
            "cd {} && cline --id {}",
            quoted(&cwd.to_string_lossy()),
            quoted(&terminal.external_id)
        ),
        app: None,
        line: None,
    })
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_model_is_listed_first() {
        let session = json!({"models": {"currentModelId": "b", "availableModels": [
            {"modelId": "a", "name": "A"}, {"modelId": "b", "name": "B"}]}});
        let models = listed(&session);
        assert_eq!(models[0].model, "b");
        assert_eq!(models.len(), 2);
        assert!(models.iter().all(|model| model.thinking.is_empty()));
    }
}
