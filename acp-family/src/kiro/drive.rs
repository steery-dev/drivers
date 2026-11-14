//! Kiro at drive: Steery starts Kiro's turns itself. A turn is this driver's own binary in
//! its turn mode, speaking ACP to `kiro-cli acp` ([`crate::turn`]), which runs the
//! conversation in the turn's own process. A continue loads the conversation
//! (`session/load`: Kiro 3.3.0's ACP server offers no `session/resume`), a fork is
//! `session/fork`. Kiro asks the turn's client before each shell command
//! (`session/request_permission`), which the turn answers through Steery's hook.
//!
//! Who holds a conversation is Kiro's own lease on it, `~/.kiro/session-leases/<id>.lease`,
//! which names the process that loaded it; a lease outlives its process, so only a live pid
//! that started before the lease was taken holds it.
use std::collections::BTreeMap;
use std::path::Path;

use steery_sdk::wire::harness::{
    Held, HeldBy, Holder, HolderStop, Launch, Marked, Model, Read, Receipt, ReceiptFind,
    ReceiptMark, Sampled, Start, Terminal, TerminalCommand,
};
use steery_sdk::{Error, refusal};

use super::store::Store;
use super::{EXECUTABLE, setup};
use crate::turn::drive as shared;

pub(crate) const LABEL: &str = "Kiro";

/// Kiro, to a turn this driver runs ([`crate::turn`]): words told mid-turn go in as one
/// prompt once Kiro has answered the turn's.
pub struct Turns;

impl crate::turn::Host for Turns {
    fn harness(&self) -> crate::Harness {
        crate::Harness::Kiro
    }

    fn told(&self) -> crate::turn::Told {
        crate::turn::Told::After
    }

    fn opens(&self) -> crate::turn::Opens {
        crate::turn::Opens::Load
    }

    /// Kiro's config option of a model's effort, offered only where the model has one.
    fn thinking(&self) -> &'static str {
        "effortLevel"
    }

    /// Kiro runs one prompt of a session at a time: once it has answered every prompt the
    /// turn sent, the turn's server runs nothing more of it.
    fn runs(
        &self,
        _acp: Option<u32>,
        _session: &str,
        _queued: &[String],
        _: &mut Option<tokio::time::Instant>,
    ) -> bool {
        false
    }
}

/// `start`: the launch plan of a turn ([`shared::launch`]). A model or an effort level that
/// Kiro's list (`listed`) lacks is refused before anything runs: Kiro sets any model it is
/// told, one it does not run too.
pub fn start(
    home: &Path,
    root: &Path,
    store: &Store,
    listed: &[Model],
    start: Start,
) -> Result<Launch, Error> {
    if start.worktree {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Steery does not start Kiro in a worktree of its own",
        ));
    }
    shared::offered(
        LABEL,
        listed,
        start.model.as_deref(),
        start.thinking.as_deref(),
    )?;
    let hook = shared::hook(&setup::hook_file(root));
    shared::launch(
        LABEL,
        setup::binary(home).as_deref(),
        start,
        |id| store.cwd(id),
        hook,
        None,
        BTreeMap::new(),
    )
}

/// `held`: the live process whose lease holds the conversation, else nobody.
pub fn held(store: &Store, held: &Held) -> Result<HeldBy, Error> {
    store.folder(&held.external_id)?;
    Ok(HeldBy::Holder(match store.holder(&held.external_id) {
        Some((pid, process_start)) => Holder::Person {
            door: None,
            stop: HolderStop::Process {
                pid,
                process_start,
                certain: true,
                how: "in Kiro, which loaded it".into(),
            },
        },
        None => Holder::Nobody {
            proof: "no live process holds Kiro's lease on it (session-leases)".into(),
        },
    }))
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

/// `receipt/mark`: how many records the conversation has.
pub fn mark(store: &Store, mark: &ReceiptMark) -> Result<Marked, Error> {
    Ok(Marked {
        mark: store.count(&mark.external_id)?.to_string(),
    })
}

/// `receipt/find`: whether exactly one of the person's words after the mark carries the
/// marker.
pub fn find(store: &Store, find: &ReceiptFind) -> Result<Receipt, Error> {
    shared::find(
        &store.said_after(&find.external_id, &find.mark)?,
        &find.delivery,
    )
}

/// `terminal`: `cd '<cwd>' && kiro-cli chat --resume-id '<id>'`, in the folder the
/// conversation ran in.
pub fn terminal(store: &Store, terminal: &Terminal) -> Result<TerminalCommand, Error> {
    let cwd = store
        .cwd(&terminal.external_id)?
        .unwrap_or_else(|| terminal.cwd.clone());
    Ok(TerminalCommand {
        command: format!(
            "cd {} && {EXECUTABLE} chat --resume-id {}",
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
