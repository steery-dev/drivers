//! Steery's driver for OpenHands at drive: it reads OpenHands' conversations from their
//! event files, plans the entries that set OpenHands up for Steery, lists the MCP servers
//! OpenHands loads, starts OpenHands' turns through its own ACP server, and reads the
//! permissions those turns ask for.

pub(crate) mod drive;
mod events;
mod hooks;
mod mcp;
mod place;
mod setup;
mod store;

use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

use crate::Harness;
use place::Place;

/// The release whose files this driver reads: the first that the driver ran against.
const MINIMUM: &str = "1.16.0";

/// How an agent in OpenHands learns its conversation's id, as Steery's skill says it.
const KNOWING: &str = "OpenHands gives no identifier of its own to the agent, and the Steery\n\
server cannot name the conversation either. If the instructions that the server\n\
announces name a `harness` and `external_id`, send exactly those values. Otherwise\n\
describe nothing.";

/// How OpenHands keeps a conversation, as Steery's skill says it.
const TRANSCRIPT: &str = "A folder per conversation: `<OPENHANDS_CONVERSATIONS_DIR, else \
<OPENHANDS_PERSISTENCE_DIR or ~/.openhands>/conversations>/<id as 32 hex digits>`. In it, \
`base_state.json` holds the conversation's state (`execution_status`, the agent's `llm`, \
`stats` with each model response's tokens), and `events/event-<NNNNN>-<event id>.json` holds \
each event as one JSON document, numbered in order and never written again. An event's `kind` \
says what it is: `MessageEvent` (`source` `user` or `agent`, words in `llm_message.content`), \
`ActionEvent` (a call: `tool_call_id`, `tool_name`, `tool_call.arguments`), \
`ObservationEvent` and `UserRejectObservation` (its end), `ConversationErrorEvent`, \
`Condensation`.";

/// The driver: the protocol it picked and where OpenHands keeps everything, both from
/// `initialize`, and the listings it answered, to answer the next with only what changed.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
    changes: steery_sdk::listing::Changes,
}

impl Driver {
    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }

    fn store(&self) -> Result<store::Store, Error> {
        Ok(store::Store::new(&self.place()?.conversations))
    }
}

/// OpenHands as Steery reads it, under the protocol picked.
fn info(protocol: u32) -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: vec![wire::Level::Observe, wire::Level::Drive],
        minimum: Some(wire::Minimum {
            release: MINIMUM.into(),
            update: Some("uv tool upgrade openhands".into()),
        }),
        sign_in: Some("run `openhands` in a terminal and set up its model".into()),
        knowing: KNOWING.into(),
        lines: wire::Lines::Agent,
        hosted: Default::default(),
        transcript: Some(TRANSCRIPT.into()),
        identity: wire::Identity {
            executables: vec![place::EXECUTABLE.into()],
            package: None,
            session_variable: None,
            call_meta: None,
        },
        setup: wire::SetupFacts {
            reads_skills: false,
            cuts_tool_text: false,
            // `sdk/mcp/tool.py`, `MCP_TOOL_TIMEOUT_SECONDS`: no entry changes it.
            call_bound: Some(300),
        },
        reader: None,
        facts: wire::HarnessFacts {
            by_file: false,
            // A continue loads the conversation in the folder it ran in.
            resumes_in_place: true,
            forks_beside: false,
            keeps: wire::Keeps {
                model: false,
                thinking: false,
            },
            door: None,
            checks_idle: false,
            worktrees: false,
            acts: vec![harness::Act::Continue, harness::Act::New],
            touches: false,
        },
        hooks: crate::turn::hooks::table(protocol),
    }
}

impl Program for Driver {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let _ = self.place.set(Place::new(&params.home));
        self.changes.picked(protocol);
        Ok(wire::Initialized {
            protocol,
            program: Harness::OpenHands.program()?,
            harness: Some(Box::new(info(protocol))),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::SETUP_PLAN => {
                result(setup::plan(self.place()?, steery_sdk::params(params)?).await)
            }
            method::SESSIONS_LIST => {
                let asked: harness::List = steery_sdk::params(params)?;
                let after = asked.after.clone();
                result(
                    self.changes
                        .answer(after.as_deref(), self.store()?.list(asked)),
                )
            }
            method::SESSIONS_LOCATE => result(self.store()?.locate(steery_sdk::params(params)?)?),
            method::SESSIONS_READ => result(
                self.store()?
                    .read(steery_sdk::params(params)?, mcp::names(self.place()?))?,
            ),
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "OpenHands has no plugin door that sends its records",
                ))
            }
            method::HOOK_READ => result(hooks::read(self.place()?, steery_sdk::params(params)?)?),
            method::HOOK_ANSWER => result(crate::turn::hooks::answer(
                drive::LABEL,
                steery_sdk::params(params)?,
            )?),
            method::HELD => result(drive::held(self.place()?, &steery_sdk::params(params)?)?),
            method::IDLE => result(drive::idle(self.place()?, &steery_sdk::params(params)?)),
            method::MODELS => result(drive::models(self.place()?)),
            method::START => result(drive::start(self.place()?, steery_sdk::params(params)?)?),
            method::TURN_READ => result(crate::turn::drive::turn_line(
                drive::LABEL,
                steery_sdk::params(params)?,
            )),
            method::STOP => result(crate::turn::drive::stop(
                drive::LABEL,
                &steery_sdk::params(params)?,
            )?),
            method::RECEIPT_MARK => result(drive::mark(&steery_sdk::params(params)?)?),
            method::RECEIPT_FIND => result(drive::find(&steery_sdk::params(params)?)?),
            method::TERMINAL => result(drive::terminal(
                self.place()?,
                &steery_sdk::params(params)?,
            )?),
            method::MCP_LIST => result(mcp::list(self.place()?, steery_sdk::params(params)?)),
            _ => Err(Error::Unknown),
        }
    }
}
