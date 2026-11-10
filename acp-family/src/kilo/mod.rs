//! Steery's driver for Kilo at live: it reads Kilo's conversations from its SQLite
//! database, plans the entries that set Kilo up for Steery (its plugin among them, which
//! reaches each live conversation through Steery's plugin door), lists the MCP servers Kilo
//! resolves, starts Kilo's turns through Kilo's own ACP server, and reads the permissions
//! those turns ask for.

mod calls;
mod config;
pub(crate) mod drive;
mod events;
mod hooks;
mod setup;
mod store;

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

use crate::Harness;

/// The driver: what `initialize` said, the listings it answered, and Kilo's models as
/// `models` last listed them, which `start` checks a pick against.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
    changes: steery_sdk::listing::Changes,
    listed: Mutex<Vec<harness::Model>>,
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

    fn info(protocol: u32) -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: vec![wire::Level::Observe, wire::Level::Drive, wire::Level::Live],
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
                door: Some(harness::LiveDoor::Plugin),
                checks_idle: true,
                worktrees: false,
                acts: vec![
                    harness::Act::Continue,
                    harness::Act::Fork,
                    harness::Act::New,
                ],
                touches: false,
            },
            hooks: crate::turn::hooks::table(protocol),
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
            harness: Some(Box::new(Self::info(protocol))),
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
            method::HOOK_READ => result(hooks::read(store, steery_sdk::params(params)?)?),
            method::HOOK_ANSWER => result(crate::turn::hooks::answer(
                "Kilo",
                steery_sdk::params(params)?,
            )?),
            method::SESSIONS_FOLD => Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Kilo's plugin sends no entries: the driver reads kilo.db",
            )),
            method::HELD => result(drive::held(store, &steery_sdk::params(params)?)),
            method::IDLE => result(drive::idle(store, &steery_sdk::params(params)?)),
            method::MODELS => {
                let models = drive::models(setup::binary(&place.home).as_deref(), store).await?;
                *self.listed.lock().unwrap_or_else(|e| e.into_inner()) = models.models.clone();
                result(models)
            }
            method::START => result(drive::start(
                setup::binary(&place.home).as_deref(),
                store,
                &self.listed.lock().unwrap_or_else(|e| e.into_inner()),
                steery_sdk::params(params)?,
            )?),
            method::TURN_READ => result(crate::turn::drive::turn_line(
                "Kilo",
                steery_sdk::params(params)?,
            )),
            method::STOP => result(crate::turn::drive::stop(
                "Kilo",
                &steery_sdk::params(params)?,
            )?),
            method::RECEIPT_MARK => result(drive::mark(store, &steery_sdk::params(params)?)?),
            method::RECEIPT_FIND => result(drive::find(store, &steery_sdk::params(params)?)?),
            method::TERMINAL => result(drive::terminal(store, &steery_sdk::params(params)?)?),
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
