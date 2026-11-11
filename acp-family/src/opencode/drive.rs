//! OpenCode 2 at drive: Steery starts OpenCode's turns itself. A turn is this driver's own
//! binary in its turn mode, speaking ACP to `opencode acp` ([`crate::turn`]), which runs the
//! turn in a private server of its own. OpenCode claims a conversation while a turn of it
//! runs (`session_v2.time_suspended`), in whatever process runs it; a process that dies in a
//! turn leaves its claim behind, so a claim says where to look, not who holds it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    Beyond, Execution, Held, HeldBy, Holder, Launch, Look, Looked, Marked, Model, Models, Receipt,
    ReceiptFind, ReceiptMark, Sample, Sampled, Start, Terminal, TerminalCommand,
};
use steery_sdk::{Error, refusal};

use super::place::Place;
use super::store::{Standing, Store};
use crate::turn::drive::{self as shared, now};

/// How long `opencode api model.list` has to list OpenCode's models, with the private
/// server it starts for that.
const LISTED_WITHIN: Duration = Duration::from_secs(30);

/// OpenCode, to a turn this driver runs ([`crate::turn`]): words told mid-turn are steered
/// into the running turn.
pub struct Turns {
    /// OpenCode's database, where the turn has a home to find it in.
    db: Option<PathBuf>,
}

impl Turns {
    /// The database under `$HOME`, as the runtime starts the turn.
    pub fn new() -> Self {
        Self {
            db: std::env::var_os("HOME").map(|home| Place::new(Path::new(&home)).db),
        }
    }
}

impl crate::turn::Host for Turns {
    fn harness(&self) -> crate::Harness {
        crate::Harness::OpenCode
    }

    fn told(&self) -> crate::turn::Told {
        crate::turn::Told::Steer
    }

    /// OpenCode still claims the session, or words wait in its inbox for a turn to take
    /// them: the turn's private server runs the session until it releases the claim.
    fn runs(&self, _acp: Option<u32>, session: &str, _: &mut Option<tokio::time::Instant>) -> bool {
        self.db.as_deref().is_some_and(|db| {
            Store::new(db)
                .standing(session)
                .is_ok_and(|standing| {
                    standing.is_some_and(|standing| standing.claimed.is_some() || standing.queued)
                })
        })
    }
}

/// `held`: with no claim on the conversation, or no such conversation, nobody runs a turn of
/// it. A claim is OpenCode running one, in its background service (which serves every
/// conversation, so the runtime must not signal it) or in a process of its own, or a claim
/// that a dead process left: the runtime looks for who holds the database.
pub fn held(place: &Place, held: &Held) -> Result<HeldBy, Error> {
    let Some(standing) = Store::new(&place.db).standing(&held.external_id)? else {
        return Ok(HeldBy::Holder(Holder::Nobody {
            proof: "OpenCode 2 has no such conversation".into(),
        }));
    };
    if standing.claimed.is_none() {
        return Ok(HeldBy::Holder(Holder::Nobody {
            proof: "OpenCode's claim to run a turn of it (session_v2.time_suspended) is clear"
                .into(),
        }));
    }
    Ok(HeldBy::Look(Looked {
        look: Look {
            transcripts: vec![place.db.clone()],
            strays: false,
            beyond: vec![Beyond {
                arg: Some("--service".into()),
                line: "OpenCode's background service, which runs it".into(),
            }],
        },
    }))
}

/// `idle`: running while OpenCode claims the conversation or words wait in its inbox; else
/// how its last turn ended, which stands while nobody holds it.
pub fn idle(place: &Place, held: &Held) -> Sampled {
    Sampled {
        sample: Store::new(&place.db)
            .standing(&held.external_id)
            .ok()
            .flatten()
            .and_then(|standing| sample(&standing, now())),
    }
}

/// Where a conversation stands. OpenCode writes an `idle` outcome when a turn ends, so with
/// no claim and the person's newest words after the last end, the turn was cut off.
fn sample(standing: &Standing, now: u64) -> Option<Sample> {
    let answer = standing.answer.as_ref();
    let model = answer.and_then(|answer| {
        let model = answer.get("model")?;
        Some(format!(
            "{}/{}",
            model.get("providerID")?.as_str()?,
            model.get("id")?.as_str()?
        ))
    });
    let thinking = answer
        .and_then(|answer| answer.pointer("/model/variant"))
        .and_then(Value::as_str)
        .filter(|variant| *variant != "default")
        .map(str::to_owned);
    let mut sample = Sample {
        state: Execution::Running,
        observed_at: now,
        started_at: standing.claimed.or(standing.asked),
        ended_at: None,
        model,
        thinking,
        error: None,
        vacant: false,
        if_unheld: false,
    };
    if standing.claimed.is_some() || standing.queued {
        return Some(sample);
    }
    sample.started_at = standing.asked;
    sample.vacant = true;
    sample.if_unheld = true;
    let ended = standing
        .idle
        .filter(|idle| standing.asked.is_none_or(|asked| *idle >= asked));
    let Some(ended) = ended else {
        standing.asked?;
        sample.state = Execution::Interrupted;
        return Some(sample);
    };
    sample.ended_at = Some(ended);
    sample.state = match standing.outcome.as_deref() {
        Some("succeeded") => Execution::Finished,
        Some("failed") => {
            sample.error = answer
                .and_then(|answer| answer.get("error"))
                .and_then(|error| {
                    error
                        .pointer("/data/message")
                        .or_else(|| error.get("message"))
                        .or_else(|| error.get("name"))
                })
                .and_then(Value::as_str)
                .map(str::to_owned);
            Execution::Failed
        }
        _ => Execution::Interrupted,
    };
    Some(sample)
}

/// `models`: what OpenCode's `model.list` lists, its default (`model.default`) first. Each
/// asks a private server, so listing never starts the person's background service.
pub async fn models(opencode: Option<&Path>, place: &Place) -> Result<Models, Error> {
    let opencode = opencode
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "OpenCode 2 is not installed"))?;
    let (listed, default) = tokio::join!(
        api(opencode, &place.home, "model.list"),
        api(opencode, &place.home, "model.default"),
    );
    let mut models = self::listed(&listed?);
    let first = default.ok().and_then(|default| {
        let model = default.get("data")?;
        Some(format!(
            "{}/{}",
            model.get("providerID")?.as_str()?,
            model.get("id")?.as_str()?
        ))
    });
    if let Some(at) = first.and_then(|first| models.iter().position(|m| m.model == first)) {
        let first = models.remove(at);
        models.insert(0, first);
    }
    let watch = ["opencode.json", "opencode.jsonc", "config.json"]
        .iter()
        .map(|name| place.config.join(name))
        .collect();
    Ok(Models {
        models,
        unlisted: None,
        watch,
        // Zen's free models need no account, and OpenCode keeps no sign-in of its own.
        signed_in: None,
    })
}

/// One of OpenCode's API operations, answered by a private server started in `cwd`.
async fn api(opencode: &Path, cwd: &Path, operation: &str) -> Result<Value, Error> {
    let said = format!("`opencode api {operation}`");
    let output = tokio::time::timeout(
        LISTED_WITHIN,
        tokio::process::Command::new(opencode)
            .args(["api", operation, "--standalone", "--log-level", "error"])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        Error::refused(
            refusal::REFUSED,
            format!("{said} did not answer within {} s", LISTED_WITHIN.as_secs()),
        )
    })?
    .map_err(|e| Error::refused(refusal::REFUSED, format!("{said} did not start: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let line = stderr.lines().rev().find(|line| !line.trim().is_empty());
        return Err(Error::refused(
            refusal::REFUSED,
            line.map_or_else(|| format!("{said} failed"), str::to_owned),
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| Error::refused(refusal::REFUSED, format!("{said} printed no JSON: {e}")))
}

/// The models of `model.list`: each `<provider>/<id>`, its variants its thinking levels.
fn listed(answer: &Value) -> Vec<Model> {
    answer
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|model| model.get("enabled").and_then(Value::as_bool) != Some(false))
        .filter_map(|model| {
            let provider = model.get("providerID")?.as_str()?;
            let id = model.get("id")?.as_str()?;
            Some(Model {
                model: format!("{provider}/{id}"),
                provider: Some(provider.to_owned()),
                label: model.get("name").and_then(Value::as_str).map(str::to_owned),
                thinking: model
                    .get("variants")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|variant| variant.get("id")?.as_str().map(str::to_owned))
                    .collect(),
                cost: model.pointer("/cost/0/input").and_then(Value::as_f64),
            })
        })
        .collect()
}

/// `start`: the launch plan of a turn ([`shared::launch`]). A model or thinking level that
/// OpenCode's last list (`listed`) does not have is refused before anything runs: OpenCode
/// checks a model only once a session is open, and a new conversation would be left behind
/// empty. The turn's OpenCode writes the database this driver reads, and folders it reads
/// outside its own are allowed it through `OPENCODE_CONFIG_CONTENT`, so OpenCode asks nobody
/// for them.
pub fn start(
    opencode: Option<&Path>,
    place: &Place,
    listed: &[Model],
    start: Start,
) -> Result<Launch, Error> {
    shared::offered(
        "OpenCode",
        listed,
        start.model.as_deref(),
        start.thinking.as_deref(),
    )?;
    let mut env = BTreeMap::from([(
        "OPENCODE_DB".to_owned(),
        place.db.to_string_lossy().into_owned(),
    )]);
    if !start.reads.is_empty() {
        let allowed: Vec<Value> = start
            .reads
            .iter()
            .map(|dir| {
                json!({
                    "action": "external_directory",
                    "resource": dir.join("*").to_string_lossy(),
                    "effect": "allow",
                })
            })
            .collect();
        env.insert(
            "OPENCODE_CONFIG_CONTENT".to_owned(),
            json!({ "permissions": allowed }).to_string(),
        );
    }
    let hook = shared::hook(&super::setup::hook_file(&place.config));
    let store = Store::new(&place.db);
    shared::launch(
        "OpenCode",
        opencode,
        start,
        |id| store.directory(id),
        hook,
        env,
    )
}

/// `receipt/mark`: the `seq` of the conversation's newest message.
pub fn mark(place: &Place, mark: &ReceiptMark) -> Result<Marked, Error> {
    Ok(Marked {
        mark: Store::new(&place.db).newest(&mark.external_id)?,
    })
}

/// `receipt/find`: whether exactly one of the person's words after the mark carries the
/// marker, in a message or still in the inbox.
pub fn find(place: &Place, find: &ReceiptFind) -> Result<Receipt, Error> {
    let said = Store::new(&place.db).said_after(&find.external_id, &find.mark)?;
    shared::find(&said, &find.delivery)
}

/// `terminal`: OpenCode's own window on the conversation, in the folder it ran in.
pub fn terminal(
    opencode: Option<&Path>,
    place: &Place,
    terminal: &Terminal,
) -> Result<TerminalCommand, Error> {
    let cwd = Store::new(&place.db)
        .directory(&terminal.external_id)?
        .unwrap_or_else(|| terminal.cwd.clone());
    // `opencode` on `PATH` can be OpenCode 1, which brew and npm still ship.
    let command = opencode.map_or_else(
        || "opencode".to_owned(),
        |path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''")),
    );
    Ok(shared::terminal(&command, &cwd, &terminal.external_id))
}
