//! Steery's driver for Kilo at observe: it reads Kilo's conversations from its SQLite
//! database, plans the entries that set Kilo up for Steery, and lists the MCP servers Kilo
//! resolves.

mod calls;
mod config;
mod events;
mod setup;
mod store;

use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

use crate::Harness;

/// The driver: what `initialize` said, and the listings it answered.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
    changes: steery_sdk::listing::Changes,
}

/// The protocol the driver picked, the person's home, and Kilo's database.
struct Place {
    protocol: u32,
    home: PathBuf,
    store: store::Store,
}

impl Driver {
    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }

    fn info() -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: vec![wire::Level::Observe],
            minimum: Some(wire::Minimum {
                release: "7.8.27".into(),
                update: Some("kilo upgrade".into()),
            }),
            sign_in: Some("run `kilo auth login`".into()),
            knowing: "Kilo gives no identifier of its own to the agent, and its one MCP\n\
                      connection serves every conversation, so the Steery server cannot\n\
                      name it either. If the instructions that the server announces name a\n\
                      `harness` and `external_id`, send exactly those values. Otherwise\n\
                      describe nothing."
                .into(),
            lines: wire::Lines::Agent,
            hosted: Default::default(),
            transcript: Some(
                "Rows in the SQLite database `kilo.db`: one `session` row per conversation, \
                 its `message` rows, and each message's `part` rows (text, reasoning, tool \
                 call, step end), each a JSON `data` column."
                    .into(),
            ),
            identity: wire::Identity {
                // npm's `kilo` and `kilocode` launch the native `.kilo` beside them.
                executables: vec!["kilo".into(), "kilocode".into(), ".kilo".into()],
                package: None,
                session_variable: None,
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
}

impl Program for Driver {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let _ = self.place.set(Place {
            protocol,
            home: params.home.clone(),
            store: store::Store::new(&params.home),
        });
        self.changes.picked(self.place()?.protocol);
        Ok(wire::Initialized {
            protocol: self.place()?.protocol,
            program: Harness::Kilo.program()?,
            harness: Some(Box::new(Self::info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let place = self.place()?;
        let store = &place.store;
        match method {
            method::SETUP_PLAN => result(
                setup::plan(
                    &place.home,
                    store.data(),
                    store.config(),
                    steery_sdk::params(params)?,
                )
                .await?,
            ),
            method::SESSIONS_LIST => {
                let asked: harness::List = steery_sdk::params(params)?;
                let after = asked.after.clone();
                result(self.changes.answer(after.as_deref(), store.list(asked)?))
            }
            method::SESSIONS_LOCATE => result(store.locate(steery_sdk::params(params)?)?),
            method::SESSIONS_READ => result(store.read(steery_sdk::params(params)?)?),
            method::SESSIONS_FOLD => Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Kilo's driver has no plugin door",
            )),
            method::MCP_LIST => result(
                config::list(
                    setup::binary(&place.home).as_deref(),
                    &place.home,
                    store.config(),
                    steery_sdk::params(params)?,
                )
                .await?,
            ),
            _ => Err(Error::Unknown),
        }
    }
}
