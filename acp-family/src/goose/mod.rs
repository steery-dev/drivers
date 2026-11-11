//! Steery's driver for Goose at drive: it reads Goose's conversations from its SQLite file,
//! plans the entries that set Goose up for Steery, lists the MCP servers Goose loads, starts
//! Goose's turns through its own ACP server, and reads the permissions those turns ask for.

mod config;
pub(crate) mod drive;
mod events;
mod hooks;
mod place;
mod setup;
mod store;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

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
/// `initialize`, the listings it answered, to answer the next with only what changed,
/// Goose's models as `models` last listed them, which `start` checks a pick against, and
/// whether that listing found Goose signed out.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<(u32, Place)>,
    changes: steery_sdk::listing::Changes,
    listed: Mutex<Vec<harness::Model>>,
    signed_out: AtomicBool,
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

/// Goose as Steery reads it, under the protocol picked.
fn info(protocol: u32) -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: vec![wire::Level::Observe, wire::Level::Drive],
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
        let _ = self.place.set((protocol, Place::new(&params.home)));
        self.changes.picked(protocol);
        Ok(wire::Initialized {
            protocol,
            program: Harness::Goose.program()?,
            harness: Some(Box::new(info(protocol))),
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
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Goose has no plugin door that sends its records",
                ))
            }
            method::HOOK_READ => result(hooks::read(self.place()?, steery_sdk::params(params)?)?),
            method::HOOK_ANSWER => result(crate::turn::hooks::answer(
                "Goose",
                steery_sdk::params(params)?,
            )?),
            method::HELD => result(drive::held(self.place()?, &steery_sdk::params(params)?)?),
            method::IDLE => result(drive::idle(self.place()?, &steery_sdk::params(params)?)),
            method::MODELS => {
                let models = drive::models(self.place()?).await?;
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
                    self.place()?,
                    &listed,
                    self.signed_out.load(Ordering::Relaxed),
                    steery_sdk::params(params)?,
                )?)
            }
            method::TURN_READ => result(crate::turn::drive::turn_line(
                "Goose",
                steery_sdk::params(params)?,
            )),
            method::STOP => result(crate::turn::drive::stop(
                "Goose",
                &steery_sdk::params(params)?,
            )?),
            method::RECEIPT_MARK => {
                result(drive::mark(self.place()?, &steery_sdk::params(params)?)?)
            }
            method::RECEIPT_FIND => {
                result(drive::find(self.place()?, &steery_sdk::params(params)?)?)
            }
            method::TERMINAL => result(drive::terminal(
                self.place()?,
                &steery_sdk::params(params)?,
            )?),
            method::MCP_LIST => result(config::mcp(self.place()?, steery_sdk::params(params)?)),
            _ => Err(Error::Unknown),
        }
    }
}
