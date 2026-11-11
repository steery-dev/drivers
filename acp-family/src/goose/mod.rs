//! Steery's driver for Goose at observe: it reads Goose's conversations from its SQLite
//! file, plans the entries that set Goose up for Steery, and lists the MCP servers Goose
//! loads.

mod config;
mod events;
mod place;
mod setup;
mod store;

use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, result};

use crate::Harness;
use place::Place;

/// The release whose files this driver reads: the first that the driver ran against.
const MINIMUM: &str = "1.59.0";

/// The variable in which Goose puts the id of its conversation, in the environment of each
/// shell command it runs and of each MCP server it starts (`developer/shell.rs:616-622`,
/// `extension_manager/mod.rs:957`).
const VARIABLE: &str = "AGENT_SESSION_ID";

/// How an agent in Goose learns its conversation's id, as Steery's skill says it.
const KNOWING: &str = "Goose puts the id of the conversation in `AGENT_SESSION_ID`, in the\n\
environment of the shell commands it runs. Read it there (`echo $AGENT_SESSION_ID`) and\n\
send it as `external_id`, with the harness `goose`. A sub-agent's variable holds its own\n\
conversation, not its lead's: send it as `sub_agent`, and your lead's id as `external_id`.";

/// How Goose keeps a conversation, as Steery's skill says it.
const TRANSCRIPT: &str = "One SQLite file, not a transcript: `<GOOSE_PATH_ROOT>/data`, else \
`<XDG_DATA_HOME or ~/.local/share>/goose`, then `sessions/sessions.db`. Table `sessions` lists \
the conversations (`parent_session_id` names a sub-agent's lead); each message is a row of \
`messages`, in `id` order, its `content_json` a JSON array of blocks tagged `type` (`text`, \
`thinking`, `toolRequest`, `toolResponse`, `error`) and its `metadata_json` saying \
`userVisible`. Goose writes a conversation's rows anew whole when it changes one.";

/// The driver: the protocol it picked and where Goose keeps everything, both from
/// `initialize`, and the listings it answered, to answer the next with only what changed.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<(u32, Place)>,
    changes: steery_sdk::listing::Changes,
}

impl Driver {
    fn place(&self) -> Result<&Place, Error> {
        Ok(&self.picked()?.1)
    }

    fn picked(&self) -> Result<&(u32, Place), Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }
}

/// Goose as Steery reads it.
fn info() -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: vec![wire::Level::Observe],
        minimum: Some(wire::Minimum {
            release: MINIMUM.into(),
            update: Some("goose update".into()),
        }),
        sign_in: Some("run `goose configure` in a terminal".into()),
        knowing: KNOWING.into(),
        lines: wire::Lines::Agent,
        hosted: Default::default(),
        transcript: Some(TRANSCRIPT.into()),
        identity: wire::Identity {
            executables: vec!["goose".into()],
            package: None,
            session_variable: Some(VARIABLE.into()),
            call_meta: None,
        },
        setup: wire::SetupFacts {
            reads_skills: false,
            cuts_tool_text: false,
            call_bound: None,
        },
        reader: None,
        facts: wire::HarnessFacts {
            by_file: false,
            resumes_in_place: true,
            forks_beside: false,
            keeps: wire::Keeps {
                model: false,
                thinking: false,
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
        let _ = self.place.set((protocol, Place::new(&params.home)));
        self.changes.picked(protocol);
        Ok(wire::Initialized {
            protocol,
            program: Harness::Goose.program()?,
            harness: Some(Box::new(info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::SETUP_PLAN => {
                let (protocol, place) = self.picked()?;
                result(setup::plan(place, *protocol, steery_sdk::params(params)?).await)
            }
            method::SESSIONS_LIST => {
                let asked: harness::List = steery_sdk::params(params)?;
                let after = asked.after.clone();
                let db = self.place()?.db();
                result(
                    self.changes
                        .answer(after.as_deref(), store::Store::new(&db).list(asked)?),
                )
            }
            method::SESSIONS_LOCATE => {
                let db = self.place()?.db();
                result(store::Store::new(&db).locate(steery_sdk::params(params)?)?)
            }
            method::SESSIONS_READ => {
                let place = self.place()?;
                let db = place.db();
                result(
                    store::Store::new(&db)
                        .read(steery_sdk::params(params)?, || config::server_names(place))?,
                )
            }
            method::MCP_LIST => result(config::mcp(self.place()?, steery_sdk::params(params)?)),
            _ => Err(Error::Unknown),
        }
    }
}
