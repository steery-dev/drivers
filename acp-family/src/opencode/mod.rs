//! Steery's driver for OpenCode 2 at observe: it reads OpenCode's conversations from its
//! SQLite file, plans the entries that set OpenCode up for Steery, and lists the MCP
//! servers OpenCode loads. OpenCode 1 keeps other tables in the same file and gets no
//! driver.

mod config;
mod events;
mod place;
mod setup;
mod store;

use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

use crate::Harness;
use place::Place;

/// The release whose tables this driver reads: the first that the driver ran against.
const MINIMUM: &str = "2.2.6";

/// How an agent in OpenCode learns its conversation's id, as Steery's skill says it.
const KNOWING: &str = "Every shell command that OpenCode runs for you has `OPENCODE_SESSION_ID` in\n\
its environment. It holds the id of the conversation that runs the command. Send it as\n\
`external_id`. When OpenCode started you as a sub-agent, the variable holds your own\n\
conversation, not your lead's: send it as `sub_agent`. Your lead sends its own id. Read it\n\
from the environment when you send it, not from your context. If the variable is unset or\n\
empty, you have no identifier: describe nothing.";

/// How OpenCode's conversations read.
const TRANSCRIPT: &str = "One SQLite file, not a transcript: each message is a row of \
`session_message`, in `seq` order, its JSON in `data`. An assistant's longer content is in \
`session_part`.";

/// The driver: the protocol it picked from `initialize`, where OpenCode keeps everything,
/// and the listings it answered, to answer the next with only what changed.
#[derive(Default)]
pub struct Driver {
    protocol: OnceLock<u32>,
    place: OnceLock<Place>,
    changes: steery_sdk::listing::Changes,
}

impl Driver {
    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }

    fn store(&self) -> Result<store::Store<'_>, Error> {
        Ok(store::Store::new(&self.place()?.db))
    }
}

/// OpenCode as Steery reads it, under the protocol picked.
fn info(protocol: u32) -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: vec![wire::Level::Observe],
        minimum: Some(wire::Minimum {
            release: MINIMUM.into(),
            update: Some("opencode upgrade".into()),
        }),
        sign_in: Some("run `opencode auth login` in a terminal".into()),
        knowing: KNOWING.into(),
        lines: wire::Lines::Agent,
        hosted: Default::default(),
        transcript: Some(TRANSCRIPT.into()),
        identity: wire::Identity {
            executables: vec!["opencode".into()],
            package: None,
            session_variable: Some("OPENCODE_SESSION_ID".into()),
            // One MCP client serves every conversation, and each `tools/call` names the
            // conversation that makes it in `_meta`: for a sub-agent, the sub-agent's own.
            call_meta: (protocol >= steery_sdk::CALL_META).then(|| wire::CallMeta {
                session: "ai.opencode/sessionID".into(),
                sub_agent: None,
            }),
        },
        setup: wire::SetupFacts {
            reads_skills: true,
            cuts_tool_text: false,
            call_bound: None,
        },
        reader: None,
        facts: wire::HarnessFacts {
            by_file: false,
            // `run -s <id>` and ACP's `session/load` take the folder the conversation ran in.
            resumes_in_place: true,
            // A fork is a new conversation that names its source.
            forks_beside: true,
            keeps: wire::Keeps {
                model: true,
                thinking: true,
            },
            door: None,
            checks_idle: false,
            worktrees: false,
            acts: Vec::<harness::Act>::new(),
            touches: false,
        },
        hooks: Vec::new(),
    }
}

impl Program for Driver {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let _ = self.protocol.set(protocol);
        let _ = self.place.set(Place::new(&params.home));
        self.changes.picked(protocol);
        Ok(wire::Initialized {
            protocol,
            program: Harness::OpenCode.program()?,
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
                result(self.changes.answer(after.as_deref(), self.store()?.list(asked)?))
            }
            method::SESSIONS_LOCATE => result(self.store()?.locate(steery_sdk::params(params)?)?),
            method::SESSIONS_READ => {
                let place = self.place()?;
                let read = self.store()?.read(steery_sdk::params(params)?, |cwd| {
                    config::server_names(place, Some(cwd))
                })?;
                result(read)
            }
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "OpenCode has no plugin door that sends its records",
                ))
            }
            method::MCP_LIST => result(config::mcp(self.place()?, steery_sdk::params(params)?)),
            _ => Err(Error::Unknown),
        }
    }
}
