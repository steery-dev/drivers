//! Drive's reads and steps on a conversation that is not a turn of the runtime's own: who
//! holds it, what can be proved of it now, its graceful stop, the receipt of words, and the
//! command that opens it in a terminal.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use steery_sdk::wire::harness::{
    Event, Execution, Held, HeldBy, Holder, HolderStop, Look, Looked, Marked, Read, Receipt,
    ReceiptFind, ReceiptMark, Sample, Sampled, Stop, Stopped, Terminal, TerminalCommand, TurnState,
};
use steery_sdk::{Error, refusal};

use super::transcripts::Store;
use crate::kernel;

/// The bytes at the end of a transcript that `idle` reads for its last turn.
const IDLE_TAIL: u64 = 256 * 1024;

/// How much later than its lock a holder's process may seem to start: the kernel and the
/// file system round their times apart.
const CLOCKS: u64 = 1_000;

/// `held`: the live process that Copilot's own in-use lock names, `inuse.<pid>.lock` in
/// the conversation's folder, which every Copilot that opens it writes (1.0.108). A process
/// killed without its cleanup leaves its lock behind, so a lock counts only while its
/// process runs and started before it. Where none does, the runtime looks.
pub fn held(store: &Store, held: Held) -> Result<HeldBy, Error> {
    let transcript = store.transcript(&held.external_id)?;
    let folder = transcript.parent().unwrap_or(Path::new("/"));
    if let Some((pid, process_start)) = locked(folder) {
        return Ok(HeldBy::Holder(Holder::Person {
            door: None,
            stop: HolderStop::Process {
                pid,
                process_start,
                certain: true,
                how: "in Copilot".into(),
            },
        }));
    }
    Ok(HeldBy::Look(Looked {
        look: Look {
            transcripts: vec![transcript],
            strays: true,
            beyond: Vec::new(),
        },
    }))
}

/// The live process that an in-use lock in `folder` names, and when it started.
fn locked(folder: &Path) -> Option<(u32, u64)> {
    std::fs::read_dir(folder).ok()?.flatten().find_map(|entry| {
        let name = entry.file_name();
        let pid: u32 = name
            .to_str()?
            .strip_prefix("inuse.")?
            .strip_suffix(".lock")?
            .parse()
            .ok()?;
        let written = millis(entry.metadata().ok()?.modified().ok()?);
        let started = kernel::started(pid)?;
        (started <= written + CLOCKS).then_some((pid, started))
    })
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `idle`: the last turn in the transcript, where it ended; nothing where it did not, as a
/// run that died leaves a turn open forever.
pub fn idle(store: &Store, held: Held) -> Result<Sampled, Error> {
    let read = store.read(Read {
        external_id: held.external_id,
        sub_agent: None,
        cursor: None,
        tail: Some(IDLE_TAIL),
    })?;
    Ok(Sampled {
        sample: sample(&read.events),
    })
}

/// The sample of the last turn in `events`, where it ended.
fn sample(events: &[Event]) -> Option<Sample> {
    let mut model = None;
    let mut started_at = None;
    let mut ended = None;
    for event in events {
        match event {
            Event::Model {
                model: named,
                thinking: effort,
                ..
            } => model = Some((named.clone(), effort.clone())),
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
    let (model, thinking) = model.unzip();
    Some(Sample {
        state: match state {
            TurnState::Finished => Execution::Finished,
            TurnState::Failed => Execution::Failed,
            TurnState::Interrupted => Execution::Interrupted,
        },
        observed_at: millis(SystemTime::now()),
        started_at,
        ended_at: Some(ended_at),
        model,
        thinking: thinking.flatten(),
        error,
        vacant: false,
        if_unheld: true,
    })
}

/// `stop`: Copilot stops a turn gracefully only for the client of its own server that runs
/// it (`session.abort`); a person's terminal or another process's turn has no such stop,
/// and the runtime's signals end it. A stop of Steery's own words cannot tell its turn.
pub async fn stop(_store: &Store, stop: Stop) -> Result<Stopped, Error> {
    if stop.say.is_some() {
        return Err(Error::refused(
            refusal::OTHER_TURN,
            "Copilot cannot tell which words began the turn that runs",
        ));
    }
    Ok(Stopped {
        stopped: false,
        proof: Some(
            "Copilot stops a turn gracefully only for the client that runs it; the runtime's \
             signals end it."
                .into(),
        ),
    })
}

/// `receipt/mark`: no door, so no point to look after.
pub fn mark(_store: &Store, _mark: ReceiptMark) -> Result<Marked, Error> {
    Err(Error::refused(
        refusal::SELECTION_UNSUPPORTED,
        "Copilot has no door whose words need a receipt",
    ))
}

/// `receipt/find`: never asked, as `mark` gives no mark.
pub fn find(_store: &Store, _find: ReceiptFind) -> Result<Receipt, Error> {
    Err(Error::refused(
        refusal::REFUSED,
        "Copilot gives no mark to look after",
    ))
}

/// `terminal`: `cd '<cwd>' && copilot --resume=<id>`, with `COPILOT_HOME='<root>'` before
/// copilot where Copilot's home is not its default.
pub fn terminal(home: &Path, store: &Store, terminal: Terminal) -> Result<TerminalCommand, Error> {
    let id = &terminal.external_id;
    store.transcript(id)?;
    if !plain_id(id) {
        return Err(Error::refused(
            refusal::NOT_FOUND,
            format!("Copilot keeps no conversation `{id}` here"),
        ));
    }
    if !terminal.cwd.is_absolute() {
        return Err(Error::invalid("the folder is not an absolute path"));
    }
    let mut command = format!("cd {} && ", quoted(&text(&terminal.cwd)?));
    if store.root() != home.join(".copilot") {
        command.push_str(&format!("COPILOT_HOME={} ", quoted(&text(store.root())?)));
    }
    command.push_str(&format!("copilot --resume={id}"));
    let line = match &terminal.holder {
        Some(Holder::Person {
            stop: HolderStop::Process { pid, .. },
            ..
        }) => Some(format!(
            "Copilot has it open already (pid {pid}). This command opens it a second time."
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

fn text(path: &Path) -> Result<String, Error> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::invalid(format!("{} is not text", path.display())))
}

/// Whether an id is one plain word that a command takes as it is, and not a flag.
fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}
