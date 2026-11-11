//! Steery's driver for Cline's CLI at live: it reads Cline's conversations from its SQLite
//! list and each one's messages file, plans the entries that set Cline up for Steery, lists
//! the MCP servers Cline loads, starts Cline's turns through its own ACP server, reads the
//! permissions those turns ask for, and steers a conversation that a person's Cline window
//! shows through Cline's hub ([`live`]).

mod config;
pub(crate) mod drive;
mod events;
mod hooks;
mod live;
mod place;
mod setup;
mod store;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

use crate::Harness;
use place::Place;

/// The release whose files this driver reads: the first that the driver ran against.
const MINIMUM: &str = "3.0.74";

/// How an agent in Cline learns its conversation's id, as Steery's skill says it.
const KNOWING: &str = "Cline gives no identifier of its own to the agent, and the Steery\n\
server cannot name the conversation either. If the instructions that the server\n\
announces name a `harness` and `external_id`, send exactly those values. Otherwise\n\
describe nothing.";

/// How Cline keeps a conversation, as Steery's skill says it.
const TRANSCRIPT: &str = "SQLite lists the conversations: `<CLINE_DIR or ~/.cline>/data/db/sessions.db`, \
table `sessions`. Each conversation's messages are one JSON file that Cline rewrites whole, \
`data/sessions/<id>/<id>.messages.json`: `messages`, each with `role` and Anthropic content \
blocks (`text`, `thinking`, `tool_use`, `tool_result`).";

/// The driver: where Cline keeps everything, the listings it answered, to answer the next
/// with only what changed, Cline's models as `models` last listed them, which `start` and
/// `pick` check a pick against, whether that listing found Cline signed out, and the door
/// of Cline's hub.
pub struct Driver {
    place: OnceLock<Place>,
    changes: steery_sdk::listing::Changes,
    listed: Mutex<Vec<harness::Model>>,
    signed_out: AtomicBool,
    live: Arc<live::Live>,
}

impl Driver {
    pub fn new(notifier: steery_sdk::Notifier) -> Self {
        Self {
            place: OnceLock::new(),
            changes: Default::default(),
            listed: Mutex::default(),
            signed_out: AtomicBool::new(false),
            live: Arc::new(live::Live::new(notifier)),
        }
    }

    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }

    fn listed(&self) -> Vec<harness::Model> {
        self.listed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Cline as Steery reads it, under the protocol picked.
fn info(protocol: u32) -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: vec![wire::Level::Observe, wire::Level::Drive, wire::Level::Live],
        minimum: Some(wire::Minimum {
            release: MINIMUM.into(),
            update: Some("cline update".into()),
        }),
        sign_in: Some("run `cline auth` in a terminal".into()),
        knowing: KNOWING.into(),
        lines: wire::Lines::Agent,
        hosted: Default::default(),
        transcript: Some(TRANSCRIPT.into()),
        identity: wire::Identity {
            executables: vec!["cline".into()],
            package: None,
            session_variable: None,
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
            // A window that joined Cline's hub takes words, a model and a stop through it.
            door: Some(harness::LiveDoor::Picks),
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
            program: Harness::Cline.program()?,
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
                let read = store::Store::new(&db)
                    .read(steery_sdk::params(params)?, |_| config::server_names(place))?;
                result(read)
            }
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Cline has no plugin door that sends its records",
                ))
            }
            method::HOOK_READ => result(hooks::read(self.place()?, steery_sdk::params(params)?)?),
            method::HOOK_ANSWER => result(crate::turn::hooks::answer(
                "Cline",
                steery_sdk::params(params)?,
            )?),
            method::HELD => {
                let held: harness::Held = steery_sdk::params(params)?;
                match self.live.held(self.place()?, &held.external_id).await {
                    Some(holder) => result(harness::HeldBy::Holder(holder)),
                    None => result(drive::held(self.place()?, &held)?),
                }
            }
            method::SAY => result(
                self.live
                    .say(self.place()?, steery_sdk::params(params)?)
                    .await?,
            ),
            method::PICK => result(
                self.live
                    .pick(self.place()?, &self.listed(), steery_sdk::params(params)?)
                    .await?,
            ),
            method::IDLE => result(drive::idle(self.place()?, &steery_sdk::params(params)?)),
            method::MODELS => {
                let models = drive::models(self.place()?).await?;
                *self.listed.lock().unwrap_or_else(|e| e.into_inner()) = models.models.clone();
                self.signed_out
                    .store(models.signed_in == Some(false), Ordering::Relaxed);
                result(models)
            }
            method::START => result(drive::start(
                self.place()?,
                &self.listed(),
                self.signed_out.load(Ordering::Relaxed),
                steery_sdk::params(params)?,
            )?),
            method::TURN_READ => result(crate::turn::drive::turn_line(
                "Cline",
                steery_sdk::params(params)?,
            )),
            method::STOP => {
                let stop: harness::Stop = steery_sdk::params(params)?;
                match stop.holder {
                    harness::Holder::Person {
                        stop: harness::HolderStop::Owner(_),
                        ..
                    } => result(self.live.stop(self.place()?, &stop).await?),
                    _ => result(crate::turn::drive::stop("Cline", &stop)?),
                }
            }
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
