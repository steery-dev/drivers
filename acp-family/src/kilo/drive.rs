//! Kilo at drive: Steery starts Kilo's turns itself. A turn is this driver's own binary in
//! its turn mode, speaking ACP to `kilo acp` ([`super::turn`]). Who holds a conversation is
//! what Kilo's own marks of running turns say (`session-turn/` in its state folder), the
//! registry by which one Kilo process tells another that it runs a turn; where a
//! conversation stands is its newest messages in `kilo.db`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use steery_driver_common::kernel;
use steery_driver_common::receipt::{DELIVERED, marked};
use steery_sdk::wire::harness::{
    Act, Ended, Execution, Held, HeldBy, Holder, HolderStop, Launch, Marked, Model, Models,
    Receipt, ReceiptFind, ReceiptMark, Sample, Sampled, Start, Stdin, Stop, Stopped, Stream,
    Terminal, TerminalCommand, TurnLine, TurnRead, TurnState,
};
use steery_sdk::{Error, refusal};

use super::config;
use super::store::{Latest, Store, Turn};

/// How long `kilo models --verbose` has to list Kilo's models.
const LISTED_WITHIN: Duration = Duration::from_secs(25);

/// The updates of a session that only a prompt makes: Kilo answering words.
const ANSWERING: [&str; 5] = [
    "agent_message_chunk",
    "agent_thought_chunk",
    "tool_call",
    "tool_call_update",
    "plan",
];

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

/// A model and thinking level that Kilo's last list has; an empty list checks nothing.
fn offered(listed: &[Model], model: Option<&str>, thinking: Option<&str>) -> Result<(), Error> {
    let (Some(model), false) = (model, listed.is_empty()) else {
        return Ok(());
    };
    let Some(entry) = listed.iter().find(|entry| entry.model == model) else {
        return Err(Error::refused(
            refusal::MODEL_MISSING,
            format!("Kilo lists no model {model}"),
        ));
    };
    match thinking {
        Some(level) if !entry.thinking.iter().any(|known| known == level) => Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("Kilo's {model} has no thinking level {level}"),
        )),
        _ => Ok(()),
    }
}

/// `start`: the launch plan of a turn, which runs this binary in its turn mode speaking to
/// `kilo acp`. Kilo resumes a conversation in the folder it ran in, so a continue whose
/// folder is gone is refused. A model or thinking level that Kilo's last list (`listed`)
/// does not have is refused before anything runs: Kilo checks a model only once a session
/// is open, and a new conversation would be left behind empty. Folders the turn reads
/// outside its own are allowed it through `KILO_CONFIG_CONTENT`, so Kilo asks nobody for
/// them.
pub fn start(
    kilo: Option<&Path>,
    store: &Store,
    listed: &[Model],
    start: Start,
) -> Result<Launch, Error> {
    let kilo = kilo.ok_or_else(|| Error::refused(refusal::NO_BINARY, "Kilo is not installed"))?;
    offered(listed, start.model.as_deref(), start.thinking.as_deref())?;
    let act = match start.act {
        Act::Continue => "continue",
        Act::Fork => "fork",
        Act::New => "new",
        Act::Hand => {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Kilo has no hand-over",
            ));
        }
    };
    if !start.cwd.is_absolute() || !start.folder.is_absolute() {
        return Err(Error::invalid("the folders must be absolute paths"));
    }
    let id = match start.act {
        Act::New => None,
        _ => {
            let id = start.external_id.clone().ok_or_else(|| {
                Error::invalid(format!("a {act} must name the conversation it acts on"))
            })?;
            let directory = store.directory(&id)?;
            if start.act == Act::Continue
                && let Some(directory) = directory
                && !directory.is_dir()
            {
                return Err(Error::refused(
                    refusal::NO_PLACE,
                    format!(
                        "{} is gone, and Kilo resumes the conversation only there",
                        directory.display()
                    ),
                ));
            }
            Some(id)
        }
    };
    let program = std::env::current_exe().map_err(|e| {
        Error::refused(
            refusal::START_FAILED,
            format!("cannot find this driver's own path: {e}"),
        )
    })?;
    let asked = super::turn::Asked {
        kilo: kilo.to_path_buf(),
        act: act.to_owned(),
        id,
        model: start.model,
        thinking: start.thinking,
        inbox: Some(start.folder.join("inbox")),
        // Without setup's hook, a permission Kilo asks for is rejected.
        hook: std::fs::read_to_string(super::setup::hook_file(store.config()))
            .ok()
            .map(|hook| hook.trim().to_owned())
            .filter(|hook| !hook.is_empty()),
    };
    let mut argv = vec![program.to_string_lossy().into_owned(), "turn".to_owned()];
    argv.extend(asked.args());
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
    Ok(Launch {
        argv,
        env,
        cwd: start.cwd,
        stdin: Stdin::Words,
        inbox: true,
    })
}

/// `stop`: Kilo has no way to end a turn from outside the process that runs it. A turn of
/// this driver's ends on the runtime's signal, which it passes to Kilo as a cancel.
pub fn stop(stop: &Stop) -> Result<Stopped, Error> {
    if stop.say.is_some() {
        return Err(Error::refused(
            refusal::OTHER_TURN,
            "Kilo's driver cannot tell which words began the running turn",
        ));
    }
    Ok(Stopped {
        stopped: false,
        proof: Some("Kilo has no way to end a turn from outside its process".into()),
    })
}

/// `receipt/mark`: the conversation's newest message; Kilo's message ids grow with time.
pub fn mark(store: &Store, mark: &ReceiptMark) -> Result<Marked, Error> {
    Ok(Marked {
        mark: store.newest(&mark.external_id)?,
    })
}

/// `receipt/find`: whether exactly one person message after the mark carries the marker.
pub fn find(store: &Store, find: &ReceiptFind) -> Result<Receipt, Error> {
    let marker = format!("{DELIVERED}{}", find.delivery);
    let found = store
        .said_after(&find.external_id, &find.mark)?
        .iter()
        .filter(|words| marked(words, &marker))
        .count();
    if found > 1 {
        return Err(Error::refused(
            refusal::REFUSED,
            "two inputs carry the delivery marker",
        ));
    }
    Ok(Receipt {
        found: found == 1,
        owner: None,
    })
}

/// `terminal`: Kilo's own window on the conversation, in the folder it ran in.
pub fn terminal(store: &Store, terminal: &Terminal) -> Result<TerminalCommand, Error> {
    let cwd = store
        .directory(&terminal.external_id)?
        .unwrap_or_else(|| terminal.cwd.clone());
    Ok(TerminalCommand {
        command: format!(
            "cd {} && kilo --session {}",
            quoted(&cwd.to_string_lossy()),
            quoted(&terminal.external_id)
        ),
        app: None,
        line: None,
    })
}

/// `turn/read`: a line of a turn's output. Standard output is ACP: the session is named
/// by Kilo's answer that opens it or by the prompt sent into it, and taken once Kilo
/// answers the prompt; Kilo's answer to a prompt ends the turn. Standard error is the turn
/// mode's one line, `<refusal word>: <why>`.
pub fn turn_line(read: TurnRead) -> TurnLine {
    match read.stream {
        Stream::Stderr => TurnLine {
            refused: read
                .line
                .split_once(':')
                .map(|(word, _)| word.trim())
                .filter(|word| refusal::ALL.contains(word))
                .map(str::to_owned)
                .or_else(|| (!read.line.trim().is_empty()).then(|| refusal::REFUSED.to_owned())),
            ..TurnLine::default()
        },
        Stream::Stdout => {
            let Ok(line) = serde_json::from_str::<Value>(&read.line) else {
                return TurnLine::default();
            };
            let method = line.get("method").and_then(Value::as_str);
            let answer = method.is_none() && line.get("id").is_some();
            let named = if answer {
                line.pointer("/result/sessionId")
            } else if method == Some("session/prompt") {
                line.pointer("/params/sessionId")
            } else {
                None
            }
            .and_then(Value::as_str)
            .map(str::to_owned);
            let update = (method == Some("session/update"))
                .then(|| line.pointer("/params/update"))
                .flatten();
            let kind = update
                .and_then(|update| update.get("sessionUpdate"))
                .and_then(Value::as_str);
            let ended = answer.then(|| ended(&line)).flatten();
            let took = read.delivery.as_ref().and_then(|_| {
                if kind.is_some_and(|kind| ANSWERING.contains(&kind))
                    || ended
                        .as_ref()
                        .is_some_and(|ended| ended.state != TurnState::Failed)
                {
                    Some(true)
                } else {
                    named.is_some().then_some(false)
                }
            });
            TurnLine {
                named,
                took,
                ended,
                refused: answer
                    .then(|| line.get("error"))
                    .flatten()
                    .map(|error| super::turn::word(error).to_owned()),
                events: Vec::new(),
                title: (kind == Some("session_info_update"))
                    .then(|| update?.get("title")?.as_str().map(str::to_owned))
                    .flatten(),
            }
        }
    }
}

/// How Kilo's answer ends the turn: a prompt's `stopReason`, or any refusal.
fn ended(answer: &Value) -> Option<Ended> {
    if let Some(error) = answer.get("error") {
        return Some(Ended {
            state: TurnState::Failed,
            error: error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    let reason = answer.pointer("/result/stopReason")?.as_str()?;
    Some(match reason {
        "end_turn" => Ended {
            state: TurnState::Finished,
            error: None,
        },
        "cancelled" => Ended {
            state: TurnState::Interrupted,
            error: None,
        },
        other => Ended {
            state: TurnState::Failed,
            error: Some(format!("Kilo stopped the turn: {other}")),
        },
    })
}

/// Now, in milliseconds since 1970.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}
