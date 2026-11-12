//! OpenClaw at drive: Steery starts OpenClaw's turns itself. A turn is this driver's own
//! binary in its turn mode, speaking ACP to `openclaw acp` ([`crate::turn`]), a bridge onto
//! OpenClaw's gateway, which runs every turn of every client. The bridge keeps each session
//! under a key: it resumes a session by its key (`agent:main:main`), and names a session it
//! makes anew by an id it draws, under the key `<agent>:acp-bridge:<id>`. Steery knows a
//! conversation by its first window's id ([`super::store`]), so a turn goes on under its
//! key and its lines are named back by that id. The bridge has no fork and no model option:
//! the model is set on the key through the gateway's `sessions.patch`. A second prompt
//! cancels the running one, so words told mid-turn wait for its end.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::json;
use steery_sdk::wire::harness::{
    Act, Event, Execution, Held, HeldBy, Holder, HolderStop, Launch, Marked, Model, Receipt,
    ReceiptFind, ReceiptMark, Sample, Sampled, Start, Terminal, TerminalCommand, TurnLine,
    TurnRead, TurnState,
};
use steery_sdk::{Error, refusal};

use super::store::{Standing, Store};
use crate::turn::drive::{self as shared, now};

/// The config option of a session that picks its thinking level
/// (`src/acp/translator.presentation.ts` `ACP_THOUGHT_LEVEL_CONFIG_ID`).
const THINKING: &str = "thought_level";

/// OpenClaw, to a turn this driver runs ([`crate::turn`]).
pub struct Turns {
    root: PathBuf,
}

impl Turns {
    pub fn new() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self {
            root: super::root(&home),
        }
    }
}

impl crate::turn::Host for Turns {
    fn harness(&self) -> crate::Harness {
        crate::Harness::OpenClaw
    }

    fn told(&self) -> crate::turn::Told {
        crate::turn::Told::After
    }

    fn thinking(&self) -> &'static str {
        THINKING
    }

    /// A run that another client of the gateway began on the key writes on after the turn's
    /// own: while the key's node names a writer and no end, the turn reads on.
    fn runs(
        &self,
        _acp: Option<u32>,
        session: &str,
        _queued: &[String],
        _: &mut Option<tokio::time::Instant>,
    ) -> bool {
        Store::new(&self.root).writing(session)
    }

    /// `openclaw gateway call sessions.patch` on the session's key: the bridge's own key for
    /// a session it made, which the gateway reads with its agent's prefix.
    fn sets_model(
        &self,
        binary: &Path,
        session: &str,
        model: &str,
    ) -> Option<tokio::process::Command> {
        let key = match session.contains(':') {
            true => session.to_owned(),
            false => format!("acp-bridge:{session}"),
        };
        let mut command = tokio::process::Command::new(binary);
        command.args([
            "gateway",
            "call",
            "sessions.patch",
            "--json",
            "--params",
            &json!({"key": key, "model": model}).to_string(),
        ]);
        Some(command)
    }
}

/// `held`: OpenClaw's gateway runs a turn of the conversation while its key's node names a
/// writer run with no end. That run is a client's that only that client can cancel, so
/// beyond Steery.
pub fn held(root: &Path, held: &Held) -> Result<HeldBy, Error> {
    let running = Store::new(root)
        .standing(&held.external_id)?
        .is_some_and(|standing| standing.newest && standing.running);
    Ok(HeldBy::Holder(match running {
        true => Holder::Person {
            door: None,
            stop: HolderStop::Beyond("OpenClaw's gateway runs a turn of it".into()),
        },
        false => Holder::Nobody {
            proof: "OpenClaw's session node names no run that writes it".into(),
        },
    }))
}

/// `idle`: running while a run writes it; else how its last turn ended, which stands while
/// nobody holds it.
pub fn idle(root: &Path, held: &Held) -> Sampled {
    let store = Store::new(root);
    Sampled {
        sample: store
            .standing(&held.external_id)
            .ok()
            .flatten()
            .and_then(|standing| {
                let events = store.events(&held.external_id).ok()?;
                sample(&events, &standing, now())
            }),
    }
}

/// Where a conversation stands by its events. With nobody writing it, a turn that began and
/// did not end was cut off.
fn sample(events: &[Event], standing: &Standing, now: u64) -> Option<Sample> {
    let mut model = None;
    let mut began = None;
    let mut ended = None;
    for event in events {
        match event {
            Event::Model { model: named, .. } => model = Some(named.clone()),
            Event::TurnBegan { at, .. } => {
                began = Some(*at);
                ended = None;
            }
            Event::TurnEnded {
                at, state, failure, ..
            } => ended = Some((*at, *state, failure.clone())),
            _ => {}
        }
    }
    let mut sample = Sample {
        state: Execution::Running,
        observed_at: now,
        started_at: began,
        ended_at: None,
        model,
        thinking: None,
        error: None,
        vacant: false,
        if_unheld: false,
    };
    if standing.newest && standing.running {
        return Some(sample);
    }
    began?;
    sample.vacant = true;
    sample.if_unheld = true;
    sample.state = match ended {
        None => Execution::Interrupted,
        Some((at, state, failure)) => {
            sample.ended_at = Some(at);
            match state {
                TurnState::Finished => Execution::Finished,
                TurnState::Interrupted => Execution::Interrupted,
                TurnState::Failed => {
                    sample.error = failure.map(|failure| failure.line);
                    Execution::Failed
                }
            }
        }
    };
    Some(sample)
}

/// `start`: the launch plan of a turn ([`shared::launch`]). A continue goes on under the
/// conversation's key, which writes only its newest conversation; a model is named to the
/// gateway as its list's `<provider>/<model>`. A model the last list (`listed`) lacks is
/// refused before anything runs; a thinking level is OpenClaw's to refuse, as its list names
/// none.
pub fn start(
    home: &Path,
    root: &Path,
    listed: &[Model],
    mut start: Start,
) -> Result<Launch, Error> {
    if start.act == Act::Fork {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "`openclaw acp` has no fork",
        ));
    }
    shared::offered("OpenClaw", listed, start.model.as_deref(), None)?;
    if let Some(model) = &start.model
        && let Some(provider) = listed
            .iter()
            .find(|entry| &entry.model == model)
            .and_then(|entry| entry.provider.as_ref())
    {
        start.model = Some(format!("{provider}/{model}"));
    }
    if start.act == Act::Continue
        && let Some(id) = &start.external_id
    {
        let standing = Store::new(root).standing(id)?.ok_or_else(|| {
            Error::refused(refusal::NOT_FOUND, format!("no OpenClaw conversation {id}"))
        })?;
        if !standing.newest {
            return Err(Error::refused(
                refusal::REFUSED,
                format!(
                    "OpenClaw's key {} writes a newer conversation now, and goes on only with \
                     that one",
                    standing.key
                ),
            ));
        }
        start.external_id = Some(standing.key);
    }
    let hook = shared::hook(&super::setup::hook_file(root));
    shared::launch(
        "OpenClaw",
        super::setup::binary(home).as_deref(),
        start,
        |_| Ok(None),
        hook,
        None,
        BTreeMap::new(),
    )
}

/// `turn/read`: a line of a turn's output ([`shared::turn_line`]), its session named by the
/// conversation that the session writes. A session the store does not have yet names none.
pub fn turn_line(root: &Path, read: TurnRead) -> TurnLine {
    let mut line = shared::turn_line("OpenClaw", read);
    if let Some(session) = line.named.take() {
        line.named = Store::new(root).conversation(&session);
    }
    line
}

/// `receipt/mark`: how many records the conversation has.
pub fn mark(root: &Path, mark: &ReceiptMark) -> Result<Marked, Error> {
    Ok(Marked {
        mark: Store::new(root).last(&mark.external_id)?.to_string(),
    })
}

/// `receipt/find`: whether exactly one of the `user` messages after the mark carries the
/// marker.
pub fn find(root: &Path, find: &ReceiptFind) -> Result<Receipt, Error> {
    let after = find
        .mark
        .parse()
        .map_err(|_| Error::invalid(format!("mark {} is not a count of OpenClaw's", find.mark)))?;
    let said = Store::new(root).said_after(&find.external_id, after)?;
    shared::find(&said, &find.delivery)
}

/// `terminal`: OpenClaw's own chat on the conversation's key, in the folder it ran in.
pub fn terminal(root: &Path, terminal: &Terminal) -> Result<TerminalCommand, Error> {
    let standing = Store::new(root)
        .standing(&terminal.external_id)?
        .ok_or_else(|| {
            Error::refused(
                refusal::NOT_FOUND,
                format!("no OpenClaw conversation {}", terminal.external_id),
            )
        })?;
    let cwd = standing.cwd.unwrap_or_else(|| terminal.cwd.clone());
    Ok(shared::terminal("openclaw tui", &cwd, &standing.key))
}
