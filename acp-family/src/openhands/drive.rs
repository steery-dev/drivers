//! OpenHands at drive: Steery starts OpenHands' turns itself. A turn is this driver's own
//! binary in its turn mode, speaking ACP to `openhands acp` ([`crate::turn`]), which runs the
//! conversation in the turn's own process and asks the turn before each batch of actions
//! its agent writes (`always-ask`, its default). OpenHands keeps no mark of who runs a
//! conversation: `base_state.json`'s `execution_status` stays `running` where its process
//! died, so who holds a conversation is for the runtime to look for.
//!
//! OpenHands runs every conversation on the one model its `agent_settings.json` names, and
//! its ACP server sets no model or reasoning effort of a session.

use std::collections::BTreeMap;

use steery_sdk::wire::harness::{
    Act, Beyond, Execution, Held, HeldBy, Holder, Launch, Look, Looked, Marked, Model, Models,
    Receipt, ReceiptFind, ReceiptMark, Sample, Sampled, Start, Terminal, TerminalCommand,
};
use steery_sdk::{Error, refusal};

use super::mcp;
use super::place::Place;
use super::store::Store;
use crate::turn::drive::{self as shared, now};

/// The label in the refusals of the family's shared methods.
pub(crate) const LABEL: &str = "OpenHands";

/// Why `start` refuses where OpenHands has no model to run.
const SIGNED_OUT: &str = "OpenHands has no model: run `openhands` and set one up";

/// OpenHands, to a turn this driver runs ([`crate::turn`]): words told mid-turn go in as one
/// prompt once OpenHands has answered the turn's. A second `session/prompt` while one runs
/// would send its message into the running conversation and start a second run of it.
pub struct Turns;

impl crate::turn::Host for Turns {
    fn harness(&self) -> crate::Harness {
        crate::Harness::OpenHands
    }

    fn told(&self) -> crate::turn::Told {
        crate::turn::Told::After
    }

    /// OpenHands has no `session/resume`; `session/load` replays the conversation's events.
    fn opens(&self) -> crate::turn::Opens {
        crate::turn::Opens::Load
    }

    /// OpenHands runs a conversation only while a prompt runs it.
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

/// `held`: where the runtime looks. OpenHands writes each event file once and holds none
/// open, so a process that runs the conversation may hold it unseen. `openhands acp` serves
/// every conversation its client opens, so stopping it would stop them all.
pub fn held(place: &Place, held: &Held) -> Result<HeldBy, Error> {
    let store = Store::new(&place.conversations);
    let Ok((folder, _)) = store.conversation(&held.external_id) else {
        return Ok(HeldBy::Holder(Holder::Nobody {
            proof: "OpenHands has no such conversation".into(),
        }));
    };
    Ok(HeldBy::Look(Looked {
        look: Look {
            transcripts: vec![folder.join("base_state.json"), folder.join("events")],
            strays: true,
            beyond: vec![Beyond {
                arg: Some("acp".into()),
                line: "OpenHands' ACP server, which serves every conversation its client opens"
                    .into(),
            }],
        },
    }))
}

/// `idle`: how the conversation's last turn ended, which stands only while nobody holds it.
/// A turn still open there was cut off. A conversation it cannot read has no sample.
pub fn idle(place: &Place, held: &Held) -> Sampled {
    let store = Store::new(&place.conversations);
    let Ok((events, open)) = store.events(&held.external_id, mcp::names(place)) else {
        return Sampled { sample: None };
    };
    let sample = if open {
        let started_at = events.iter().rev().find_map(|event| match event {
            steery_sdk::wire::harness::Event::TurnBegan { at, .. } => Some(*at),
            _ => None,
        });
        Some(Sample {
            state: Execution::Interrupted,
            observed_at: now(),
            started_at,
            ended_at: None,
            model: None,
            thinking: None,
            error: None,
            vacant: true,
            if_unheld: true,
        })
    } else {
        shared::sample(&events)
    };
    Sampled { sample }
}

/// The model `agent_settings.json` names, which every conversation runs on.
fn model(place: &Place) -> Option<String> {
    steery_driver_common::mcp::json(&place.settings())?["llm"]["model"]
        .as_str()
        .filter(|model| !model.is_empty())
        .map(str::to_owned)
}

/// `models`: the one model OpenHands runs here, with no thinking level a session can set.
/// Without one, OpenHands is not set up to run anything.
pub fn models(place: &Place) -> Models {
    let model = model(place);
    Models {
        signed_in: Some(model.is_some()),
        models: model
            .into_iter()
            .map(|model| Model {
                model,
                provider: None,
                label: None,
                thinking: Vec::new(),
                cost: None,
            })
            .collect(),
        unlisted: None,
        watch: vec![place.settings()],
    }
}

/// `start`: the launch plan of a turn ([`shared::launch`]). OpenHands' ACP server loads a
/// conversation or starts one; it has no fork and no hand-over. A model other than the one
/// it runs, or any thinking level, is refused before anything runs.
pub fn start(place: &Place, mut start: Start) -> Result<Launch, Error> {
    if matches!(start.act, Act::Fork | Act::Hand) {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "OpenHands continues a conversation or starts one; it has no fork or hand-over",
        ));
    }
    let listed = models(place).models;
    if listed.is_empty() {
        return Err(Error::refused(refusal::REFUSED, SIGNED_OUT));
    }
    shared::offered(LABEL, &listed, start.model.as_deref(), None)?;
    if let Some(level) = &start.thinking {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("OpenHands sets no thinking level of a turn, so not {level}"),
        ));
    }
    // The one model it runs needs no pick, and its ACP server would ignore one.
    start.model = None;
    let hook = shared::hook(&super::setup::hook_file(place));
    let store = Store::new(&place.conversations);
    let directory = |id: &str| Ok(store.conversation(id)?.1.cwd);
    // A loaded conversation works in the folder its server process runs in, and saves that
    // folder over its own, so a continue runs where the conversation ran.
    if start.act == Act::Continue
        && let Some(id) = &start.external_id
        && let Some(cwd) = directory(id)?
    {
        start.cwd = cwd;
    }
    shared::launch(
        LABEL,
        place.binary().as_deref(),
        start,
        directory,
        hook,
        None,
        BTreeMap::new(),
    )
}

/// `receipt/mark`: no door, so no point to look after.
pub fn mark(_: &ReceiptMark) -> Result<Marked, Error> {
    Err(Error::refused(
        refusal::SELECTION_UNSUPPORTED,
        "OpenHands has no door whose words need a receipt",
    ))
}

/// `receipt/find`: never asked, as `mark` gives no mark.
pub fn find(_: &ReceiptFind) -> Result<Receipt, Error> {
    Err(Error::refused(
        refusal::SELECTION_UNSUPPORTED,
        "OpenHands has no door whose words need a receipt",
    ))
}

/// `terminal`: OpenHands' own window on the conversation, in the folder it ran in.
pub fn terminal(place: &Place, terminal: &Terminal) -> Result<TerminalCommand, Error> {
    let (_, head) = Store::new(&place.conversations).conversation(&terminal.external_id)?;
    let cwd = head.cwd.unwrap_or_else(|| terminal.cwd.clone());
    Ok(TerminalCommand {
        command: format!(
            "cd {} && {} --resume {}",
            quoted(&cwd.to_string_lossy()),
            super::place::EXECUTABLE,
            quoted(&head.id)
        ),
        app: None,
        line: None,
    })
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}
