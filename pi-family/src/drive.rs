//! The harness's own way in, from its command line.
//!
//! `<harness> -p --mode json` runs one turn and exits, reading the words on standard input
//! when it is not a terminal. omp continues a conversation by id, `--resume <id>`, and
//! forks one with `--fork <id>`; pi by its transcript, `--session <file>` and
//! `--fork <file>`. A fork starts in the folder the turn starts in. A conversation whose
//! folder is gone neither resumes, and says so. Neither makes a worktree of its own.
//!
//! A run says itself in pi's JSON lines: the `session` line names the conversation before
//! anything is asked of a model, the person's `message_end` is the receipt of the words,
//! and `agent_end` says how the run ended. omp's `--model` resolves loosely, so a pick goes
//! to omp only as one of its own catalog's entries; pi's goes to Steery's plugin
//! (`--steery-model`), which refuses one pi does not run exactly.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use steery_sdk::wire::harness::{
    Act, Ended, Execution, Held, HeldBy, Holder, Launch, Look, Looked, ReadAs, Reader, ReaderFile,
    ReaderPlan, ReaderTools, Sample, Sampled, Start, Stdin, Stream, Terminal, TerminalCommand,
    TurnLine, TurnRead, TurnState,
};
use steery_sdk::{Error, refusal};

use crate::Harness;
use crate::models::{Catalog, PI_SETTINGS};
use crate::transcripts::{self, Store};

/// The file a reader is asked through, in its private folder.
const ASKED: &str = "asked.txt";

/// `held`: neither harness keeps a registry of its own, so the runtime looks for a process
/// holding the transcript open or naming it, and asks the plugin door. Every omp process
/// holds its conversation's transcript open. A pi without Steery's plugin holds nothing
/// open and renames its process `pi`, so a pi no door, turn or `STEERY_STARTED` answers
/// for may hold any conversation: a stray.
pub fn held(store: &Store, held: Held) -> HeldBy {
    HeldBy::Look(Looked {
        look: Look {
            transcripts: store.transcripts(&held.external_id),
            strays: store.harness() == Harness::Pi,
            beyond: Vec::new(),
        },
    })
}

/// `idle`: the harness writes nothing that says a conversation is running but its plugin
/// door, and the runtime reads the door itself. Without one, a conversation on disk is
/// idle exactly where no process holds it (and, in pi, no stray runs).
pub fn idle(store: &Store, held: Held) -> Sampled {
    let observed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|now| u64::try_from(now.as_millis()).ok())
        .unwrap_or_default();
    Sampled {
        sample: (!store.transcripts(&held.external_id).is_empty()).then_some(Sample {
            state: Execution::Finished,
            observed_at,
            started_at: None,
            ended_at: None,
            model: None,
            thinking: None,
            error: None,
            vacant: true,
            if_unheld: true,
        }),
    }
}

/// Steery's plugin's own flags in pi, which pick exactly or refuse.
const STEERY_MODEL: &str = "steery-model";
const STEERY_THINKING: &str = "steery-thinking";

/// `start`: the launch plan of a turn. The words go on standard input.
pub async fn start(store: &Store, catalog: &Catalog, start: Start) -> Result<Launch, Error> {
    let harness = store.harness();
    if start.worktree {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("{} makes no worktree of its own", harness.id()),
        ));
    }
    let binary = harness.need_binary()?;
    let mut argv: Vec<String> = vec![binary.to_string_lossy().into_owned()];
    argv.extend(["-p", "--mode", "json"].map(String::from));
    // omp names a conversation by its id, pi by its transcript.
    let named = |id: &str, file: PathBuf| match harness {
        Harness::Omp => Ok(id.to_owned()),
        Harness::Pi => file
            .into_os_string()
            .into_string()
            .map_err(|_| Error::invalid("the transcript's path is not UTF-8")),
    };
    match (start.act, start.external_id.as_deref()) {
        (Act::Continue, Some(id)) => {
            let file = store.find(id)?;
            if let Some(cwd) = transcripts::head(&file).cwd
                && !cwd.is_dir()
            {
                return Err(Error::refused(
                    refusal::NO_PLACE,
                    format!(
                        "{} is gone, and {} resumes a conversation only there",
                        cwd.display(),
                        harness.id()
                    ),
                ));
            }
            let flag = match harness {
                Harness::Omp => "--resume",
                Harness::Pi => "--session",
            };
            argv.extend([flag.into(), named(id, file)?]);
        }
        (Act::Fork, Some(id)) => {
            let file = store.find(id)?;
            argv.extend(["--fork".into(), named(id, file)?]);
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
                format!("{} takes no handed-over conversation", harness.id()),
            ));
        }
    }
    if harness == Harness::Omp
        && let Some(model) = &start.model
    {
        match catalog.lists(store.root(), model).await {
            Ok(true) => {}
            Ok(false) => {
                return Err(Error::refused(
                    refusal::MODEL_MISSING,
                    format!("omp lists no model {model} here"),
                ));
            }
            Err(why) => return Err(Error::refused(refusal::START_FAILED, why)),
        }
    }
    if let Some(model) = &start.model {
        argv.extend(["--model".into(), model.clone()]);
    }
    if let Some(thinking) = &start.thinking {
        argv.extend(["--thinking".into(), thinking.clone()]);
    }
    if harness == Harness::Pi {
        if let Some(model) = &start.model {
            argv.push(format!("--{STEERY_MODEL}={model}"));
        }
        if let Some(thinking) = &start.thinking {
            argv.push(format!("--{STEERY_THINKING}={thinking}"));
        }
    }
    Ok(Launch {
        argv,
        env: BTreeMap::new(),
        cwd: start.cwd,
        stdin: Stdin::Words,
        inbox: false,
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
    /// The harness took the words.
    took: bool,
}

impl Runs {
    /// `turn/read`: what one line of a run says. Standard output is pi's JSON lines;
    /// standard error is the harness's own words for what it will not run. A turn's words
    /// are taken once; past the line that ended it, the lines the harness writes as it
    /// settles say nothing, and a run started again (`agent_start`) is read on.
    pub fn read(&self, harness: Harness, read: TurnRead) -> TurnLine {
        let line = match read.stream {
            Stream::Stderr => {
                return TurnLine {
                    refused: refused_by(harness, &read.line).map(str::to_owned),
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
            // A turn not held is a new one, which the harness opens with its naming line,
            // or one whose end was answered: settling lines say nothing, and a run started
            // again took its words before.
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
/// (`message_end`, which the harness writes to its transcript as it says it). Words that
/// carry a `delivery` are taken only by a message naming it whole — `aim.<throw>.1` is not
/// `aim.<throw>.10` — so nothing else the run takes in is read as them; words without one
/// by the run's first message of the person's.
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
/// the harness writes as it settles.
fn after_end(line: &Value) -> bool {
    !matches!(
        line.get("type").and_then(Value::as_str),
        Some("agent_start" | "agent_end")
    )
}

/// `agent_end`: the run ran to its end when the model's last word stopped of its own
/// accord, failed when it stopped on an `error`, with the harness's `errorMessage`, and
/// was interrupted on an `aborted` request. A run that goes on is no end: one retried
/// (`willRetry`), and one the harness continues itself (`isTerminal: false`) unless it
/// yielded while work it started runs on, where print mode exits.
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
        Some("aborted") => Some(Ended {
            state: TurnState::Interrupted,
            error: None,
        }),
        _ => Some(Ended {
            state: TurnState::Finished,
            error: None,
        }),
    }
}

/// What the harness will not run, by the words it says it in on standard error. Any
/// other line is no refusal.
fn refused_by(harness: Harness, said: &str) -> Option<&'static str> {
    match harness {
        Harness::Omp => omp_refused(said),
        Harness::Pi => pi_refused(said),
    }
}

/// omp 18.4.12's own lines: a `--resume` conversation whose folder is gone; `Session
/// "<id>" not found.` for an id it has no conversation for; `Model "<pick>" not found`;
/// and no model, a provider it does not know or no sign-in for the model, where the run
/// exits before the words go in.
fn omp_refused(said: &str) -> Option<&'static str> {
    const NO_FOLDER: &str = "belongs to a directory that no longer exists";
    const SESSION: &str = "Session \"";
    const MODEL: &str = "Model \"";
    const NOT_FOUND: &str = "\" not found";
    const NO_MODEL: [&str; 4] = [
        "No models available",
        "Unknown provider \"",
        "No model selected",
        "No API key found for ",
    ];
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

/// pi 1.0's own lines, and Steery's plugin's `steery-refused: <word>[: why]`, which it
/// says before pi looks for a model or a sign-in: a `--session` folder gone, a fork of a
/// file that is not there, a model pi cannot find or finds twice, and a thinking level or
/// a flag of the plugin's it does not take.
fn pi_refused(said: &str) -> Option<&'static str> {
    if let Some(word) = said.trim().strip_prefix("steery-refused: ") {
        let word = word.split(':').next().unwrap_or_default().trim();
        return Some(if word == refusal::MODEL_MISSING {
            refusal::MODEL_MISSING
        } else {
            refusal::SELECTION_UNSUPPORTED
        });
    }
    if said.contains("Stored session working directory does not exist: ") {
        Some(refusal::NO_PLACE)
    } else if said.contains("Cannot fork: source session ") {
        Some(refusal::NOT_FOUND)
    } else if said.contains("\" not found. Use --list-models")
        || said.contains("\" is ambiguous across providers: ")
    {
        Some(refusal::MODEL_MISSING)
    } else if said.contains("Invalid thinking level \"")
        || (said.contains("Unknown option")
            && (said.contains(STEERY_MODEL) || said.contains(STEERY_THINKING)))
    {
        Some(refusal::SELECTION_UNSUPPORTED)
    } else {
        None
    }
}

/// `reader`: the harness as a one-shot reader, on the model it is given, else on its own
/// default. A reader with no tools runs in a private folder and is asked through a file
/// there, named as `@path`; one that reads runs in the place it looks at, which it must
/// leave as it found it, and is asked on its command line. The answer is the whole
/// output.
///
/// omp: everything closed that a flag closes; its MCP servers arrive through discovery,
/// which no switch turns off, and `--no-extensions` keeps out what an extension brings,
/// Steery's plugin among them. pi: no tools, skills, templates, themes or context files,
/// and its MCP and code-mode extensions turned off by a settings file in the private
/// folder; other extensions stay, since a provider may come from one.
pub fn reader(harness: Harness, reader: Reader) -> Result<ReaderPlan, Error> {
    let binary = harness.need_binary()?;
    if !reader.cwd.is_absolute() {
        return Err(Error::invalid("the reader's folder is no absolute path"));
    }
    let mut argv: Vec<String> = vec![binary.to_string_lossy().into_owned(), "--print".into()];
    if let Some(model) = reader.model {
        argv.extend(["--model".into(), model]);
    }
    if let Some(system) = reader.system {
        argv.extend(["--system-prompt".into(), system]);
    }
    let shut: &[&str] = match (harness, reader.tools) {
        (Harness::Omp, ReaderTools::None) => &["--no-tools"],
        // omp's reading tools are `read`, `grep` and `glob`.
        (Harness::Omp, ReaderTools::Read) => &["--tools=read,grep,glob"],
        (Harness::Pi, ReaderTools::None) => &["--no-tools"],
        // pi's are `read`, `grep`, `find` and `ls`.
        (Harness::Pi, ReaderTools::Read) => &["--tools", "read,grep,find,ls", "--no-extensions"],
    };
    argv.extend(shut.iter().map(|flag| (*flag).to_owned()));
    let closed: &[&str] = match harness {
        Harness::Omp => &[
            "--no-lsp",
            "--no-pty",
            "--no-extensions",
            "--no-skills",
            "--no-rules",
            "--no-session",
            "--no-title",
        ],
        Harness::Pi => &[
            "--no-skills",
            "--no-prompt-templates",
            "--no-themes",
            "--no-context-files",
            "--no-session",
        ],
    };
    argv.extend(closed.iter().map(|flag| (*flag).to_owned()));
    let files = match reader.tools {
        ReaderTools::None => {
            match harness {
                Harness::Omp => argv.extend(["--thinking", "off", "--"].map(String::from)),
                Harness::Pi => argv.extend(["--approve", "--thinking", "off"].map(String::from)),
            }
            let asked = reader.cwd.join(ASKED);
            let asked = asked
                .to_str()
                .ok_or_else(|| Error::invalid("the reader's folder is not UTF-8"))?;
            argv.push(format!("@{asked}"));
            let mut files = vec![ReaderFile {
                path: PathBuf::from(ASKED),
                content: reader.asked,
            }];
            if harness == Harness::Pi {
                files.push(ReaderFile {
                    path: PathBuf::from(".pi/settings.json"),
                    content: PI_SETTINGS.into(),
                });
            }
            files
        }
        ReaderTools::Read => {
            if harness == Harness::Pi {
                argv.push("--no-approve".into());
            }
            argv.extend(["--".into(), reader.asked]);
            Vec::new()
        }
    };
    Ok(ReaderPlan {
        argv,
        env: BTreeMap::new(),
        cwd: reader.cwd,
        stdin: Stdin::None,
        files,
        read: ReadAs::Whole,
    })
}

/// `terminal`: `cd '<cwd>' && omp --resume=<id>` or `pi --session <id>`, with
/// `PI_CODING_AGENT_DIR='<root>'` before it where the conversation is filed under another
/// agent folder than the harness's default.
pub fn terminal(home: &Path, store: &Store, terminal: Terminal) -> Result<TerminalCommand, Error> {
    let harness = store.harness();
    let id = &terminal.external_id;
    if !plain_id(id) {
        return Err(Error::refused(
            refusal::NOT_FOUND,
            format!("{} has no conversation {id} here", harness.id()),
        ));
    }
    store.find(id)?;
    if !terminal.cwd.is_absolute() {
        return Err(Error::invalid("the folder is no absolute path"));
    }
    let cwd = terminal
        .cwd
        .to_str()
        .ok_or_else(|| Error::invalid("the folder is not UTF-8"))?;
    let mut command = format!("cd {} && ", quoted(cwd));
    let root = store.root();
    if root != harness.default_root(home) {
        let root = root
            .to_str()
            .ok_or_else(|| Error::invalid("the agent folder is not UTF-8"))?;
        command.push_str(&format!("PI_CODING_AGENT_DIR={} ", quoted(root)));
    }
    command.push_str(&match harness {
        Harness::Omp => format!("omp --resume={id}"),
        Harness::Pi => format!("pi --session {id}"),
    });
    // A second process on one conversation appends to its transcript beside the first.
    let line = matches!(terminal.holder, Some(Holder::Person { .. })).then(|| {
        format!(
            "{0} has it open already; this opens a second {0} writing to the same conversation.",
            harness.id()
        )
    });
    Ok(TerminalCommand {
        command,
        app: None,
        line,
    })
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Whether a conversation's id is one plain word a command takes as it is, and the
/// harness cannot read as a flag.
fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delivery_is_taken_only_whole() {
        let line = |text: &str| {
            serde_json::json!({"type": "message_end",
                "message": {"role": "user", "content": [{"type": "text", "text": text}]}})
        };
        assert!(taken(
            &line("go steery-delivery:aim.t.1 now"),
            Some("aim.t.1")
        ));
        assert!(!taken(
            &line("go steery-delivery:aim.t.10"),
            Some("aim.t.1")
        ));
        assert!(taken(&line("anything"), None));
    }

    #[test]
    fn each_harness_refuses_in_its_own_words() {
        assert_eq!(
            refused_by(Harness::Pi, "steery-refused: model-missing: no such model"),
            Some(refusal::MODEL_MISSING)
        );
        assert_eq!(
            refused_by(Harness::Pi, "Cannot fork: source session /x.jsonl"),
            Some(refusal::NOT_FOUND)
        );
        assert_eq!(
            refused_by(Harness::Omp, "Session \"abc\" not found."),
            Some(refusal::NOT_FOUND)
        );
        assert_eq!(
            refused_by(Harness::Omp, "Cannot fork: source session"),
            None
        );
    }
}
