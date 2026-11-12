//! Hermes at drive: Steery starts Hermes's turns itself. A turn is this driver's own binary
//! in its turn mode, speaking ACP to `hermes acp` ([`crate::turn`]). `hermes acp` is one
//! more client of Hermes's gateway, which runs every turn of every client, queues the
//! prompts of a conversation in order, and marks the turn it runs with a lease
//! ([`Standing`]). A cancel reaches only the client's own turn, and the gateway runs a turn
//! on when its client is gone, so a turn that another client runs is beyond Steery's stop.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    Act, Event, Execution, Held, HeldBy, Holder, HolderStop, Launch, Marked, Model, Models,
    Receipt, ReceiptFind, ReceiptMark, Sample, Sampled, Start, Terminal, TerminalCommand,
    TurnState,
};
use steery_sdk::{Error, refusal};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout};

use super::store::{Home, Standing};
use crate::turn::drive::{self as shared, now};

/// How long `hermes acp` has to open the session that lists Hermes's models: it starts
/// Hermes's gateway first where none runs.
const LISTED_WITHIN: Duration = Duration::from_secs(90);

/// The config option of a session that picks its model.
const MODEL: &str = "model";

/// The config option of a session that picks its thinking level
/// (`acp_adapter/gateway_session_meta.py:72`).
const THINKING: &str = "reasoning_effort";

/// The thinking level that is none set: the profile's own.
const UNSET: &str = "default";

/// The auth method `hermes acp` offers where no provider is configured, alone then
/// (`acp_adapter/auth.py:8`).
const SETUP: &str = "hermes-setup";

/// Why `start` refuses where Hermes is signed out.
const SIGNED_OUT: &str = "Hermes has no provider: run `hermes setup`";

/// Hermes, to a turn this driver runs ([`crate::turn`]): words told mid-turn go in as one
/// more `session/prompt`, which Hermes's gateway queues behind the running one and answers
/// once it ran.
pub struct Turns {
    root: PathBuf,
}

impl Turns {
    pub fn new() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self {
            root: super::store::root(&home),
        }
    }
}

impl crate::turn::Host for Turns {
    fn harness(&self) -> crate::Harness {
        crate::Harness::Hermes
    }

    fn told(&self) -> crate::turn::Told {
        crate::turn::Told::Prompt
    }

    fn thinking(&self) -> &'static str {
        THINKING
    }

    /// Words another client queued run in the gateway after the turn's own: while Hermes's
    /// marks say a turn of the session runs or waits, the turn reads on.
    fn runs(
        &self,
        _acp: Option<u32>,
        session: &str,
        _queued: &[String],
        _: &mut Option<tokio::time::Instant>,
    ) -> bool {
        Home::new(&self.root)
            .standing(session)
            .ok()
            .flatten()
            .is_some_and(|standing| standing.leased.is_some() || standing.pending)
    }
}

/// `held`: Hermes's gateway runs a turn of the conversation while its lease says so, and
/// will while it has words admitted and not run. Either is a client's turn that only that
/// client can cancel, so beyond Steery. With neither, nobody runs one.
pub fn held(root: &Path, held: &Held) -> Result<HeldBy, Error> {
    let Some(standing) = Home::new(root).standing(&held.external_id)? else {
        return Ok(HeldBy::Holder(Holder::Nobody {
            proof: "Hermes has no such conversation".into(),
        }));
    };
    let line = match (&standing.leased, standing.pending) {
        (Some(client), _) if !client.is_empty() => {
            format!("Hermes's gateway runs a turn of it for its {client} client")
        }
        (Some(_), _) => "Hermes's gateway runs a turn of it".to_owned(),
        (None, true) => "Hermes's gateway holds words for it that it has not run".to_owned(),
        (None, false) => {
            return Ok(HeldBy::Holder(Holder::Nobody {
                proof: "Hermes's turn leases and admissions (state.db) name no turn of it".into(),
            }));
        }
    };
    Ok(HeldBy::Holder(Holder::Person {
        door: None,
        stop: HolderStop::Beyond(line),
    }))
}

/// `idle`: running while Hermes's marks say a turn of it runs or waits; else how its last
/// turn ended, which stands while nobody holds it.
pub fn idle(root: &Path, held: &Held) -> Sampled {
    let home = Home::new(root);
    Sampled {
        sample: home
            .standing(&held.external_id)
            .ok()
            .flatten()
            .and_then(|standing| {
                let events = home.events(&standing.external_id).ok()?;
                sample(&events, &standing, now())
            }),
    }
}

/// Where a conversation stands by its events. With nobody holding it, a turn that began and
/// did not end was cut off.
fn sample(events: &[Event], standing: &Standing, now: u64) -> Option<Sample> {
    let mut model = None;
    let mut began = None;
    let mut ended = None;
    for event in events {
        match event {
            Event::Model { model: named, .. } => model = Some(named.clone()),
            Event::TurnBegan { at, .. } => {
                began = Some(*at);
                ended = None;
            }
            Event::TurnEnded {
                at, state, failure, ..
            } => ended = Some((*at, *state, failure.clone())),
            _ => {}
        }
    }
    let mut sample = Sample {
        state: Execution::Running,
        observed_at: now,
        started_at: began,
        ended_at: None,
        model,
        thinking: None,
        error: None,
        vacant: false,
        if_unheld: false,
    };
    if standing.leased.is_some() || standing.pending {
        return Some(sample);
    }
    began?;
    sample.vacant = true;
    sample.if_unheld = true;
    sample.state = match ended {
        None => Execution::Interrupted,
        Some((at, state, failure)) => {
            sample.ended_at = Some(at);
            match state {
                TurnState::Finished => Execution::Finished,
                TurnState::Interrupted => Execution::Interrupted,
                TurnState::Failed => {
                    sample.error = failure.map(|failure| failure.line);
                    Execution::Failed
                }
            }
        }
    };
    Some(sample)
}

/// `models`: the models of the session that `hermes acp` opens, its current model first,
/// each with the thinking levels Hermes offers (the same for every model). The session is
/// deleted again with `hermes sessions delete`, as Hermes's ACP has no delete. Where Hermes
/// has no provider, it lists nothing and says so.
pub async fn models(root: &Path) -> Result<Models, Error> {
    let hermes = super::setup::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "Hermes is not installed"))?;
    let watch = vec![
        root.join("config.yaml"),
        root.join("auth.json"),
        root.join(".env"),
    ];
    let opened = tokio::time::timeout(LISTED_WITHIN, open(&hermes))
        .await
        .map_err(|_| {
            Error::refused(
                refusal::REFUSED,
                format!(
                    "`hermes acp` opened no session within {} s",
                    LISTED_WITHIN.as_secs()
                ),
            )
        })??;
    let Some(opened) = opened else {
        return Ok(Models {
            models: Vec::new(),
            unlisted: None,
            watch,
            signed_in: Some(false),
        });
    };
    if let Some(error) = opened.get("error") {
        let said = error
            .pointer("/data/details")
            .or(error.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("refused");
        return Err(Error::refused(
            refusal::REFUSED,
            format!("`hermes acp` opened no session: {said}"),
        ));
    }
    let session = &opened["result"];
    if let Some(id) = session["sessionId"].as_str() {
        delete(&hermes, id).await?;
    }
    Ok(Models {
        models: listed(session),
        unlisted: None,
        watch,
        signed_in: Some(true),
    })
}

/// Start `hermes acp` in a folder of its own, initialize it and ask for a new session:
/// Hermes's answer, or none where it offers no provider to sign in with.
async fn open(hermes: &Path) -> Result<Option<Value>, Error> {
    let cwd = std::env::temp_dir().join(format!("steery-hermes-models-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).map_err(failed)?;
    let mut child = tokio::process::Command::new(hermes)
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
    let signed_in = initialized
        .pointer("/result/authMethods")
        .and_then(Value::as_array)
        .is_none_or(|methods| methods.iter().any(|method| method["id"] != SETUP));
    let answer = match (initialized.get("error"), signed_in) {
        (Some(_), _) => Some(initialized),
        (None, false) => None,
        (None, true) => Some(
            ask(
                &mut stdin,
                &mut lines,
                2,
                "session/new",
                json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}),
            )
            .await?,
        ),
    };
    drop(stdin);
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&cwd);
    Ok(answer)
}

/// Delete the session `models` opened, through Hermes's own command.
async fn delete(hermes: &Path, id: &str) -> Result<(), Error> {
    let output = tokio::process::Command::new(hermes)
        .args(["sessions", "delete", "--yes", id])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(failed)?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::refused(
        refusal::REFUSED,
        format!(
            "`hermes sessions delete` kept the session it listed models in: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    ))
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
                "`hermes acp` closed before it answered",
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
    Error::refused(refusal::REFUSED, format!("`hermes acp` did not run: {e}"))
}

/// The models of a session's answer: the options of its `model` config option, as Hermes
/// spells each (`provider:model`), its `currentValue` first, each with the session's
/// `reasoning_effort` levels but `default`, which is no level.
fn listed(session: &Value) -> Vec<Model> {
    let options = &session["configOptions"];
    let thinking: Vec<String> = values(option(options, THINKING))
        .into_iter()
        .map(|(value, _)| value)
        .filter(|value| value != UNSET)
        .collect();
    let model = option(options, MODEL);
    let current = model.and_then(|model| model["currentValue"].as_str());
    let mut models: Vec<Model> = values(model)
        .into_iter()
        .map(|(model, label)| Model {
            thinking: thinking.clone(),
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

/// `start`: the launch plan of a turn ([`shared::launch`]). Hermes's ACP server resumes or
/// forks the very session it is given, in the folder it is given, so a turn goes on from
/// the chain's newest session; resuming an id it does not have, it starts a new
/// conversation instead (`acp_adapter/server.py:684-687`), so an unknown one is refused
/// here. A model or thinking level its last list (`listed`) does not have is refused
/// before anything runs.
pub fn start(
    root: &Path,
    listed: &[Model],
    signed_out: bool,
    mut start: Start,
) -> Result<Launch, Error> {
    if signed_out && start.act != Act::Hand {
        return Err(Error::refused(refusal::REFUSED, SIGNED_OUT));
    }
    shared::offered(
        "Hermes",
        listed,
        start.model.as_deref(),
        start.thinking.as_deref(),
    )?;
    if let Some(id) = &start.external_id
        && matches!(start.act, Act::Continue | Act::Fork)
    {
        let standing = Home::new(root).standing(id)?.ok_or_else(|| {
            Error::refused(refusal::NOT_FOUND, format!("no Hermes conversation {id}"))
        })?;
        start.external_id = Some(standing.tip);
    }
    let hook = shared::hook(&super::setup::hook_file(root));
    shared::launch(
        "Hermes",
        super::setup::binary().as_deref(),
        start,
        |_| Ok(None),
        hook,
        None,
        BTreeMap::new(),
    )
}

/// `receipt/mark`: the conversation's newest row.
pub fn mark(root: &Path, mark: &ReceiptMark) -> Result<Marked, Error> {
    Ok(Marked {
        mark: Home::new(root).last(&mark.external_id)?.to_string(),
    })
}

/// `receipt/find`: whether exactly one of the person's rows after the mark carries the
/// marker.
pub fn find(root: &Path, find: &ReceiptFind) -> Result<Receipt, Error> {
    let after = find
        .mark
        .parse()
        .map_err(|_| Error::invalid(format!("mark {} is not a row of Hermes's", find.mark)))?;
    let said = Home::new(root).said_after(&find.external_id, after)?;
    shared::find(&said, &find.delivery)
}

/// `terminal`: Hermes's own chat on the conversation's newest session, in the folder it
/// ran in.
pub fn terminal(root: &Path, terminal: &Terminal) -> Result<TerminalCommand, Error> {
    let standing = Home::new(root).standing(&terminal.external_id)?;
    let cwd = standing
        .as_ref()
        .and_then(|standing| standing.cwd.clone())
        .unwrap_or_else(|| terminal.cwd.clone());
    let session = standing.map_or_else(|| terminal.external_id.clone(), |standing| standing.tip);
    Ok(TerminalCommand {
        command: format!(
            "cd {} && hermes --resume {}",
            quoted(&cwd.to_string_lossy()),
            quoted(&session)
        ),
        app: None,
        line: None,
    })
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}
