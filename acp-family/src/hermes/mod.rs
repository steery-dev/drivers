//! Steery's driver for Hermes Agent at observe: it reads Hermes's conversations from the
//! SQLite store `state.db` in its home and in each profile's, plans the skill that sets
//! Hermes up for Steery, and lists the MCP servers of its `config.yaml`.

mod calls;
mod events;
mod mcp;
mod setup;
mod store;

use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, result};

use crate::Harness;

/// The driver: the protocol it picked and Hermes's home, both from `initialize`.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
}

struct Place {
    protocol: u32,
    /// Hermes's home: `HERMES_HOME`, else `~/.hermes`.
    root: PathBuf,
}

impl Driver {
    fn info() -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: vec![wire::Level::Observe],
            minimum: Some(wire::Minimum {
                release: "0.23.1".into(),
                update: Some("hermes update".into()),
            }),
            sign_in: Some("run `hermes setup`".into()),
            knowing:
                "Hermes Agent puts the id of the conversation in `HERMES_SESSION_ID`, in the\n\
                      environment of its terminal. Read it there (`echo $HERMES_SESSION_ID`) and\n\
                      send it as `external_id`, with the harness `hermes`."
                    .into(),
            lines: wire::Lines::Agent,
            hosted: Default::default(),
            transcript: None,
            identity: wire::Identity {
                executables: vec!["hermes".into()],
                package: None,
                session_variable: Some("HERMES_SESSION_ID".into()),
                call_meta: None,
            },
            setup: wire::SetupFacts {
                reads_skills: true,
                cuts_tool_text: false,
                call_bound: None,
            },
            reader: None,
            facts: wire::HarnessFacts {
                by_file: false,
                resumes_in_place: true,
                forks_beside: false,
                keeps: wire::Keeps {
                    model: true,
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

    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }
}

impl Program for Driver {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let _ = self.place.set(Place {
            protocol,
            root: store::root(&params.home),
        });
        Ok(wire::Initialized {
            protocol: self.place()?.protocol,
            program: Harness::Hermes.program()?,
            harness: Some(Box::new(Self::info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let place = self.place()?;
        let root = &place.root;
        match method {
            method::SETUP_PLAN => {
                result(setup::plan(root, steery_sdk::params(params)?, place.protocol).await)
            }
            method::SESSIONS_LIST => {
                result(store::Home::new(root).list(steery_sdk::params(params)?)?)
            }
            method::SESSIONS_LOCATE => {
                result(store::Home::new(root).locate(steery_sdk::params(params)?)?)
            }
            method::SESSIONS_READ => {
                result(store::Home::new(root).read(steery_sdk::params(params)?)?)
            }
            method::MCP_LIST => {
                let _: harness::McpList = steery_sdk::params(params)?;
                result(mcp::list(root))
            }
            _ => Err(Error::Unknown),
        }
    }
}
