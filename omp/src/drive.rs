//! omp's own way in, from its command line.
//!
//! `omp -p --mode json` runs one turn and exits, reading the words on standard input when
//! it is not a terminal. `--resume <id>` continues a conversation — by id, which omp
//! refuses when it has none — and `--fork <id>` starts a second one from it in the folder
//! the turn starts in. A conversation whose folder is gone omp will not resume, and says
//! so; forked, it goes on in the folder given (omp 18.4.4). omp makes no worktree of its
//! own.
//!
//! A run says itself in pi's JSON lines: the `session` line names the conversation before
//! anything is asked of a model, the person's `message_end` is omp's receipt of the words,
//! and `agent_end` says how the run ended. `--model` resolves loosely, so a pick goes to
//! omp only as one of its own catalog's entries.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use steery_sdk::wire::harness::{
    Act, Ended, Feed, Held, HeldBy, Launch, Look, Model, Models, ReadAs, Reader, ReaderPlan,
    ReaderTools, Start, Stdin, Stream, Terminal, TerminalCommand, TurnLine, TurnRead, TurnState,
};
use steery_sdk::{Error, refusal};

use crate::transcripts;

/// How long `omp models` has to answer `models`: omp took over 20 s on a Mac that had
/// just started and was swapping.
const LISTED_WITHIN: Duration = Duration::from_secs(25);
/// How long a `start` waits to learn whether omp lists the model it was told to run.
const A_TURN_WAITS: Duration = Duration::from_secs(8);
/// How long a list answers a `start` without asking omp again.
const KEPT: Duration = Duration::from_secs(600);

/// `held`: omp keeps no registry of its own, so the runtime looks for an omp process
/// holding the transcript open, and asks the plugin door.
pub fn held(root: &Path, held: Held) -> Result<HeldBy, Error> {
    let transcript =
        transcripts::find(root, &held.external_id).ok_or_else(|| missing(&held.external_id))?;
    Ok(HeldBy::Look {
        look: Look {
            transcript,
            executables: vec!["omp".into()],
        },
    })
}

/// omp's catalog as `omp models --json` lists it, and when it was listed last.
#[derive(Default)]
pub struct Catalog {
    kept: Mutex<Option<(Instant, Vec<Model>)>>,
}

impl Catalog {
    /// `models`: every model omp has a sign-in for, as it lists them. omp that will not
    /// list is no list, and says why on standard error.
    pub async fn list(&self) -> Models {
        let models = match self.ask(LISTED_WITHIN).await {
            Ok(models) => models,
            Err(why) => {
                eprintln!("omp models: {why}");
                Vec::new()
            }
        };
        Models {
            models,
            unlisted: None,
        }
    }

    /// Whether omp runs `model` as itself: one of its catalog's entries, which its
    /// `--model` finds exactly. omp resolves anything else loosely and runs what it found
    /// without a word, so a pick it does not list is never handed to it.
    async fn lists(&self, model: &str) -> Result<bool, String> {
        let named = |models: &[Model]| models.iter().any(|offer| offer.model == model);
        {
            let kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, models)) = kept.as_ref()
                && at.elapsed() < KEPT
                && named(models)
            {
                return Ok(true);
            }
        }
        Ok(named(&self.ask(A_TURN_WAITS).await?))
    }

    async fn ask(&self, within: Duration) -> Result<Vec<Model>, String> {
        let binary = crate::binary().ok_or("omp is not on PATH here")?;
        let output = tokio::time::timeout(
            within,
            crate::helper(&binary)
                .args(["models", "--json"])
                .stdout(std::process::Stdio::piped())
                .output(),
        )
        .await
        .map_err(|_| format!("omp did not list its models within {} s", within.as_secs()))?
        .map_err(|e| format!("omp models: {e}"))?;
        if !output.status.success() {
            return Err(format!("omp models exited with {}", output.status));
        }
        let listed: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("omp models said no JSON: {e}"))?;
        let models = offers(&listed)?;
        *self.kept.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Instant::now(), models.clone()));
        Ok(models)
    }
}

/// `{"models": [{provider, id, selector, name, thinking}]}`: each by its selector,
/// `provider/id`, which is what `--model` takes exactly.
fn offers(listed: &Value) -> Result<Vec<Model>, String> {
    let rows = listed
        .get("models")
        .and_then(Value::as_array)
        .ok_or("omp models has no models array")?;
    rows.iter()
        .map(|row| {
            let selector = row
                .get("selector")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or("omp model has no selector")?;
            let label = row
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty() && *name != selector);
            let thinking = match row.get("thinking") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(levels)) => levels
                    .iter()
                    .map(|level| {
                        level
                            .as_str()
                            .filter(|level| !level.is_empty())
                            .map(str::to_owned)
                            .ok_or("omp model has an invalid thinking level")
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                _ => return Err("omp model has invalid thinking levels".to_owned()),
            };
            Ok(Model {
                provider: row
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                model: selector.to_owned(),
                label: label.map(str::to_owned),
                thinking,
            })
        })
        .collect()
}

/// `start`: the launch plan of an omp turn. The words go on standard input.
pub async fn start(root: &Path, catalog: &Catalog, start: Start) -> Result<Launch, Error> {
    let binary = crate::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "omp is not on PATH here"))?;
    let mut argv: Vec<String> = vec![binary.to_string_lossy().into_owned()];
    argv.extend(["-p", "--mode", "json"].map(String::from));
    match (start.act, start.external_id.as_deref()) {
        (Act::Continue, Some(id)) => {
            let file = transcripts::find(root, id).ok_or_else(|| missing(id))?;
            if let Some(cwd) = transcripts::head(&file).cwd
                && !cwd.is_dir()
            {
                return Err(Error::refused(
                    refusal::NO_PLACE,
                    format!(
                        "{} is gone, and omp resumes a conversation only there",
                        cwd.display()
                    ),
                ));
            }
            argv.extend(["--resume".into(), id.into()]);
        }
        (Act::Fork, Some(id)) => {
            transcripts::find(root, id).ok_or_else(|| missing(id))?;
            argv.extend(["--fork".into(), id.into()]);
        }
        (Act::Continue | Act::Fork, None) => {
            return Err(Error::invalid(
                "a continue or a fork names its conversation",
            ));
        }
        (Act::New, _) => {}
        (Act::Hand, _) => {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "omp takes no handed-over conversation",
            ));
        }
    }
    if let Some(model) = &start.model {
        match catalog.lists(model).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(Error::refused(
                    refusal::MODEL_MISSING,
                    format!("omp lists no model {model} here"),
                ));
            }
            Err(why) => return Err(Error::refused(refusal::START_FAILED, why)),
        }
        argv.extend(["--model".into(), model.clone()]);
    }
    if let Some(thinking) = &start.thinking {
        argv.extend(["--thinking".into(), thinking.clone()]);
    }
    Ok(Launch {
        argv,
        env: BTreeMap::new(),
        cwd: start.cwd,
        stdin: Stdin::Words,
    })
}

/// What each turn's lines said so far, by the turn they belong to: a turn is held from
/// its first line until `ended` is answered for it.
#[derive(Default)]
pub struct Runs {
    runs: Mutex<HashMap<String, Run>>,
}

/// One turn's lines so far.
struct Run {
    /// omp took the words.
    took: bool,
}

impl Runs {
    /// `turn/read`: what one line of an omp run says. Standard output is pi's JSON lines;
    /// standard error is omp's own words for what it will not run. A turn's words are
    /// taken once; past the line that ended it, the lines omp writes as it settles say
    /// nothing, and a run omp starts again (`agent_start`) is read on.
    pub fn read(&self, read: TurnRead) -> TurnLine {
        let line = match read.stream {
            Stream::Stderr => {
                return TurnLine {
                    refused: refused_by(&read.line).map(str::to_owned),
                    ..TurnLine::default()
                };
            }
            Stream::Stdout => match serde_json::from_str::<Value>(&read.line) {
                Ok(line) => line,
                Err(_) => return TurnLine::default(),
            },
        };
        let named = named(&line);
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let run = match runs.get_mut(&read.turn) {
            Some(run) => run,
            // A turn not held is a new one, which omp opens with its naming line, or one
            // whose end was answered: settling lines say nothing, and a run started again
            // took its words before.
            None if named.is_none() && after_end(&line) => return TurnLine::default(),
            None => runs.entry(read.turn.clone()).or_insert(Run {
                took: named.is_none(),
            }),
        };
        let took = !run.took && taken(&line, read.delivery.as_deref());
        run.took |= took;
        let ended = end(&line);
        if ended.is_some() {
            runs.remove(&read.turn);
        }
        TurnLine {
            named,
            took: took.then_some(true),
            ended,
            refused: None,
        }
    }
}

/// `{"type":"session","id"}`, the first line of every run, names the conversation.
fn named(line: &Value) -> Option<String> {
    (line.get("type").and_then(Value::as_str) == Some("session"))
        .then(|| line.get("id").and_then(Value::as_str))
        .flatten()
        .map(str::to_owned)
}

/// Steery's marker for the reference it puts in a turn's words.
const DELIVERED: &str = "steery-delivery:";

/// The receipt of the turn's words: the person's message going into the conversation
/// (`message_end`, which omp writes to its transcript as it says it). Words that carry a
/// `delivery` are taken only by a message naming it whole — `aim.<throw>.1` is not
/// `aim.<throw>.10` — so nothing else the run takes in is read as them; words without
/// one by the run's first message of the person's.
fn taken(line: &Value, delivery: Option<&str>) -> bool {
    if line.get("type").and_then(Value::as_str) != Some("message_end") {
        return false;
    }
    let Some(message) = line.get("message") else {
        return false;
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(delivery) = delivery else {
        return true;
    };
    let text = match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return false,
    };
    let marker = format!("{DELIVERED}{delivery}");
    text.match_indices(&marker).any(|(at, _)| {
        text[at + marker.len()..]
            .chars()
            .next()
            .is_none_or(|next| !(next.is_ascii_alphanumeric() || matches!(next, '.' | '-' | '_')))
    })
}

/// Every line after a run's `agent_end` but one that starts or ends the run again: what
/// omp writes as it settles.
fn after_end(line: &Value) -> bool {
    !matches!(
        line.get("type").and_then(Value::as_str),
        Some("agent_start" | "agent_end")
    )
}

/// `agent_end`: the run ran to its end when the model's last word stopped of its own
/// accord, failed when it stopped on an `error`, with omp's `errorMessage`, and did
/// neither on an `aborted` request. A run that goes on is no end: one retried
/// (`willRetry`), and one omp continues itself (`isTerminal: false`) unless it yielded
/// while work it started runs on, where print mode exits.
fn end(line: &Value) -> Option<Ended> {
    if line.get("type").and_then(Value::as_str) != Some("agent_end") {
        return None;
    }
    let flag = |name: &str| line.get(name).and_then(Value::as_bool);
    if flag("willRetry") == Some(true)
        || (flag("isTerminal") == Some(false) && flag("yielded") != Some(true))
    {
        return None;
    }
    let last =
        line.get("messages")
            .and_then(Value::as_array)
            .and_then(|messages| {
                messages.iter().rev().find(|message| {
                    message.get("role").and_then(Value::as_str) == Some("assistant")
                })
            });
    match last
        .and_then(|message| message.get("stopReason"))
        .and_then(Value::as_str)
    {
        Some("error") => Some(Ended {
            state: TurnState::Failed,
            error: last
                .and_then(|message| message.get("errorMessage"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        }),
        Some("aborted") => None,
        _ => Some(Ended {
            state: TurnState::Finished,
            error: None,
        }),
    }
}

/// omp 18.4.12's own lines on standard error for what it will not run.
///
/// The folder a `--resume` conversation ran in is gone.
const NO_FOLDER: &str = "belongs to a directory that no longer exists";
/// `--resume` or `--fork` of an id omp has no conversation for: `Session "<id>" not found.`
const SESSION: &str = "Session \"";
/// A model omp cannot find: `Model "<pick>" not found`.
const MODEL: &str = "Model \"";
const NOT_FOUND: &str = "\" not found";
/// No model at all, a provider omp does not know, and no sign-in for the model: the run
/// exits before the words go in.
const NO_MODEL: [&str; 4] = [
    "No models available",
    "Unknown provider \"",
    "No model selected",
    "No API key found for ",
];

/// What omp will not run, by the words it says it in: a folder gone, a model it cannot
/// run here, and a conversation it has not. Any other line is no refusal.
fn refused_by(said: &str) -> Option<&'static str> {
    let says = |what: &str| said.contains(what) && said.contains(NOT_FOUND);
    if said.contains(NO_FOLDER) {
        Some(refusal::NO_PLACE)
    } else if says(MODEL) || NO_MODEL.iter().any(|line| said.contains(line)) {
        Some(refusal::MODEL_MISSING)
    } else if says(SESSION) {
        Some(refusal::NOT_FOUND)
    } else {
        None
    }
}

/// `reader`: omp as a one-shot reader, everything closed that a flag closes. Its MCP
/// servers arrive through discovery and no switch turns that off. `--no-extensions` keeps
/// out what an extension brings, Steery's plugin among them. The input is a file in the
/// reader's folder, named to omp as `@path`; the answer is the whole output.
pub fn reader(reader: Reader) -> Result<ReaderPlan, Error> {
    let binary = crate::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "omp is not on PATH here"))?;
    let mut argv: Vec<String> = vec![binary.to_string_lossy().into_owned(), "--print".into()];
    match reader.tools {
        // Asked for no model, omp runs on none a reader can count on.
        ReaderTools::None => {
            let model = reader.model.ok_or_else(|| {
                Error::refused(
                    refusal::MODEL_MISSING,
                    "an omp reader runs on the model it is given",
                )
            })?;
            argv.extend(["--model".into(), model]);
            argv.extend(["--system-prompt".into(), reader.system]);
            argv.extend(
                [
                    "--no-tools",
                    "--no-lsp",
                    "--no-pty",
                    "--no-extensions",
                    "--no-skills",
                    "--no-rules",
                    "--no-session",
                    "--no-title",
                    "--thinking",
                    "off",
                ]
                .map(String::from),
            );
        }
        // omp's reading tools are `read`, `grep` and `glob`.
        ReaderTools::Read => {
            if let Some(model) = reader.model {
                argv.extend(["--model".into(), model]);
            }
            argv.extend(["--system-prompt".into(), reader.system]);
            argv.extend(
                [
                    "--tools=read,grep,glob",
                    "--no-lsp",
                    "--no-pty",
                    "--no-extensions",
                    "--no-skills",
                    "--no-rules",
                    "--no-session",
                    "--no-title",
                ]
                .map(String::from),
            );
        }
    }
    Ok(ReaderPlan {
        launch: Launch {
            argv,
            env: BTreeMap::new(),
            cwd: reader.cwd,
            stdin: Stdin::None,
        },
        feed: Feed::File,
        read: ReadAs::Whole,
    })
}

/// `terminal`: `cd '<cwd>' && omp --resume=<id>`, with `PI_CODING_AGENT_DIR='<root>'`
/// before omp where the conversation is filed under another agent folder than omp's
/// default.
pub fn terminal(home: &Path, root: &Path, terminal: Terminal) -> Result<TerminalCommand, Error> {
    let id = &terminal.external_id;
    if !plain_id(id) || transcripts::find(root, id).is_none() {
        return Err(missing(id));
    }
    if !terminal.cwd.is_absolute() {
        return Err(Error::invalid("the folder is no absolute path"));
    }
    let cwd = terminal
        .cwd
        .to_str()
        .ok_or_else(|| Error::invalid("the folder is not UTF-8"))?;
    let mut command = format!("cd {} && ", quoted(cwd));
    if root != crate::default_root(home) {
        let root = root
            .to_str()
            .ok_or_else(|| Error::invalid("omp's agent folder is not UTF-8"))?;
        command.push_str(&format!("PI_CODING_AGENT_DIR={} ", quoted(root)));
    }
    command.push_str(&format!("omp --resume={id}"));
    Ok(TerminalCommand { command })
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Whether a conversation's id is one plain word a command takes as it is, and omp
/// cannot read as a flag.
fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn missing(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("omp has no conversation {id} here"),
    )
}
