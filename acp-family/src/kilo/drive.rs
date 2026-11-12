//! Kilo at drive: Steery starts Kilo's turns itself. A turn is this driver's own binary in
//! its turn mode, speaking ACP to `kilo acp` ([`crate::turn`]). Who holds a conversation is
//! what Kilo's own marks of running turns say (`session-turn/` in its state folder), the
//! registry by which one Kilo process tells another that it runs a turn; where a
//! conversation stands is its newest messages in `kilo.db`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use steery_driver_common::kernel;
use steery_sdk::wire::harness::{
    Execution, Held, HeldBy, Holder, HolderStop, Launch, Marked, Model, Models, Receipt,
    ReceiptFind, ReceiptMark, Sample, Sampled, Start, Terminal, TerminalCommand,
};
use steery_sdk::{Error, refusal};

use super::config;
use super::store::{Latest, Store, Turn};
use crate::turn::drive::{self as shared, now};

/// How long `kilo models --verbose` has to list Kilo's models.
const LISTED_WITHIN: Duration = Duration::from_secs(25);

/// How long the newest words of a session may wait for an answer with no Kilo mark before
/// the turn takes it that Kilo runs nothing more of it.
const UNANSWERED_WITHIN: Duration = Duration::from_secs(5);

/// Kilo, to a turn this driver runs ([`crate::turn`]): words told mid-turn go in as one
/// more prompt, which Kilo queues behind the one running.
pub struct Turns {
    /// Kilo's database and marks, where the turn has a home to find them in.
    store: Option<Store>,
}

impl Turns {
    /// The store under `$HOME`, as the runtime starts the turn.
    pub fn new() -> Self {
        Self {
            store: std::env::var_os("HOME").map(|home| Store::new(Path::new(&home))),
        }
    }
}

impl crate::turn::Host for Turns {
    fn harness(&self) -> crate::Harness {
        crate::Harness::Kilo
    }

    fn told(&self) -> crate::turn::Told {
        crate::turn::Told::Prompt
    }

    /// Kilo's own mark says this Kilo runs a turn of the session (its ACP server, or a
    /// child of the launcher that `kilo` may be), or the newest words of the session have
    /// no answer yet, as between queued words' turn and the one they took over, for at most
    /// [`UNANSWERED_WITHIN`] with no mark.
    fn runs(
        &self,
        acp: Option<u32>,
        session: &str,
        _queued: &[String],
        unmarked: &mut Option<tokio::time::Instant>,
    ) -> bool {
        let Some(store) = &self.store else {
            return false;
        };
        let marked = match (store.turn(session), acp) {
            (Some(Turn::Here(pid)), Some(kilo)) => {
                pid == kilo
                    || kernel::descendants(kilo)
                        .iter()
                        .any(|(child, _)| *child == pid)
            }
            _ => false,
        };
        if marked {
            *unmarked = None;
            return true;
        }
        let unanswered = store.latest(session).is_ok_and(|latest| {
            let answered = latest
                .answer
                .as_ref()
                .and_then(|answer| answer.pointer("/time/created"))
                .and_then(Value::as_u64);
            latest
                .asked
                .is_some_and(|asked| answered.is_none_or(|at| at < asked))
        });
        unanswered
            && unmarked
                .get_or_insert_with(tokio::time::Instant::now)
                .elapsed()
                < UNANSWERED_WITHIN
    }
}

/// `held`: the Kilo process whose mark says it runs a turn of the conversation. Kilo marks
/// every turn it runs, whatever runs it, so with no live mark nobody runs one.
pub fn held(store: &Store, held: &Held) -> HeldBy {
    HeldBy::Holder(match store.turn(&held.external_id) {
        Some(Turn::Here(pid)) => match kernel::started(pid) {
            Some(process_start) => Holder::Person {
                door: None,
                stop: HolderStop::Process {
                    pid,
                    process_start,
                    certain: true,
                    how: "in Kilo, running a turn".into(),
                },
            },
            None => nobody(),
        },
        Some(Turn::Elsewhere(host)) => Holder::Person {
            door: None,
            stop: HolderStop::Beyond(format!("Kilo on {host} runs a turn of it")),
        },
        None => nobody(),
    })
}

fn nobody() -> Holder {
    Holder::Nobody {
        proof: "Kilo's own marks of the turns it runs (session-turn) name no live turn of it"
            .into(),
    }
}

/// `idle`: running while a Kilo process's mark says so; else how its newest answer ended,
/// which stands while no Kilo runs a turn of it.
pub fn idle(store: &Store, held: &Held) -> Sampled {
    let id = &held.external_id;
    Sampled {
        sample: store
            .latest(id)
            .ok()
            .and_then(|latest| sample(&latest, store.turn(id).is_some(), now())),
    }
}

/// Where a conversation stands by its newest messages; `running` where a mark says a turn
/// of it runs. An answer is Kilo's message for one step of a turn: one that ended on
/// `tool-calls` has a step after it, so with no turn running, the turn was cut off.
fn sample(latest: &Latest, running: bool, now: u64) -> Option<Sample> {
    let answer = latest.answer.as_ref();
    let created = |answer: &Value| answer.pointer("/time/created").and_then(Value::as_u64);
    let model = answer
        .and_then(|answer| {
            Some(format!(
                "{}/{}",
                answer.get("providerID")?.as_str()?,
                answer.get("modelID")?.as_str()?
            ))
        })
        .or_else(|| {
            let model = latest.model.as_ref()?;
            Some(format!(
                "{}/{}",
                model.get("providerID")?.as_str()?,
                model.get("modelID")?.as_str()?
            ))
        });
    let thinking = answer
        .and_then(|answer| answer.get("variant"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut sample = Sample {
        state: Execution::Running,
        observed_at: now,
        started_at: latest.asked,
        ended_at: None,
        model,
        thinking,
        error: None,
        vacant: false,
        if_unheld: false,
    };
    if running {
        return Some(sample);
    }
    sample.vacant = true;
    sample.if_unheld = true;
    let answered = answer.filter(|answer| {
        latest
            .asked
            .is_none_or(|asked| created(answer).is_some_and(|at| at >= asked))
    });
    let Some(answer) = answered else {
        latest.asked?;
        sample.state = Execution::Interrupted;
        return Some(sample);
    };
    let completed = answer.pointer("/time/completed").and_then(Value::as_u64);
    sample.ended_at = completed.or_else(|| created(answer));
    let error = answer.get("error");
    sample.state = match (completed, error) {
        (_, Some(error))
            if error.get("name").and_then(Value::as_str) == Some("MessageAbortedError") =>
        {
            Execution::Interrupted
        }
        (_, Some(error)) => {
            sample.error = error
                .pointer("/data/message")
                .or_else(|| error.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            Execution::Failed
        }
        (Some(_), None) if answer.get("finish").and_then(Value::as_str) != Some("tool-calls") => {
            Execution::Finished
        }
        _ => Execution::Interrupted,
    };
    Some(sample)
}

/// `models`: what `kilo models --verbose` lists, the model Kilo starts a conversation on
/// first: the configuration's `model`, else the newest of its recent picks that it lists.
pub async fn models(kilo: Option<&Path>, store: &Store) -> Result<Models, Error> {
    let kilo = kilo.ok_or_else(|| Error::refused(refusal::NO_BINARY, "Kilo is not installed"))?;
    let output = tokio::time::timeout(
        LISTED_WITHIN,
        tokio::process::Command::new(kilo)
            .args(["--log-level", "ERROR", "models", "--verbose"])
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        Error::refused(
            refusal::REFUSED,
            format!(
                "`kilo models` did not answer within {} s",
                LISTED_WITHIN.as_secs()
            ),
        )
    })?
    .map_err(|e| {
        Error::refused(
            refusal::REFUSED,
            format!("`kilo models` did not start: {e}"),
        )
    })?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        let line = said.lines().rev().find(|line| !line.trim().is_empty());
        return Err(Error::refused(
            refusal::REFUSED,
            line.unwrap_or("`kilo models` failed").to_owned(),
        ));
    }
    let mut models = listed(&String::from_utf8_lossy(&output.stdout));
    let global = config::files(store.config(), None)
        .iter()
        .filter_map(|file| crate::jsonc::jsonc(file))
        .collect::<Vec<_>>();
    let first = configured(&global)
        .or_else(|| recent(store.state(), &models))
        .or_else(|| fallback(&models, &global));
    if let Some(at) = first.and_then(|first| models.iter().position(|m| m.model == first)) {
        let first = models.remove(at);
        models.insert(0, first);
    }
    let mut watch = config::files(store.config(), None);
    watch.push(store.state().join("model.json"));
    watch.push(store.data().join("auth.json"));
    Ok(Models {
        models,
        unlisted: None,
        watch,
        signed_in: None,
    })
}

/// The models of `kilo models --verbose`: each a line of its id, then its JSON, whose
/// `variants` are its thinking levels.
fn listed(output: &str) -> Vec<Model> {
    /// A model as Kilo prints it.
    #[derive(serde::Deserialize)]
    struct Listed {
        #[serde(rename = "providerID")]
        provider_id: String,
        id: String,
        name: Option<String>,
        cost: Option<Cost>,
        #[serde(default)]
        variants: Variants,
    }
    #[derive(serde::Deserialize)]
    struct Cost {
        input: Option<f64>,
    }
    let mut models = Vec::new();
    let mut json = String::new();
    for line in output.lines() {
        if line.starts_with('{') {
            json.clear();
        }
        json.push_str(line);
        json.push('\n');
        if line != "}" {
            continue;
        }
        let Ok(model) = serde_json::from_str::<Listed>(&json) else {
            continue;
        };
        models.push(Model {
            model: format!("{}/{}", model.provider_id, model.id),
            provider: Some(model.provider_id),
            label: model.name,
            thinking: model.variants.0,
            cost: model.cost.and_then(|cost| cost.input),
        });
    }
    models
}

/// The names of a model's variants, in Kilo's order (`low` before `high`).
#[derive(Default)]
struct Variants(Vec<String>);

impl<'de> serde::Deserialize<'de> for Variants {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Names;
        impl<'de> serde::de::Visitor<'de> for Names {
            type Value = Variants;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object of variants")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Variants, A::Error> {
                let mut names = Vec::new();
                while let Some((name, _)) = map.next_entry::<String, serde::de::IgnoredAny>()? {
                    names.push(name);
                }
                Ok(Variants(names))
            }
        }
        deserializer.deserialize_map(Names)
    }
}

/// Kilo's own default where nothing names one (`Provider.defaultModel`, provider.ts): the
/// first provider the configuration's `provider` allows (any, where it names none), and of
/// its models the first by Kilo's `sort`: by the first of `PRIORITY` its id holds, a `latest`
/// before the rest, then by id, last first.
fn fallback(models: &[Model], global: &[Value]) -> Option<String> {
    const PRIORITY: [&str; 4] = ["gpt-5", "claude-sonnet-4", "big-pickle", "gemini-3-pro"];
    let allowed: Vec<&String> = global
        .iter()
        .filter_map(|config| config.get("provider")?.as_object())
        .flat_map(|providers| providers.keys())
        .collect();
    let provider = models
        .iter()
        .filter_map(|model| model.provider.as_ref())
        .find(|provider| allowed.is_empty() || allowed.contains(provider))?;
    let id = |model: &Model| model.model[provider.len() + 1..].to_owned();
    let rank = |id: &str| {
        let priority = PRIORITY.iter().position(|filter| id.contains(filter));
        (
            std::cmp::Reverse(priority.map_or(-1, |at| at as i64)),
            !id.contains("latest"),
        )
    };
    models
        .iter()
        .filter(|model| model.provider.as_ref() == Some(provider))
        .min_by(|a, b| {
            let (a, b) = (id(a), id(b));
            rank(&a).cmp(&rank(&b)).then_with(|| b.cmp(&a))
        })
        .map(|model| model.model.clone())
}

/// The `model` of Kilo's global configuration files: the last that names one wins.
fn configured(global: &[Value]) -> Option<String> {
    global
        .iter()
        .rev()
        .find_map(|config| config.get("model")?.as_str().map(str::to_owned))
}

/// The newest of Kilo's recent picks (`model.json`) that it lists.
fn recent(state: &Path, models: &[Model]) -> Option<String> {
    let text = std::fs::read_to_string(state.join("model.json")).ok()?;
    let picks: Value = serde_json::from_str(&text).ok()?;
    picks
        .get("recent")?
        .as_array()?
        .iter()
        .filter_map(|pick| {
            Some(format!(
                "{}/{}",
                pick.get("providerID")?.as_str()?,
                pick.get("modelID")?.as_str()?
            ))
        })
        .find(|pick| models.iter().any(|model| model.model == *pick))
}

/// `start`: the launch plan of a turn ([`shared::launch`]). A model or thinking level that
/// Kilo's last list (`listed`) does not have is refused before anything runs: Kilo checks a
/// model only once a session is open, and a new conversation would be left behind empty.
/// Folders the turn reads outside its own are allowed it through `KILO_CONFIG_CONTENT`, so
/// Kilo asks nobody for them.
pub fn start(
    kilo: Option<&Path>,
    store: &Store,
    listed: &[Model],
    start: Start,
) -> Result<Launch, Error> {
    shared::offered(
        "Kilo",
        listed,
        start.model.as_deref(),
        start.thinking.as_deref(),
    )?;
    let mut env = BTreeMap::new();
    // The turn's Kilo writes the database this driver reads.
    if let Some(db) = store.moved() {
        env.insert("KILO_DB".to_owned(), db.to_string_lossy().into_owned());
    }
    if !start.reads.is_empty() {
        let allowed: serde_json::Map<String, Value> = start
            .reads
            .iter()
            .map(|dir| (dir.join("*").to_string_lossy().into_owned(), json!("allow")))
            .collect();
        env.insert(
            "KILO_CONFIG_CONTENT".to_owned(),
            json!({"permission": {"external_directory": allowed}}).to_string(),
        );
    }
    let hook = shared::hook(&super::setup::hook_file(store.config()));
    shared::launch(
        "Kilo",
        kilo,
        start,
        |id| store.directory(id),
        hook,
        None,
        env,
    )
}

/// `receipt/mark`: the conversation's newest message; Kilo's message ids grow with time.
pub fn mark(store: &Store, mark: &ReceiptMark) -> Result<Marked, Error> {
    Ok(Marked {
        mark: store.newest(&mark.external_id)?,
    })
}

/// `receipt/find`: whether exactly one person message after the mark carries the marker.
pub fn find(store: &Store, find: &ReceiptFind) -> Result<Receipt, Error> {
    shared::find(
        &store.said_after(&find.external_id, &find.mark)?,
        &find.delivery,
    )
}

/// `terminal`: Kilo's own window on the conversation, in the folder it ran in.
pub fn terminal(store: &Store, terminal: &Terminal) -> Result<TerminalCommand, Error> {
    let cwd = store
        .directory(&terminal.external_id)?
        .unwrap_or_else(|| terminal.cwd.clone());
    Ok(shared::terminal("kilo", &cwd, &terminal.external_id))
}
