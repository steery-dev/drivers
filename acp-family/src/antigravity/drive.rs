//! Antigravity at drive: Steery starts Antigravity's turns itself. A turn is this driver's own
//! binary in its turn mode, speaking ACP to `agy acp` ([`crate::turn`]), which runs the
//! conversation in the turn's own process. While a process runs a conversation it holds the
//! conversation's presence lock ([`super::store`]), so who holds one is the process that
//! holds that file, and what stands of a conversation is true only while nobody does.
use std::collections::BTreeMap;
use std::path::Path;

use steery_sdk::wire::harness::{
    Act, Event, Execution, Held, HeldBy, Holder, Launch, Look, Looked, Marked, Model, Receipt,
    ReceiptFind, ReceiptMark, Sample, Sampled, Start, Terminal, TerminalCommand, TurnState, Voice,
};
use steery_sdk::{Error, refusal};

use super::Place;
use super::store::{self, Store};
use crate::turn::drive::{self as shared, now};

/// The harness, as drive's lines name it.
pub(crate) const LABEL: &str = "Antigravity";

/// The config option of a session that picks its thinking level.
const THINKING: &str = "thought_level";

/// The thinking levels that `agy models` puts at the end of a model's id.
const LEVELS: [&str; 2] = ["low", "high"];

/// Why `start` refuses where Antigravity is signed out.
const SIGNED_OUT: &str =
    "Antigravity is signed out: run `agy` in a terminal and sign in, or set `GEMINI_API_KEY`";

/// Antigravity, to a turn this driver runs ([`crate::turn`]): words told mid-turn go in as one
/// prompt once Antigravity has answered the turn's. A second `session/prompt` while one runs
/// is queued behind it, and Antigravity has no way to steer words into the running turn.
pub struct Turns;

impl crate::turn::Host for Turns {
    fn harness(&self) -> crate::Harness {
        crate::Harness::Antigravity
    }

    fn told(&self) -> crate::turn::Told {
        crate::turn::Told::After
    }

    /// `session/resume` opens a conversation without replaying it.
    fn opens(&self) -> crate::turn::Opens {
        crate::turn::Opens::Resume
    }

    fn thinking(&self) -> &'static str {
        THINKING
    }

    /// Antigravity runs one prompt of a session at a time and nothing of it without one:
    /// once it has answered every prompt the turn sent, it runs nothing more of it.
    fn runs(
        &self,
        _acp: Option<u32>,
        _session: &str,
        _queued: &[String],
        _: &mut Option<tokio::time::Instant>,
    ) -> bool {
        false
    }

    /// An ACP session writes no history, so the turn marks its conversation as driven, in
    /// the root of the home `start` gave it ([`store::drove`]). A mark it cannot write leaves
    /// the conversation unlisted; standard error is the turn's refusal, so it says nothing.
    fn opened(&self, session: &str) -> std::io::Result<()> {
        if let Some(home) = std::env::var_os("HOME") {
            let _ = store::drove(&super::root(Path::new(&home)), session);
        }
        Ok(())
    }
}

/// `held`: with no such conversation, or with its presence lock free, nobody runs a turn of
/// it. Else the runtime looks for the process that holds the lock open.
pub fn held(place: &Place, held: &Held) -> Result<HeldBy, Error> {
    let store = &place.store;
    let id = &held.external_id;
    if !store.transcript(id).is_file() {
        return Ok(HeldBy::Holder(Holder::Nobody {
            proof: "Antigravity has no such conversation".into(),
        }));
    }
    if !store.running(id) {
        return Ok(HeldBy::Holder(Holder::Nobody {
            proof: "no process holds the conversation's presence lock".into(),
        }));
    }
    Ok(HeldBy::Look(Looked {
        look: Look {
            transcripts: vec![store.lock(id)],
            strays: false,
            beyond: Vec::new(),
        },
    }))
}

/// `idle`: how the conversation's last turn ended, which stands only while nobody holds it.
/// A turn left open at the end of its transcript was cut off.
pub fn idle(place: &Place, held: &Held) -> Sampled {
    Sampled {
        sample: events(&place.store, &held.external_id)
            .ok()
            .and_then(|events| sample(&events, now())),
    }
}

/// Where a conversation stands by its events; none before its first turn ends.
fn sample(events: &[Event], now: u64) -> Option<Sample> {
    let mut model = None;
    let mut began = None;
    let mut ended = None;
    for event in events {
        match event {
            Event::Model { model: named, .. } => model = Some(named.clone()),
            Event::TurnBegan { at, .. } => began = Some(*at),
            Event::TurnEnded {
                at, state, failure, ..
            } => ended = Some((*at, *state, failure.clone())),
            _ => {}
        }
    }
    let (at, state, failure) = ended?;
    let (state, error) = match state {
        TurnState::Failed => (Execution::Failed, failure.map(|failure| failure.line)),
        TurnState::Interrupted => (Execution::Interrupted, None),
        _ => (Execution::Finished, None),
    };
    Some(Sample {
        state,
        observed_at: now,
        started_at: began,
        ended_at: Some(at),
        model,
        thinking: None,
        error,
        vacant: true,
        if_unheld: true,
    })
}

/// Every event of the conversation's transcript, read as one nobody runs.
fn events(store: &Store, id: &str) -> Result<Vec<Event>, Error> {
    let path = store.transcript(id);
    let mut cursor = None;
    let mut all = Vec::new();
    loop {
        let read = super::events::read(&path, cursor.as_deref(), None, false)?;
        all.extend(read.events);
        if !read.more {
            return Ok(all);
        }
        cursor = Some(read.cursor);
    }
}

/// The person's words in the conversation, in order.
fn said(store: &Store, id: &str) -> Result<Vec<String>, Error> {
    Ok(events(store, id)?
        .into_iter()
        .filter_map(|event| match event {
            Event::Words {
                voice: Voice::Person,
                text,
                ..
            } => Some(text),
            _ => None,
        })
        .collect())
}

/// `start`: the launch plan of a turn ([`shared::launch`]). Antigravity's ACP server resumes
/// a conversation or starts one; it has no fork and no hand-over. A model or thinking level
/// its last list (`listed`) does not have is refused before anything runs. `agy models`
/// lists a model at each thinking level as one id (`gemini-3.1-pro-high`), which the turn
/// picks as the session's `model` and `thought_level` (`gemini-3.1-pro`, `high`). The turn
/// runs in the person's home that `initialize` named, where Antigravity keeps its root.
pub fn start(
    place: &Place,
    listed: &[Model],
    signed_out: bool,
    mut start: Start,
) -> Result<Launch, Error> {
    if matches!(start.act, Act::Fork | Act::Hand) {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Antigravity continues a conversation or starts one; it has no fork or hand-over",
        ));
    }
    if signed_out {
        return Err(Error::refused(refusal::REFUSED, SIGNED_OUT));
    }
    shared::offered(
        LABEL,
        listed,
        start.model.as_deref(),
        start.thinking.as_deref(),
    )?;
    if let Some(listed) = start.model.take() {
        let (model, level) = picked(&listed);
        start.model = Some(model);
        start.thinking = start.thinking.or(level);
    }
    let hook = shared::hook(&super::setup::hook_file(&place.root));
    shared::launch(
        LABEL,
        super::binary(&place.home).as_deref(),
        start,
        |id| place.store.folder(id),
        hook,
        BTreeMap::from([("HOME".to_owned(), place.home.to_string_lossy().into_owned())]),
    )
}

/// The session's model and thinking level of a model id that `agy models` lists.
fn picked(listed: &str) -> (String, Option<String>) {
    match listed.rsplit_once('-') {
        Some((model, level)) if LEVELS.contains(&level) => {
            (model.to_owned(), Some(level.to_owned()))
        }
        _ => (listed.to_owned(), None),
    }
}

/// `receipt/mark`: how many times the person has spoken in the conversation.
pub fn mark(place: &Place, mark: &ReceiptMark) -> Result<Marked, Error> {
    Ok(Marked {
        mark: said(&place.store, &mark.external_id)?.len().to_string(),
    })
}

/// `receipt/find`: whether exactly one of the person's words after the mark carries the
/// marker.
pub fn find(place: &Place, find: &ReceiptFind) -> Result<Receipt, Error> {
    let mark: usize = find
        .mark
        .parse()
        .map_err(|_| Error::invalid(format!("`{}` is no mark of this driver's", find.mark)))?;
    let said = said(&place.store, &find.external_id)?;
    shared::find(said.get(mark..).unwrap_or_default(), &find.delivery)
}

/// `terminal`: Antigravity's own terminal on the conversation, in the folder it ran in.
pub fn terminal(place: &Place, terminal: &Terminal) -> Result<TerminalCommand, Error> {
    let cwd = place
        .store
        .folder(&terminal.external_id)?
        .unwrap_or_else(|| terminal.cwd.clone());
    Ok(TerminalCommand {
        command: format!(
            "cd {} && agy --conversation {}",
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
