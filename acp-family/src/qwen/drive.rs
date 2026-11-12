//! Qwen Code's own way in, from its command line and its chat files: who holds a
//! conversation, what its last turn did, how a turn starts and stops, how a person reopens
//! it, and its one-shot reader.
//!
//! A turn is `qwen -o stream-json --approval-mode yolo`, run by the runtime itself: the
//! words on standard input, which Qwen reads as the prompt where it gets no `-p`, and one
//! Claude Code stream-json record on each line of standard output ([`super::turn`]).
//!
//! `yolo` runs every call unasked, so Steery's `PreToolUse` hook is the turn's one gate:
//! setup writes it guarded ([`super::hooks::command`]), and `start` refuses where the
//! settings Qwen loads in the folder lack it ([`super::setup::gated`]). Qwen keeps no lock
//! and no pid of a running conversation, so who holds one is for the runtime to look for.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use steery_sdk::wire::harness::{
    Act, Beyond, Event, Execution, Held, HeldBy, Holder, HolderStop, Launch, Look, Looked, Marked,
    Read, ReadAs, Reader, ReaderPlan, ReaderTools, Receipt, ReceiptFind, ReceiptMark, Sample,
    Sampled, Start, Stdin, Terminal, TerminalCommand, TurnState,
};
use steery_sdk::{Error, refusal};

use super::{models, store::Store};

/// How much of a chat file's end `idle` reads for its last turn.
const IDLE_TAIL: u64 = 512 * 1024;

/// The built-in tools of a reader that looks.
const READ_TOOLS: &str = "read_file,grep_search,glob";

/// The tools a reader never runs: those that start an agent, a skill or a worktree, or
/// write Qwen's own records. `--core-tools` does not hold them back (0.27.2).
const READER_EXCLUDES: &str = "agent,skill,tool_search,tool_call,list_agents,task_stop,\
                               enter_worktree,exit_worktree,record_artifact,get_goal,\
                               update_goal,send_message,report_findings";

fn not_installed() -> Error {
    Error::refused(refusal::NO_BINARY, "qwen is not installed here")
}

/// `start`: the plan of a turn that the runtime runs as Qwen itself, the words on standard
/// input. A continue or a fork names a conversation Qwen keeps here; a model is one that
/// `models` lists. Qwen has no thinking level a run can pick.
pub async fn start(root: &Path, store: &Store, start: Start) -> Result<Launch, Error> {
    let binary = super::binary().ok_or_else(not_installed)?;
    if !start.cwd.is_absolute() {
        return Err(Error::invalid("the folder is not an absolute path"));
    }
    if !super::setup::gated(root, &start.cwd) {
        return Err(Error::refused(
            refusal::REFUSED,
            format!(
                "Steery's PreToolUse hook is not in the settings Qwen Code loads in {}, and \
                 Qwen would run every call of the turn unasked. Set Qwen Code up again",
                start.cwd.display()
            ),
        ));
    }
    let resume = match (start.act, start.external_id.as_deref()) {
        (Act::Continue | Act::Fork, Some(id)) => {
            store.find(id)?;
            Some(id.to_owned())
        }
        (Act::Continue | Act::Fork, None) => {
            return Err(Error::invalid(
                "a continue or a fork must name its conversation",
            ));
        }
        (Act::New, _) => None,
        (Act::Hand, _) => {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Qwen Code does not run a hand over as a turn",
            ));
        }
    };
    if start.worktree {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Steery does not start Qwen Code in a worktree of its own",
        ));
    }
    thinking(start.thinking.as_deref())?;
    if let Some(model) = &start.model {
        let listed = models::list(root).await.map_err(|_| {
            Error::refused(
                refusal::MODEL_MISSING,
                format!("Qwen Code listed no models here, so it cannot run {model}"),
            )
        })?;
        if !listed.models.iter().any(|listed| &listed.model == model) {
            return Err(Error::refused(
                refusal::MODEL_MISSING,
                format!("Qwen Code does not run {model} here"),
            ));
        }
    }
    let mut argv = vec![
        text(&binary)?,
        "-o".into(),
        "stream-json".into(),
        "--approval-mode".into(),
        "yolo".into(),
    ];
    if let Some(id) = resume {
        argv.extend(["-r".into(), id]);
        if start.act == Act::Fork {
            argv.push("--fork-session".into());
        }
    }
    if let Some(model) = start.model {
        argv.extend(["-m".into(), model]);
    }
    for read in &start.reads {
        argv.extend(["--add-dir".into(), text(read)?]);
    }
    Ok(Launch {
        argv,
        env: BTreeMap::new(),
        cwd: start.cwd,
        stdin: Stdin::Words,
        inbox: false,
    })
}

/// Qwen picks how hard its model thinks from its settings alone.
fn thinking(thinking: Option<&str>) -> Result<(), Error> {
    match thinking {
        Some(thinking) => Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("Qwen Code has no thinking level {thinking} a run can pick"),
        )),
        None => Ok(()),
    }
}

fn text(path: &Path) -> Result<String, Error> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::invalid(format!("{} is not text", path.display())))
}

/// `held`: the runtime looks for the processes that hold or name the conversation's chat
/// file. Qwen opens it only to append a record, so a `qwen` that runs the conversation is
/// a stray between its writes. `qwen --acp` serves every conversation its client opens, so
/// stopping it would stop them all.
pub fn held(store: &Store, held: &Held) -> HeldBy {
    let Ok((file, _)) = store.find(&held.external_id) else {
        return HeldBy::Holder(Holder::Nobody {
            proof: "Qwen Code has no such conversation".into(),
        });
    };
    HeldBy::Look(Looked {
        look: Look {
            transcripts: vec![file],
            strays: true,
            beyond: vec![Beyond {
                arg: Some("--acp".into()),
                line: "Qwen Code's ACP server, which serves every conversation its client opens"
                    .into(),
            }],
        },
    })
}

/// `idle`: how the conversation's last turn ended, by the end of its chat file, which
/// stands only while nobody holds it; nothing where the turn did not end, as a run that
/// died leaves it open forever.
pub fn idle(store: &Store, held: Held) -> Sampled {
    let read = store.read(Read {
        external_id: held.external_id,
        sub_agent: None,
        cursor: None,
        tail: Some(IDLE_TAIL),
    });
    Sampled {
        sample: read.ok().and_then(|read| sample(&read.events)),
    }
}

/// The sample of the last turn in `events`, where it ended.
fn sample(events: &[Event]) -> Option<Sample> {
    let mut model = None;
    let mut started_at = None;
    let mut ended = None;
    for event in events {
        match event {
            Event::Model { model: named, .. } => model = Some(named.clone()),
            Event::TurnBegan { at, .. } => {
                started_at = Some(*at);
                ended = None;
            }
            Event::TurnEnded {
                at, state, failure, ..
            } => ended = Some((*at, *state, failure.as_ref().map(|f| f.line.clone()))),
            _ => {}
        }
    }
    let (ended_at, state, error) = ended?;
    Some(Sample {
        state: match state {
            TurnState::Finished => Execution::Finished,
            TurnState::Failed => Execution::Failed,
            TurnState::Interrupted => Execution::Interrupted,
        },
        observed_at: now(),
        started_at,
        ended_at: Some(ended_at),
        model,
        thinking: None,
        error,
        vacant: false,
        if_unheld: true,
    })
}

/// Milliseconds since 1970.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `receipt/mark`: no door, so no point to look after.
pub fn mark(_: ReceiptMark) -> Result<Marked, Error> {
    Err(Error::refused(
        refusal::SELECTION_UNSUPPORTED,
        "Qwen Code has no door whose words need a receipt",
    ))
}

/// `receipt/find`: never asked, as `mark` gives no mark.
pub fn find(_: ReceiptFind) -> Result<Receipt, Error> {
    Err(Error::refused(
        refusal::REFUSED,
        "Qwen Code gives no mark to look after",
    ))
}

/// `terminal`: `cd '<cwd>' && qwen -r <id>`, with `QWEN_HOME='<root>'` before qwen where
/// Qwen's root is not its default.
pub fn terminal(
    home: &Path,
    root: &Path,
    store: &Store,
    terminal: Terminal,
) -> Result<TerminalCommand, Error> {
    let id = &terminal.external_id;
    if !plain_id(id) || store.find(id).is_err() {
        return Err(Error::refused(
            refusal::NOT_FOUND,
            format!("Qwen Code keeps no conversation {id} here"),
        ));
    }
    if !terminal.cwd.is_absolute() {
        return Err(Error::invalid("the folder is not an absolute path"));
    }
    let mut command = format!("cd {} && ", quoted(&text(&terminal.cwd)?));
    if root != home.join(".qwen") {
        command.push_str(&format!("QWEN_HOME={} ", quoted(&text(root)?)));
    }
    command.push_str(&format!("qwen -r {id}"));
    let line = match &terminal.holder {
        Some(Holder::Person {
            stop: HolderStop::Process { pid, .. },
            ..
        }) => Some(format!(
            "Qwen Code has it open already (pid {pid}). This command opens it a second time."
        )),
        Some(Holder::Person {
            stop: HolderStop::Beyond(how),
            ..
        }) => Some(how.clone()),
        _ => None,
    };
    Ok(TerminalCommand {
        command,
        app: None,
        line,
    })
}

/// Whether an id is one plain word that a command takes as it is, and not a flag.
pub(super) fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// `reader`: `qwen -o text --chat-recording=false --approval-mode plan`, the words on
/// standard input, on the model given. `--core-tools` allows no built-in tool (or only
/// [`READ_TOOLS`]), `--exclude-tools` holds back [`READER_EXCLUDES`],
/// `--allowed-mcp-server-names ""` starts no MCP server, `--system-prompt` says the
/// instructions, and `--chat-recording=false` saves nothing. The answer is the whole output.
pub fn reader(reader: Reader) -> Result<ReaderPlan, Error> {
    let binary = super::binary().ok_or_else(not_installed)?;
    if !reader.cwd.is_absolute() {
        return Err(Error::invalid(
            "the reader's folder is not an absolute path",
        ));
    }
    thinking(reader.thinking.as_deref())?;
    let mut argv = vec![
        text(&binary)?,
        "-o".into(),
        "text".into(),
        "--chat-recording=false".into(),
        "--approval-mode".into(),
        "plan".into(),
        "--core-tools".into(),
        match reader.tools {
            ReaderTools::None => String::new(),
            ReaderTools::Read => READ_TOOLS.into(),
        },
        "--exclude-tools".into(),
        READER_EXCLUDES.into(),
        "--allowed-mcp-server-names".into(),
        String::new(),
    ];
    if let Some(model) = reader.model {
        argv.extend(["-m".into(), model]);
    }
    if let Some(system) = reader.system {
        argv.extend(["--system-prompt".into(), system]);
    }
    Ok(ReaderPlan {
        argv,
        env: BTreeMap::new(),
        cwd: reader.cwd,
        stdin: Stdin::Words,
        files: Vec::new(),
        read: ReadAs::Whole,
    })
}
