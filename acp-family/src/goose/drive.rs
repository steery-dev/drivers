//! Goose at drive: Steery starts Goose's turns itself. A turn is this driver's own binary in
//! its turn mode, speaking ACP to `goose acp` ([`crate::turn`]), which runs the conversation
//! in the turn's own process. Goose keeps no mark of a running turn, no lock, no status, no
//! pid, so who holds a conversation is for the runtime to look for, and what stands of it is
//! true only while nobody does.
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    Act, Beyond, Event, Execution, Held, HeldBy, Holder, Launch, Look, Looked, Marked, Model,
    Models, Receipt, ReceiptFind, ReceiptMark, Sample, Sampled, Start, Terminal, TerminalCommand,
    TurnState,
};
use steery_sdk::{Error, refusal};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout};

use super::events::{Fold, State};
use super::place::Place;
use super::store::Store;
use crate::turn::drive::{self as shared, now};

/// How long `goose acp` has to open a session that lists Goose's models.
const LISTED_WITHIN: Duration = Duration::from_secs(30);

/// Goose's words where a session has no provider to run it, which is how it is signed out
/// (`acp/server/new_session.rs:149-152`).
const NO_PROVIDER: &str = "Failed to resolve provider and model";

/// The config option of a session that picks its model.
const MODEL: &str = "model";

/// The config option of a session that picks its thinking level.
pub(crate) const THINKING: &str = "thinking_effort";

/// Why `start` refuses where Goose is signed out.
const SIGNED_OUT: &str = "Goose has no provider: run `goose configure`";

/// Goose, to a turn this driver runs ([`crate::turn`]): words told mid-turn go in as one
/// prompt once Goose has answered the turn's. Goose refuses a second `session/prompt` while
/// one runs (`-32602`, "session already has active run"), and its steer is unstable.
pub struct Turns;

impl crate::turn::Host for Turns {
    fn harness(&self) -> crate::Harness {
        crate::Harness::Goose
    }

    fn told(&self) -> crate::turn::Told {
        crate::turn::Told::After
    }

    fn serves(&self) -> &'static [&'static str] {
        &["acp"]
    }

    fn opens(&self) -> crate::turn::Opens {
        crate::turn::Opens::Load
    }

    fn thinking(&self) -> &'static str {
        THINKING
    }

    /// Goose runs one prompt of a session at a time and nothing of it without one: once it
    /// has answered every prompt the turn sent, the turn's server runs nothing more of it.
    fn runs(
        &self,
        _acp: Option<u32>,
        _session: &str,
        _queued: &[String],
        _: &mut Option<tokio::time::Instant>,
    ) -> bool {
        false
    }
}

/// `held`: with no such conversation, nobody runs a turn of it. Else Goose keeps no mark of
/// who runs it, so the runtime looks for the processes that hold its database. A `goose`
/// that runs a conversation need not hold the database open between its writes, so a stray
/// can hold it. `goose acp` serves every conversation its client opens, so stopping it
/// would stop them all.
pub fn held(place: &Place, held: &Held) -> Result<HeldBy, Error> {
    let db = place.db();
    if Store::new(&db).row(&held.external_id)?.is_none() {
        return Ok(HeldBy::Holder(Holder::Nobody {
            proof: "Goose has no such conversation".into(),
        }));
    }
    Ok(HeldBy::Look(Looked {
        look: Look {
            transcripts: vec![place.db()],
            strays: true,
            beyond: vec![Beyond {
                arg: Some("acp".into()),
                line: "Goose's ACP server, which serves every conversation its client opens".into(),
            }],
        },
    }))
}

/// `idle`: how the conversation's last turn ended, which stands only while nobody holds it.
pub fn idle(place: &Place, held: &Held) -> Sampled {
    let db = place.db();
    let store = Store::new(&db);
    Sampled {
        sample: store.row(&held.external_id).ok().flatten().and_then(|row| {
            let messages = store.messages(&row.id).ok()?;
            Some(sample(&messages, row.updated, now()))
        }),
    }
}

/// Where a conversation stands by its messages. With nobody holding it, a turn that Goose
/// neither answered nor failed was cut off.
fn sample(messages: &[super::events::Message], updated: u64, now: u64) -> Sample {
    let mut fold = Fold::new(State::default(), &[]);
    for message in messages {
        fold.message(message);
    }
    fold.settle();
    let open = fold.state.open();
    let mut model = None;
    let mut ended = None;
    for event in &fold.events {
        match event {
            Event::Model { model: named, .. } => model = Some(named.clone()),
            Event::TurnEnded { state, failure, .. } => ended = Some((*state, failure.clone())),
            _ => {}
        }
    }
    let (state, error) = match ended {
        _ if open => (Execution::Interrupted, None),
        Some((TurnState::Failed, failure)) => {
            (Execution::Failed, failure.map(|failure| failure.line))
        }
        Some((TurnState::Interrupted, _)) => (Execution::Interrupted, None),
        _ => (Execution::Finished, None),
    };
    Sample {
        state,
        observed_at: now,
        started_at: messages
            .iter()
            .rev()
            .find(|message| message.begins())
            .map(|message| message.at),
        ended_at: Some(updated),
        model,
        thinking: None,
        error,
        vacant: true,
        if_unheld: true,
    }
}

/// `models`: the models of the session that `goose acp` opens, its default first, each with
/// the thinking levels Goose offers it. The session is deleted again, so Goose keeps no row
/// of it.
/// Where Goose has no provider to open a session with, it lists nothing and says so.
pub async fn models(place: &Place) -> Result<Models, Error> {
    let goose = place
        .binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "Goose is not installed"))?;
    let watch = vec![place.config_yaml()];
    let (opened, levels) = tokio::time::timeout(LISTED_WITHIN, open(&goose))
        .await
        .map_err(|_| {
            Error::refused(
                refusal::REFUSED,
                format!(
                    "`goose acp` opened no session within {} s",
                    LISTED_WITHIN.as_secs()
                ),
            )
        })??;
    if let Some(error) = opened.get("error") {
        if error["data"]
            .as_str()
            .is_some_and(|data| data.starts_with(NO_PROVIDER))
        {
            return Ok(Models {
                models: Vec::new(),
                unlisted: None,
                watch,
                signed_in: Some(false),
            });
        }
        let said = error["data"]
            .as_str()
            .or(error["message"].as_str())
            .unwrap_or("refused");
        return Err(Error::refused(
            refusal::REFUSED,
            format!("`goose acp` opened no session: {said}"),
        ));
    }
    Ok(Models {
        models: listed(&opened["result"], &levels),
        unlisted: None,
        watch,
        signed_in: Some(true),
    })
}

/// Start `goose acp` in a folder of its own, initialize it, ask for a new session, pick
/// each of its other models in turn to read that model's thinking levels (Goose offers each
/// model its own: `claude-haiku-4.5` has only `off`), and delete that session again; Goose's
/// answer to the new session, with the thinking levels of each model it picked.
async fn open(goose: &Path) -> Result<(Value, BTreeMap<String, Vec<String>>), Error> {
    let cwd = std::env::temp_dir().join(format!("steery-goose-models-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).map_err(failed)?;
    let mut child = tokio::process::Command::new(goose)
        .arg("acp")
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
    let initialized = ask(
        &mut stdin,
        &mut lines,
        1,
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    )
    .await?;
    let mut answer = initialized;
    if answer.get("error").is_none() {
        answer = ask(
            &mut stdin,
            &mut lines,
            2,
            "session/new",
            json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}),
        )
        .await?;
    }
    let mut levels = BTreeMap::new();
    if let Some(session) = answer.pointer("/result/sessionId").cloned() {
        let picked = answer["result"]["configOptions"].clone();
        let current = option(&picked, MODEL).and_then(|model| model["currentValue"].as_str());
        let others: Vec<String> = values(option(&picked, MODEL))
            .into_iter()
            .map(|(model, _)| model)
            .filter(|model| Some(model.as_str()) != current)
            .collect();
        for (id, model) in (3..).zip(others) {
            let set = ask(
                &mut stdin,
                &mut lines,
                id,
                "session/set_config_option",
                json!({"sessionId": session, "configId": MODEL, "value": model}),
            )
            .await?;
            let thinking = values(option(&set["result"]["configOptions"], THINKING));
            levels.insert(
                model,
                thinking.into_iter().map(|(level, _)| level).collect(),
            );
        }
        let deleted = ask(
            &mut stdin,
            &mut lines,
            0,
            "session/delete",
            json!({"sessionId": session}),
        )
        .await?;
        if let Some(error) = deleted.get("error") {
            return Err(Error::refused(
                refusal::REFUSED,
                format!("`goose acp` kept the session it listed models in: {error}"),
            ));
        }
    }
    drop(stdin);
    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&cwd);
    Ok((answer, levels))
}

/// Send one request and read lines until its answer.
async fn ask(
    stdin: &mut ChildStdin,
    lines: &mut Lines<BufReader<ChildStdout>>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value, Error> {
    let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .await
        .map_err(failed)?;
    loop {
        let Some(line) = lines.next_line().await.map_err(failed)? else {
            return Err(Error::refused(
                refusal::REFUSED,
                "`goose acp` closed before it answered",
            ));
        };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message.get("method").is_none() && message["id"] == id {
            return Ok(message);
        }
    }
}

fn failed(e: std::io::Error) -> Error {
    Error::refused(refusal::REFUSED, format!("`goose acp` did not run: {e}"))
}

/// The models of a session's answer: the options of its `model` config option, its
/// `currentValue` first with the session's own `thinking_effort` options, each other model
/// with the levels Goose offered once it was picked (`levels`).
fn listed(session: &Value, levels: &BTreeMap<String, Vec<String>>) -> Vec<Model> {
    let options = &session["configOptions"];
    let thinking: Vec<String> = values(option(options, THINKING))
        .into_iter()
        .map(|(value, _)| value)
        .collect();
    let model = option(options, MODEL);
    let current = model.and_then(|model| model["currentValue"].as_str());
    let mut models: Vec<Model> = values(model)
        .into_iter()
        .map(|(model, label)| Model {
            thinking: levels.get(&model).unwrap_or(&thinking).clone(),
            model,
            provider: None,
            label,
            cost: None,
        })
        .collect();
    if let Some(at) = current.and_then(|current| models.iter().position(|m| m.model == current)) {
        let first = models.remove(at);
        models.insert(0, first);
    }
    models
}

/// The config option `id` of a session's options.
fn option<'a>(options: &'a Value, id: &str) -> Option<&'a Value> {
    options
        .as_array()
        .into_iter()
        .flatten()
        .find(|option| option["id"] == id)
}

/// The values of a select config option, each with its name.
fn values(option: Option<&Value>) -> Vec<(String, Option<String>)> {
    option
        .and_then(|option| option["options"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| {
            Some((
                value["value"].as_str()?.to_owned(),
                value["name"].as_str().map(str::to_owned),
            ))
        })
        .collect()
}

/// `start`: the launch plan of a turn ([`shared::launch`]). Goose's ACP server loads a
/// conversation or starts one; it has no fork and no hand-over. A model or thinking level
/// its last list (`listed`) does not have is refused before anything runs.
pub fn start(
    place: &Place,
    listed: &[Model],
    signed_out: bool,
    start: Start,
) -> Result<Launch, Error> {
    if matches!(start.act, Act::Fork | Act::Hand) {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Goose continues a conversation or starts one; it has no fork or hand-over",
        ));
    }
    if signed_out {
        return Err(Error::refused(refusal::REFUSED, SIGNED_OUT));
    }
    shared::offered(
        "Goose",
        listed,
        start.model.as_deref(),
        start.thinking.as_deref(),
    )?;
    let hook = shared::hook(&super::setup::hook_file(&place.config));
    let db = place.db();
    let store = Store::new(&db);
    shared::launch(
        "Goose",
        place.binary().as_deref(),
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

/// `terminal`: Goose's own chat on the conversation, in the folder it ran in.
pub fn terminal(place: &Place, terminal: &Terminal) -> Result<TerminalCommand, Error> {
    let db = place.db();
    let cwd = Store::new(&db)
        .directory(&terminal.external_id)?
        .unwrap_or_else(|| terminal.cwd.clone());
    Ok(TerminalCommand {
        command: format!(
            "cd {} && goose session --resume --session-id {}",
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
