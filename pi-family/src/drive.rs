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

use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_driver_common::receipt::{self, DELIVERED, End, marked};
use steery_sdk::wire::harness::{
    Act, Ended, Execution, Held, HeldBy, Holder, Launch, Look, Looked, Marked, ReadAs, Reader,
    ReaderFile, ReaderPlan, ReaderTools, Receipt, ReceiptFind, ReceiptMark, Sample, Sampled, Start,
    Stdin, Stream, Terminal, TerminalCommand, TurnLine, TurnRead, TurnState,
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
    // omp names a conversation by its id, pi by its transcript.
    let named = |id: &str, file: PathBuf| match harness {
        Harness::Omp => Ok(id.to_owned()),
        Harness::Pi => file
            .into_os_string()
            .into_string()
            .map_err(|_| Error::invalid("the transcript's path is not UTF-8")),
    };
    let opened = match (start.act, start.external_id.as_deref()) {
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
            Some([flag.into(), named(id, file)?])
        }
        (Act::Fork, Some(id)) => {
            let file = store.find(id)?;
            Some(["--fork".into(), named(id, file)?])
        }
        (Act::Continue | Act::Fork, None) => {
            return Err(Error::invalid(
                "a continue or a fork names its conversation",
            ));
        }
        (Act::New, _) => None,
        (Act::Hand, _) => {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                format!("{} takes no handed-over conversation", harness.id()),
            ));
        }
    };
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
    let mut argv: Vec<String> = vec![binary.to_string_lossy().into_owned()];
    argv.extend(args(
        harness,
        opened,
        start.model.as_deref(),
        start.thinking.as_deref(),
    ));
    Ok(Launch {
        argv,
        env: BTreeMap::new(),
        cwd: start.cwd,
        stdin: Stdin::Words,
        inbox: false,
    })
}

/// A turn's arguments after the binary: one run in JSON lines, the conversation it opens
/// as the harness names it, and the pick — to pi also as Steery's plugin's own flags.
fn args(
    harness: Harness,
    opened: Option<[String; 2]>,
    model: Option<&str>,
    thinking: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = ["-p", "--mode", "json"].map(String::from).into();
    args.extend(opened.into_iter().flatten());
    if let Some(model) = model {
        args.extend(["--model".into(), model.to_owned()]);
    }
    if let Some(thinking) = thinking {
        args.extend(["--thinking".into(), thinking.to_owned()]);
    }
    if harness == Harness::Pi {
        if let Some(model) = model {
            args.push(format!("--{STEERY_MODEL}={model}"));
        }
        if let Some(thinking) = thinking {
            args.push(format!("--{STEERY_THINKING}={thinking}"));
        }
    }
    args
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
    match delivery {
        None => message.get("role").and_then(Value::as_str) == Some("user"),
        Some(delivery) => says(message, &format!("{DELIVERED}{delivery}")),
    }
}

/// Whether `message` is the person's and its words carry `marker` whole: its text, or one
/// of its text parts.
fn says(message: &Value, marker: &str) -> bool {
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match message.get("content") {
        Some(Value::String(text)) => marked(text, marker),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .any(|text| marked(text, marker)),
        _ => false,
    }
}

/// A point in a conversation's one transcript: the file, its identity and the end of its
/// last whole record.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mark {
    path: PathBuf,
    dev: u64,
    ino: u64,
    bytes: u64,
}

/// `receipt/mark`: where the conversation's one transcript ends now, at its last whole
/// record. Words the plugin door hands over while a turn runs go in at the harness's next
/// tool boundary, often after the door stopped waiting; the transcript says whether they
/// did. No owner apart from the conversation takes them.
pub fn mark(store: &Store, mark: ReceiptMark) -> Result<Marked, Error> {
    let unsupported = |line: String| Error::refused(refusal::SELECTION_UNSUPPORTED, line);
    let path = store.find(&mark.external_id).map_err(|e| match e {
        Error::Refused { line, .. } => unsupported(line.unwrap_or_default()),
        other => other,
    })?;
    let end = receipt::end(&path).map_err(|e| unsupported(format!("{}: {e}", path.display())))?;
    let mark = Mark {
        dev: end.dev,
        ino: end.ino,
        bytes: end.bytes,
        path,
    };
    Ok(Marked {
        mark: serde_json::to_string(&mark).map_err(|e| Error::invalid(e.to_string()))?,
    })
}

/// `receipt/find`: the person's message carrying the delivery marker, among the whole
/// records the transcript gained past the mark. Two carrying it is refused.
pub fn find(find: ReceiptFind) -> Result<Receipt, Error> {
    let mark: Mark = serde_json::from_str(&find.mark)
        .map_err(|_| Error::invalid("the mark is no mark of this driver's"))?;
    let end = End {
        dev: mark.dev,
        ino: mark.ino,
        bytes: mark.bytes,
    };
    let found = receipt::found(&mark.path, &end, &find.delivery, input_carries)
        .map_err(|line| Error::refused(refusal::REFUSED, line))?;
    Ok(Receipt { found, owner: None })
}

/// Whether a transcript record is the person's message whose words carry `marker` whole:
/// `{"type":"message","message":{"role":"user",…}}`, as both harnesses write the words
/// the plugin door or a turn's standard input hands them. Nothing else they write is the
/// person's: custom messages, tool results, compactions.
fn input_carries(record: &Value, marker: &str) -> bool {
    record.get("type").and_then(Value::as_str) == Some("message")
        && record
            .get("message")
            .is_some_and(|message| says(message, marker))
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
    planned(harness, &harness.need_binary()?, reader)
}

/// [`reader`]'s plan for the harness at `binary`.
fn planned(harness: Harness, binary: &Path, reader: Reader) -> Result<ReaderPlan, Error> {
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
    use steery_sdk::wire::harness::HolderStop;

    fn asked(tools: ReaderTools) -> Reader {
        Reader {
            system: Some("you read".into()),
            model: Some("a/model".into()),
            tools,
            cwd: PathBuf::from("/tmp/steery-read"),
            asked: "what is here?".into(),
        }
    }

    /// A reader with no tools is started with its tools, its session and every discovery a
    /// flag closes turned off, on the model it is given, asked through a file in its own
    /// folder; pi's folder settings turn its MCP off.
    #[test]
    fn pi_and_omp_are_started_with_their_tools_and_discovery_off() {
        for harness in [Harness::Pi, Harness::Omp] {
            let plan = planned(harness, Path::new("/bin/h"), asked(ReaderTools::None)).unwrap();
            let argv = &plan.argv;
            for expected in ["--print", "--no-tools", "--no-session", "--system-prompt"] {
                assert!(
                    argv.iter().any(|arg| arg == expected),
                    "{harness:?}: {expected}"
                );
            }
            let model = argv.iter().position(|arg| arg == "--model").unwrap();
            assert_eq!(argv[model + 1], "a/model");
            assert_eq!(argv.last().unwrap(), "@/tmp/steery-read/asked.txt");
            assert_eq!(plan.files[0].path, PathBuf::from(ASKED));
            assert_eq!(plan.files[0].content, "what is here?");
            assert_eq!((plan.stdin, plan.read), (Stdin::None, ReadAs::Whole));
        }
        let pi = planned(Harness::Pi, Path::new("/bin/pi"), asked(ReaderTools::None)).unwrap();
        assert!(
            pi.files
                .iter()
                .any(|file| file.path == Path::new(".pi/settings.json")
                    && file.content.contains("-builtin:mcp"))
        );
    }

    /// A reader that looks at a place reads it and leaves it as it found it: no file is
    /// written there, no conversation is kept, nothing that writes or skips a check is
    /// passed, and the question comes last, after `--`.
    #[test]
    fn a_look_only_reads_and_leaves_no_conversation() {
        for harness in [Harness::Pi, Harness::Omp] {
            let plan = planned(harness, Path::new("/bin/h"), asked(ReaderTools::Read)).unwrap();
            assert!(plan.files.is_empty(), "{harness:?}");
            assert!(
                plan.argv.iter().any(|arg| arg == "--no-session"),
                "{harness:?}"
            );
            assert_eq!(plan.argv[plan.argv.len() - 2..], ["--", "what is here?"]);
            for forbidden in ["--no-tools", "--auto-approve", "--yolo", "--approve"] {
                assert!(
                    !plan.argv.iter().any(|arg| arg == forbidden),
                    "{harness:?}: {forbidden}"
                );
            }
        }
        assert!(
            planned(
                Harness::Omp,
                Path::new("/bin/omp"),
                Reader {
                    cwd: PathBuf::from("relative"),
                    ..asked(ReaderTools::Read)
                }
            )
            .is_err(),
            "a folder that is no absolute path"
        );
    }

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
        // As omp 18.4.4 says a conversation whose folder is gone.
        assert_eq!(
            refused_by(
                Harness::Omp,
                r#"Error: Session "01a0f1ae-71a7-758d-8931-92ecb219b85d" belongs to a directory that no longer exists (/tmp/cwdgone-a); run interactively to move it into the current project."#
            ),
            Some(refusal::NO_PLACE)
        );
    }

    /// A pick goes to the harness as it spells it, the provider in the model's name kept;
    /// pi's also goes to Steery's plugin, which picks exactly or refuses.
    #[test]
    fn a_pick_keeps_its_provider_and_model() {
        let resume = || Some(["--resume".to_owned(), "source".to_owned()]);
        assert_eq!(
            args(Harness::Omp, resume(), Some("provider/model"), Some("high")),
            [
                "-p",
                "--mode",
                "json",
                "--resume",
                "source",
                "--model",
                "provider/model",
                "--thinking",
                "high"
            ]
        );
        assert_eq!(
            args(Harness::Pi, None, Some("provider/model"), Some("high")),
            [
                "-p",
                "--mode",
                "json",
                "--model",
                "provider/model",
                "--thinking",
                "high",
                "--steery-model=provider/model",
                "--steery-thinking=high"
            ]
        );
        assert_eq!(
            args(
                Harness::Omp,
                Some(["--fork".to_owned(), "01a0cd6f".to_owned()]),
                None,
                None
            ),
            ["-p", "--mode", "json", "--fork", "01a0cd6f"]
        );
    }

    /// A run's first line names its conversation; it ends on a terminal `agent_end`, as the
    /// model's last stop says, an error with the harness's own words; nothing else ends it,
    /// and the lines it writes as it settles say nothing.
    #[test]
    fn agent_end_is_the_end_and_an_error_says_what_went_wrong() {
        let runs = Runs::default();
        let read = |turn: &str, line: Value| {
            runs.read(
                Harness::Omp,
                TurnRead {
                    turn: turn.into(),
                    stream: Stream::Stdout,
                    line: line.to_string(),
                    delivery: None,
                },
            )
        };
        let ended = |last: Value| {
            serde_json::json!({"type": "agent_end", "isTerminal": true, "messages": [
                {"role": "user"},
                last,
                {"role": "toolResult"}
            ]})
        };
        let stopped =
            |stop: &str| ended(serde_json::json!({"role": "assistant", "stopReason": stop}));
        let state = |turn: &str, line: Value| read(turn, line).ended.map(|ended| ended.state);
        assert_eq!(
            read(
                "a",
                serde_json::json!({"type": "session", "version": 3, "id": "01a0cd6f", "cwd": "/w"})
            )
            .named
            .as_deref(),
            Some("01a0cd6f")
        );
        assert_eq!(
            read(
                "a",
                serde_json::json!({"type": "turn_end", "message": {"stopReason": "stop"}})
            )
            .ended,
            None
        );
        assert_eq!(
            read(
                "a",
                serde_json::json!({"type": "agent_end", "isTerminal": false})
            )
            .ended,
            None
        );
        assert_eq!(state("a", stopped("stop")), Some(TurnState::Finished));
        assert_eq!(
            read("a", serde_json::json!({"type": "message_end"})),
            TurnLine::default(),
            "a run that ended says nothing as it settles"
        );
        assert_eq!(state("b", stopped("aborted")), Some(TurnState::Interrupted));
        // As omp keeps it, 2026-09-16.
        let refused = r#"402 {"type":"error","error":{"type":"unknown_error","message":"Insufficient Balance"}}"#;
        assert_eq!(
            read(
                "c",
                ended(
                    serde_json::json!({"role": "assistant", "stopReason": "error",
                    "errorStatus": 402, "errorMessage": refused})
                )
            )
            .ended,
            Some(Ended {
                state: TurnState::Failed,
                error: Some(refused.to_owned())
            })
        );
    }

    /// A conversation on disk is idle where nobody holds it, which the runtime looks for
    /// itself; one with no transcript has no sample.
    #[test]
    fn a_transcript_nobody_holds_is_an_idle_conversation() {
        let home = std::env::temp_dir().join(format!("steery-omp-idle-{}", std::process::id()));
        let root = Harness::Omp.default_root(&home);
        let folder = root.join("sessions/--tmp--");
        std::fs::create_dir_all(&folder).unwrap();
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let file = folder.join(format!("2026-09-29T20-00-00-000Z_{id}.jsonl"));
        std::fs::write(
            &file,
            format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"cwd\":\"/tmp\"}}\n"),
        )
        .unwrap();
        let store = Store::new(Harness::Omp, &home, root, None);
        let held = |id: &str| Held {
            external_id: id.into(),
        };
        let HeldBy::Look(Looked { look }) = super::held(&store, held(id)) else {
            panic!("the runtime looks for an omp's holder");
        };
        assert_eq!(look.transcripts, [file]);
        assert!(!look.strays, "every omp holds its transcript open");
        let sample = idle(&store, held(id)).sample.expect("on disk: a sample");
        assert_eq!(sample.state, Execution::Finished);
        assert!(sample.if_unheld && sample.vacant);
        assert!(idle(&store, held("bbbbbbbb")).sample.is_none());
        std::fs::remove_dir_all(home).ok();
    }

    /// What a person runs is what the harness gets, each part one literal word: the folder
    /// entered, the agent folder where it is not the default, the conversation by its own
    /// way in. Only a plain id and an absolute folder make a command.
    #[test]
    fn the_command_runs_literally_in_its_folder_and_configuration() {
        let home = std::env::temp_dir().join(format!("steery-pi-terminal-{}", std::process::id()));
        let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let cwd = home.join("it's $HOME; work");
        let bin = home.join("bin");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        let filed = |root: &Path| {
            let folder = root.join("sessions/--w--");
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(
                folder.join(format!("2026-09-29T20-00-00-000Z_{id}.jsonl")),
                format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"cwd\":\"/w\"}}\n"),
            )
            .unwrap();
        };
        for harness in [Harness::Omp, Harness::Pi] {
            let stand_in = bin.join(harness.id());
            std::fs::write(
                &stand_in,
                "#!/bin/sh\npwd\nprintf '%s\\n' \"${PI_CODING_AGENT_DIR-default}\" \"$@\"\n",
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let moved = home.join("agent's $(touch nope)");
        for (harness, root, args) in [
            (
                Harness::Omp,
                Harness::Omp.default_root(&home),
                vec![format!("--resume={id}")],
            ),
            (Harness::Omp, moved.clone(), vec![format!("--resume={id}")]),
            (
                Harness::Pi,
                moved.clone(),
                vec!["--session".into(), id.into()],
            ),
        ] {
            filed(&root);
            let store = Store::new(harness, &home, root.clone(), None);
            let at = |id: &str, cwd: &Path, holder: Option<Holder>| {
                terminal(
                    &home,
                    &store,
                    Terminal {
                        external_id: id.into(),
                        cwd: cwd.to_owned(),
                        holder,
                    },
                )
            };
            let opened = at(id, &cwd, None).unwrap();
            assert!(opened.app.is_none() && opened.line.is_none());
            let result = std::process::Command::new("/bin/sh")
                .args(["-c", &opened.command])
                .env("PATH", &bin)
                .env_remove("PI_CODING_AGENT_DIR")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let agent = if root == harness.default_root(&home) {
                "default".to_owned()
            } else {
                root.display().to_string()
            };
            let mut expected = vec![cwd.display().to_string(), agent];
            expected.extend(args);
            assert_eq!(
                String::from_utf8(result.stdout).unwrap(),
                format!("{}\n", expected.join("\n")),
                "{harness:?}"
            );
            assert!(!cwd.join("nope").exists());
            let beside = at(
                id,
                &cwd,
                Some(Holder::Person {
                    door: None,
                    stop: HolderStop::Beyond("a writer".into()),
                }),
            )
            .unwrap();
            assert!(beside.line.is_some(), "a second writer is said");
            assert!(at(id, Path::new("relative"), None).is_err());
            for id in ["--fork", "a b", "bbbbbbbb"] {
                assert!(at(id, &cwd, None).is_err(), "{id}");
            }
        }
        std::fs::remove_dir_all(home).ok();
    }
}
