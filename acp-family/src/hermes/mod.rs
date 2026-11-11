//! Steery's driver for Hermes Agent at drive: it reads Hermes's conversations from the
//! SQLite store `state.db` in its home and in each profile's, plans the skill, the MCP
//! server and the hook that set Hermes up for Steery, lists the MCP servers of its
//! `config.yaml`, and starts Hermes's turns through `hermes acp`.

mod calls;
pub mod drive;
mod events;
mod hooks;
mod mcp;
mod setup;
mod store;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

use crate::Harness;

/// The driver: the protocol it picked and Hermes's home, both from `initialize`, Hermes's
/// models as `models` last listed them, which `start` checks a pick against, and whether
/// that listing found Hermes signed out.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
    listed: Mutex<Vec<harness::Model>>,
    signed_out: AtomicBool,
}

struct Place {
    protocol: u32,
    /// Hermes's home: `HERMES_HOME`, else `~/.hermes`.
    root: PathBuf,
}

impl Driver {
    fn info(protocol: u32) -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: vec![wire::Level::Observe, wire::Level::Drive],
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
                // Hermes resumes a conversation in the folder it is given
                // (`acp_adapter/server.py:684`).
                resumes_in_place: false,
                forks_beside: false,
                keeps: wire::Keeps {
                    model: true,
                    thinking: false,
                },
                door: None,
                checks_idle: false,
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
            harness: Some(Box::new(Self::info(protocol))),
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
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Hermes has no plugin door that sends its records",
                ))
            }
            method::HOOK_READ => result(hooks::read(root, steery_sdk::params(params)?)?),
            method::HOOK_ANSWER => result(crate::turn::hooks::answer(
                "Hermes",
                steery_sdk::params(params)?,
            )?),
            method::HELD => result(drive::held(root, &steery_sdk::params(params)?)?),
            method::IDLE => result(drive::idle(root, &steery_sdk::params(params)?)),
            method::MODELS => {
                let models = drive::models(root).await?;
                *self.listed.lock().unwrap_or_else(|e| e.into_inner()) = models.models.clone();
                self.signed_out
                    .store(models.signed_in == Some(false), Ordering::Relaxed);
                result(models)
            }
            method::START => {
                let listed = self
                    .listed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                result(drive::start(
                    root,
                    &listed,
                    self.signed_out.load(Ordering::Relaxed),
                    steery_sdk::params(params)?,
                )?)
            }
            method::TURN_READ => result(crate::turn::drive::turn_line(
                "Hermes",
                steery_sdk::params(params)?,
            )),
            method::STOP => result(crate::turn::drive::stop(
                "Hermes",
                &steery_sdk::params(params)?,
            )?),
            method::RECEIPT_MARK => result(drive::mark(root, &steery_sdk::params(params)?)?),
            method::RECEIPT_FIND => result(drive::find(root, &steery_sdk::params(params)?)?),
            method::TERMINAL => result(drive::terminal(root, &steery_sdk::params(params)?)?),
            method::MCP_LIST => {
                let _: harness::McpList = steery_sdk::params(params)?;
                result(mcp::list(root))
            }
            _ => Err(Error::Unknown),
        }
    }
}
