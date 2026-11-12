//! Drive: Steery starts Crush's turns as `crush run`, the words on standard input, through
//! this driver's own binary in its turn mode ([`crate::turn`]). `crush run` prints only the
//! answer's text, so the turn mode reads what Crush logs (`-v`), which names the session,
//! and once Crush exits it reads how the turn ended from the project's store.
//!
//! `crush run` approves every permission of its session itself (`app.go`,
//! `AutoApproveSession`), so Steery's `PreToolUse` hook is the turn's only gate, and `start`
//! refuses without it ([`crate::setup::gated`]).
//!
//! The other methods read Crush's stores: where to look for who holds a session, how its
//! last turn ended, the receipt of words, and the command that opens it in a terminal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use steery_driver_common::program::helper;
use steery_driver_common::receipt::{DELIVERED, marked};
use steery_sdk::wire::harness::{
    Act, Ended, Event, Execution, Held, HeldBy, Holder, HolderStop, Launch, Look, Looked, Marked,
    Model, Models, Read, Receipt, ReceiptFind, ReceiptMark, Sample, Sampled, Start, Stdin, Stop,
    Stopped, Stream, Terminal, TerminalCommand, TurnLine, TurnRead, TurnState, Unlisted,
};
use steery_sdk::{Error, refusal};

use crate::home;
use crate::store::Store;
use crate::turn;

/// How long `crush models` has to print its list.
const MODELS_WITHIN: Duration = Duration::from_secs(30);

/// The bytes of a session's end that `idle` reads for its last turn.
const IDLE_TAIL: u64 = 256 * 1024;

/// What Crush's list of models prints after a provider that it cannot run.
const NOT_CONFIGURED: &str = " (not configured)";

/// `models`: what `crush models` shows on a terminal, a tree of each provider with its
/// models, a provider that Crush cannot run marked `(not configured)`. Off a terminal it
/// prints every provider it knows of alike, so the driver runs it on one. The labels,
/// thinking levels and costs come from Crush's own catalog, `providers.json` in its data
/// folder. The model Crush runs first is the one its configuration picks (`models.large`),
/// else the default of the first provider it can run, in its catalog's order
/// (`config/load.go`, `defaultModelSelection`). A project's `crush.json` can add providers
/// that this list, read in the person's home, does not show.
pub async fn models(home: &Path) -> Result<Models, Error> {
    let binary = home::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "crush is not installed here"))?;
    let mut command = helper(&binary);
    command.arg("models").current_dir(home);
    let (status, shown) = steery_driver_common::login::shown(command, MODELS_WITHIN)
        .await
        .map_err(|e| Error::refused(refusal::REFUSED, format!("crush models did not start: {e}")))?
        .ok_or_else(|| {
            Error::refused(
                refusal::REFUSED,
                format!(
                    "crush models did not answer within {} s",
                    MODELS_WITHIN.as_secs()
                ),
            )
        })?;
    if !status.success() {
        return Err(Error::refused(
            refusal::REFUSED,
            failure(shown.lines()).unwrap_or_else(|| format!("crush models exited with {status}")),
        ));
    }
    let data = home::data(home);
    let catalog: Vec<Known> = std::fs::read(data.join("providers.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let chosen = [data.join("crush.json"), home::settings(home)]
        .iter()
        .find_map(|path| large(path));
    let models = ordered(listed(&shown, &catalog), &catalog, chosen);
    Ok(Models {
        signed_in: Some(!models.is_empty()),
        models,
        unlisted: Some(Unlisted {
            thinking: Vec::new(),
        }),
        watch: vec![
            home::settings(home),
            data.join("crush.json"),
            data.join("providers.json"),
        ],
    })
}

/// A provider of Crush's catalog.
#[derive(Deserialize)]
struct Known {
    id: String,
    #[serde(default)]
    default_large_model_id: Option<String>,
    #[serde(default)]
    models: Vec<KnownModel>,
}

#[derive(Deserialize)]
struct KnownModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    pricing: Option<Pricing>,
    #[serde(default)]
    reasoning: Option<Reasoning>,
}

#[derive(Deserialize)]
struct Pricing {
    #[serde(default)]
    input: Option<f64>,
}

#[derive(Deserialize)]
struct Reasoning {
    #[serde(default)]
    effort_levels: Vec<Level>,
}

#[derive(Deserialize)]
struct Level {
    value: String,
}

/// The provider and model that a configuration file picks as Crush's large model.
fn large(path: &Path) -> Option<(String, String)> {
    let config: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let large = config.pointer("/models/large")?;
    Some((
        large.get("provider")?.as_str()?.to_owned(),
        large.get("model")?.as_str()?.to_owned(),
    ))
}

/// Each model of each provider that the tree shows Crush can run, as `<provider>/<id>`,
/// which `crush run -m` takes whatever the id holds.
fn listed(shown: &str, catalog: &[Known]) -> Vec<Model> {
    let mut models = Vec::new();
    let mut provider: Option<&str> = None;
    for line in shown.lines() {
        let line = line.trim_end().trim_start_matches(char::is_control);
        if let Some(name) = branch(line) {
            provider = (!name.ends_with(NOT_CONFIGURED)).then_some(name);
            continue;
        }
        let Some(provider) = provider else {
            continue;
        };
        let Some(id) = line
            .strip_prefix("│   ")
            .or_else(|| line.strip_prefix("    "))
            .and_then(branch)
        else {
            continue;
        };
        let known = catalog
            .iter()
            .find(|known| known.id == provider)
            .and_then(|known| known.models.iter().find(|model| model.id == id));
        models.push(Model {
            model: format!("{provider}/{id}"),
            provider: Some(provider.to_owned()),
            label: known.and_then(|model| model.name.clone()),
            thinking: known
                .and_then(|model| model.reasoning.as_ref())
                .map(|reasoning| {
                    reasoning
                        .effort_levels
                        .iter()
                        .map(|level| level.value.clone())
                        .collect()
                })
                .unwrap_or_default(),
            cost: known
                .and_then(|model| model.pricing.as_ref())
                .and_then(|pricing| pricing.input),
        });
    }
    models
}

/// The name on one branch of the tree.
fn branch(line: &str) -> Option<&str> {
    line.strip_prefix("├── ")
        .or_else(|| line.strip_prefix("└── "))
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// `models` with Crush's first model first: `chosen`, else the default of the first
/// provider of the catalog that the list holds.
fn ordered(
    mut models: Vec<Model>,
    catalog: &[Known],
    chosen: Option<(String, String)>,
) -> Vec<Model> {
    let first = chosen.or_else(|| {
        catalog.iter().find_map(|known| {
            let id = known.default_large_model_id.clone()?;
            models
                .iter()
                .any(|model| model.provider.as_deref() == Some(&known.id))
                .then(|| (known.id.clone(), id))
        })
    });
    if let Some((provider, id)) = first
        && let Some(at) = models
            .iter()
            .position(|model| model.model == format!("{provider}/{id}"))
    {
        let model = models.remove(at);
        models.insert(0, model);
    }
    models
}

/// `start`: the plan of this driver's turn mode, which runs `crush run -q -v` in `cwd`. A
/// continue (`-s`) names a session of the project in `cwd`: Crush looks for it in that
/// project's store only. Crush checks the model and the thinking level itself; the turn
/// mode says its refusal.
pub fn start(home: &Path, start: Start) -> Result<Launch, Error> {
    let binary = home::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "crush is not installed here"))?;
    if !crate::setup::gated(home) {
        return Err(Error::refused(
            refusal::REFUSED,
            format!(
                "Steery's PreToolUse hook is not in {}, and Crush would run every call of the \
                 turn unasked. Set Crush up again",
                home::settings(home).display()
            ),
        ));
    }
    if !start.cwd.is_absolute() {
        return Err(Error::invalid("the folder is not an absolute path"));
    }
    if start.worktree {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Steery does not start Crush in a worktree of its own",
        ));
    }
    let mut run = vec!["run".to_owned(), "-q".into(), "-v".into()];
    match start.act {
        Act::New => {}
        Act::Continue => {
            let id = start
                .external_id
                .ok_or_else(|| Error::invalid("a continue must name its session"))?;
            let place = Store::new(home).place(&id)?;
            if !start.cwd.is_dir() {
                return Err(Error::refused(
                    refusal::NO_PLACE,
                    format!(
                        "{} is gone, and Crush continues the session there",
                        start.cwd.display()
                    ),
                ));
            }
            if place != start.cwd {
                return Err(Error::refused(
                    refusal::NO_PLACE,
                    format!(
                        "Crush keeps session {id} in {}, and continues it only there",
                        place.display()
                    ),
                ));
            }
            run.extend(["-s".into(), id]);
        }
        Act::Fork | Act::Hand => {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Steery starts only new and continued Crush sessions",
            ));
        }
    }
    // `-m` and `--reasoning-effort` hold for this run only: Crush writes neither into its
    // configuration (`app.go`, `OverridePreferredModel`).
    if let Some(model) = start.model {
        run.extend(["-m".into(), model]);
    }
    if let Some(thinking) = start.thinking {
        run.extend(["--reasoning-effort".into(), thinking]);
    }
    let me = std::env::current_exe().map_err(|e| {
        Error::refused(
            refusal::START_FAILED,
            format!("cannot find this driver: {e}"),
        )
    })?;
    let mut argv = vec![
        text_of(me)?,
        turn::MODE.into(),
        text_of(home.to_owned())?,
        text_of(binary)?,
    ];
    argv.extend(run);
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

/// `turn/read`: one line of the turn mode ([`turn::Line`]). It names the session once Crush
/// does, and says how the turn ended once Crush exited; a refusal comes on standard error.
pub fn read(read: &TurnRead) -> TurnLine {
    let mut line = TurnLine::default();
    let Ok(said) = serde_json::from_str::<turn::Line>(&read.line) else {
        return line;
    };
    line.named = said.named;
    line.ended = said.ended;
    line.refused = said.refused.filter(|_| read.stream == Stream::Stderr);
    line
}

/// The message of Crush's own error box in `lines`, as `crush` prints a command's error: a
/// line `ERROR` alone, then the message.
pub fn failure<'a>(lines: impl Iterator<Item = &'a str>) -> Option<String> {
    let lines: Vec<&str> = lines.map(str::trim).collect();
    let at = lines.iter().rposition(|line| *line == "ERROR")?;
    let message = lines[at + 1..]
        .iter()
        .filter(|line| !line.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    (!message.is_empty()).then_some(message)
}

/// The refusal that one of Crush's errors means (0.100.0).
pub fn refusal_of(message: &str) -> Option<&'static str> {
    if message.starts_with("Session not found") {
        Some(refusal::NOT_FOUND)
    } else if message.contains("data directory already in use") {
        Some(refusal::HARNESS_BUSY)
    } else if message.contains("does not support reasoning effort")
        || message.contains("accepted values")
    {
        Some(refusal::SELECTION_UNSUPPORTED)
    } else if message.contains("model \"") && message.contains("not found")
        || message.contains("found in multiple providers")
    {
        Some(refusal::MODEL_MISSING)
    } else {
        None
    }
}

/// `held`: Crush keeps no mark of the session it runs. Every Crush that has a project open,
/// a turn or a person's window, holds the project's store open, so the runtime looks there.
pub fn held(home: &Path, held: Held) -> Result<HeldBy, Error> {
    Ok(HeldBy::Look(Looked {
        look: Look {
            transcripts: vec![Store::new(home).transcript(&held.external_id)?],
            strays: false,
            beyond: Vec::new(),
        },
    }))
}

/// `idle`: how the session's last turn ended, where it did. It stands while nobody holds
/// the store.
pub fn idle(home: &Path, held: Held) -> Result<Sampled, Error> {
    let read = Store::new(home).read(
        Read {
            external_id: held.external_id,
            sub_agent: None,
            cursor: None,
            tail: Some(IDLE_TAIL),
        },
        |_| Vec::new(),
    )?;
    Ok(Sampled {
        sample: last(&read.events).map(|(started_at, ended_at, ended)| Sample {
            state: match ended.state {
                TurnState::Finished => Execution::Finished,
                TurnState::Failed => Execution::Failed,
                TurnState::Interrupted => Execution::Interrupted,
            },
            observed_at: now(),
            started_at,
            ended_at: Some(ended_at),
            model: None,
            thinking: None,
            error: ended.error,
            vacant: false,
            if_unheld: true,
        }),
    })
}

/// The last turn of `events` that ended: when it began, when and how it ended.
pub fn last(events: &[Event]) -> Option<(Option<u64>, u64, Ended)> {
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
            } => {
                ended = Some((
                    *at,
                    Ended {
                        state: *state,
                        error: failure.as_ref().map(|f| f.line.clone()),
                    },
                ));
            }
            _ => {}
        }
    }
    let (at, ended) = ended?;
    Some((started_at, at, ended))
}

/// `stop`: Crush has no way to end a turn from outside its process. The runtime's signals
/// reach the turn mode, which interrupts Crush as Ctrl-C does.
pub fn stop(stop: &Stop) -> Result<Stopped, Error> {
    if stop.say.is_some() {
        return Err(Error::refused(
            refusal::OTHER_TURN,
            "Crush's driver cannot tell which words began the running turn",
        ));
    }
    Ok(Stopped {
        stopped: false,
        proof: Some("Crush has no way to end a turn from outside its process".into()),
    })
}

/// `receipt/mark`: how many times the person spoke in the session.
pub fn mark(home: &Path, mark: ReceiptMark) -> Result<Marked, Error> {
    Ok(Marked {
        mark: Store::new(home).said(&mark.external_id)?.len().to_string(),
    })
}

/// `receipt/find`: whether exactly one of the person's words after the mark carries the
/// delivery's marker.
pub fn find(home: &Path, find: ReceiptFind) -> Result<Receipt, Error> {
    let after: usize = find
        .mark
        .parse()
        .map_err(|_| Error::invalid(format!("mark {} is not a mark of Crush's", find.mark)))?;
    let said = Store::new(home).said(&find.external_id)?;
    let marker = format!("{DELIVERED}{}", find.delivery);
    let found = said
        .get(after..)
        .unwrap_or_default()
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

/// `terminal`: `cd '<project>' && crush --session <id>`, in the project that keeps it.
pub fn terminal(home: &Path, terminal: Terminal) -> Result<TerminalCommand, Error> {
    let id = &terminal.external_id;
    let place = Store::new(home).place(id)?;
    let line = match &terminal.holder {
        Some(Holder::Person {
            stop: HolderStop::Process { pid, .. },
            ..
        }) => Some(format!(
            "Crush has this project open already (pid {pid}). A second Crush cannot open it \
             until that one quits."
        )),
        Some(Holder::Person {
            stop: HolderStop::Beyond(how),
            ..
        }) => Some(how.clone()),
        _ => None,
    };
    Ok(TerminalCommand {
        command: format!(
            "cd {} && crush --session {}",
            quoted(&place.to_string_lossy()),
            quoted(id)
        ),
        app: None,
        line,
    })
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Now, in milliseconds since 1970.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
