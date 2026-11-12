//! Drive: Droid's own way in, from its command line and its files: who holds a
//! conversation, what its last turn did, how a turn starts and stops, and how a person
//! reopens it.
//!
//! A turn is `droid exec -o stream-json`, run by the runtime itself: the words on standard
//! input, one record on each line of standard output ([`super::turn`]). A continue is
//! `-s <id>`, a fork `--fork <id>`.
//!
//! The turn runs with `--skip-permissions-unsafe`. Under `--auto`, Droid's own gate ends a
//! headless turn at a call its level or its denylist does not allow (1.9.0), so a person's
//! answer would never be asked. Steery's `PreToolUse` hook is the turn's only gate, and
//! `start` refuses without it. A call that hook denies fails, and the turn goes on.

use std::collections::BTreeMap;
use std::path::Path;

use steery_sdk::wire::harness::{
    Act, Held, HeldBy, Launch, Look, Looked, Marked, Read, Receipt, ReceiptFind, ReceiptMark,
    Sampled, Start, Stdin, Stop, Stopped, Terminal, TerminalCommand,
};
use steery_sdk::{Error, refusal};

use super::{EXECUTABLE, models, setup, store::Store};
use crate::turn::drive as shared;

const LABEL: &str = "Droid";

/// `start`: the plan of a turn that the runtime runs as Droid itself. A continue or a fork
/// names a conversation Droid keeps here (Droid opens a new one under an unknown id); a
/// model and a thinking level are ones that Droid's help lists. Droid reads outside its
/// folder with no flag, so `reads` asks for nothing more.
pub async fn start(home: &Path, root: &Path, store: &Store, start: Start) -> Result<Launch, Error> {
    let binary = setup::binary(home)
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "droid is not installed here"))?;
    if !setup::gated(root) {
        return Err(Error::refused(
            refusal::REFUSED,
            format!(
                "Steery's PreToolUse hook is not in {}, and Droid would run every call of the \
                 turn unasked. Set Droid up again",
                root.join("hooks.json").display()
            ),
        ));
    }
    if !start.cwd.is_absolute() {
        return Err(Error::invalid("the folder must be an absolute path"));
    }
    let on = match (start.act, start.external_id.as_deref()) {
        (Act::Continue | Act::Fork, Some(id)) => {
            store.file(id)?;
            if start.act == Act::Continue && !start.cwd.is_dir() {
                return Err(Error::refused(
                    refusal::NO_PLACE,
                    format!(
                        "{} is gone, and Droid resumes the conversation there",
                        start.cwd.display()
                    ),
                ));
            }
            Some((
                if start.act == Act::Fork {
                    "--fork"
                } else {
                    "-s"
                },
                id.to_owned(),
            ))
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
                "Droid does not run a hand over as a turn",
            ));
        }
    };
    if start.worktree {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Steery does not start Droid in a worktree of its own",
        ));
    }
    if start.model.is_some() || start.thinking.is_some() {
        let listed = models::list(home, root).await?;
        shared::offered(
            LABEL,
            &listed.models,
            start.model.as_deref(),
            start.thinking.as_deref(),
        )?;
    }
    let mut argv = vec![
        text(&binary)?,
        "exec".into(),
        "-o".into(),
        "stream-json".into(),
        "--skip-permissions-unsafe".into(),
    ];
    if let Some((flag, id)) = on {
        argv.extend([flag.into(), id]);
    }
    if let Some(model) = start.model {
        argv.extend(["-m".into(), model]);
    }
    if let Some(thinking) = start.thinking {
        argv.extend(["-r".into(), thinking]);
    }
    argv.extend(["--cwd".into(), text(&start.cwd)?]);
    Ok(Launch {
        argv,
        env: BTreeMap::new(),
        cwd: start.cwd,
        stdin: Stdin::Words,
        inbox: false,
    })
}

fn text(path: &Path) -> Result<String, Error> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::invalid(format!("{} is not text", path.display())))
}

/// `held`: where the runtime looks. Droid keeps no registry of its windows, and writes the
/// conversation's file as each record comes, without holding it open, so a `droid` that
/// resumed it may hold it unseen.
pub fn held(store: &Store, held: Held) -> HeldBy {
    HeldBy::Look(Looked {
        look: Look {
            transcripts: store
                .file(&held.external_id)
                .map(|(file, _)| vec![file])
                .unwrap_or_default(),
            strays: true,
            beyond: Vec::new(),
        },
    })
}

/// `idle`: the last turn in the conversation's file where it ended.
pub fn idle(store: &Store, held: Held) -> Result<Sampled, Error> {
    let read = store.read(Read {
        external_id: held.external_id,
        sub_agent: None,
        cursor: None,
        tail: None,
    })?;
    Ok(Sampled {
        sample: shared::sample(&read.events),
    })
}

/// `stop`: Droid has no graceful stop of its own for a headless turn or a window; the
/// runtime's signals end it.
pub fn stop(stop: &Stop) -> Result<Stopped, Error> {
    shared::stop(LABEL, stop)
}

/// `receipt/mark`: no door, so no point to look after.
pub fn mark(_: ReceiptMark) -> Result<Marked, Error> {
    Err(Error::refused(
        refusal::SELECTION_UNSUPPORTED,
        "Droid has no door whose words need a receipt",
    ))
}

/// `receipt/find`: never asked, as `mark` gives no mark.
pub fn find(_: ReceiptFind) -> Result<Receipt, Error> {
    Err(Error::refused(
        refusal::REFUSED,
        "Droid gives no mark to look after",
    ))
}

/// `terminal`: `cd '<cwd>' && droid --resume '<id>'`, in the folder the conversation ran in.
pub fn terminal(store: &Store, terminal: Terminal) -> Result<TerminalCommand, Error> {
    let (_, head) = store.file(&terminal.external_id)?;
    let cwd = head.cwd().unwrap_or(&terminal.cwd);
    Ok(TerminalCommand {
        command: format!(
            "cd {} && {EXECUTABLE} --resume {}",
            quoted(&cwd.to_string_lossy()),
            quoted(&terminal.external_id)
        ),
        app: None,
        line: None,
    })
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}
