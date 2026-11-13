//! Steery's driver for OpenClaw at live: it reads OpenClaw's conversations from each
//! agent's SQLite store, `<state>/agents/<agent>/agent/openclaw-agent.sqlite`, plans the
//! entries that set OpenClaw up for Steery, lists the MCP servers OpenClaw loads, runs
//! OpenClaw's turns through `openclaw acp` ([`drive`]), and reaches a conversation that
//! runs on OpenClaw's gateway through the gateway's WebSocket ([`live`]).

pub mod drive;
mod events;
mod hooks;
mod live;
mod mcp;
mod models;
mod setup;
mod sign_in;
mod store;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

use crate::Harness;

/// The driver: the protocol it picked from `initialize`, where OpenClaw keeps everything,
/// the listings it answered, to answer the next with only what changed, the sign-in that
/// waits for its person, the models `models` last listed, which `start` and `pick` check a
/// pick against, and the door of OpenClaw's gateway.
pub struct Driver {
    protocol: OnceLock<u32>,
    place: OnceLock<Place>,
    changes: steery_sdk::listing::Changes,
    sign_ins: sign_in::SignIns,
    listed: Mutex<Vec<harness::Model>>,
    live: Arc<live::Live>,
}

/// The person's home, and OpenClaw's state folder.
pub(crate) struct Place {
    pub home: PathBuf,
    pub root: PathBuf,
}

/// OpenClaw's state folder: `$OPENCLAW_STATE_DIR` (trimmed, `~` expanded), else
/// `<home>/.openclaw` (`src/config/state-dir.ts`).
pub(crate) fn root(home: &Path) -> PathBuf {
    let given = std::env::var("OPENCLAW_STATE_DIR").unwrap_or_default();
    let given = given.trim();
    if given.is_empty() {
        return home.join(".openclaw");
    }
    match given.strip_prefix('~') {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(given),
    }
}

impl Driver {
    pub fn new(notifier: steery_sdk::Notifier) -> Self {
        Self {
            protocol: OnceLock::new(),
            place: OnceLock::new(),
            changes: steery_sdk::listing::Changes::default(),
            sign_ins: sign_in::SignIns,
            listed: Mutex::default(),
            live: Arc::new(live::Live::new(notifier)),
        }
    }

    fn listed(&self) -> Vec<harness::Model> {
        self.listed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Driver {
    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }

    fn store(&self) -> Result<store::Store, Error> {
        Ok(store::Store::new(&self.place()?.root))
    }

    /// Whether the protocol picked has needs.
    fn needs(&self) -> bool {
        self.protocol
            .get()
            .is_some_and(|&protocol| protocol >= steery_driver_common::program::NEEDS)
    }

    fn info(protocol: u32) -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: vec![wire::Level::Observe, wire::Level::Drive, wire::Level::Live],
            minimum: Some(wire::Minimum {
                release: setup::MINIMUM_TEXT.into(),
                update: Some("openclaw update".into()),
            }),
            sign_in: Some("run `openclaw onboard` in a terminal".into()),
            knowing: "OpenClaw gives no identifier of its own to the agent. The Steery server\n\
                      names the identifier when the connection names the conversation: the\n\
                      instructions that the server announces hold that `harness` and\n\
                      `external_id`. Read them there and send exactly those values. If the\n\
                      instructions name no conversation, describe nothing."
                .into(),
            lines: wire::Lines::Agent,
            hosted: Default::default(),
            transcript: Some(
                "Rows of `transcript_events` in the agent's `openclaw-agent.sqlite`, one JSON \
                 record each (zstd-packed when large), each naming its parent record."
                    .into(),
            ),
            identity: wire::Identity {
                executables: vec!["openclaw".into()],
                package: Some("openclaw".into()),
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
                forks_beside: true,
                keeps: wire::Keeps {
                    model: true,
                    thinking: true,
                },
                // OpenClaw's gateway takes words, a model and a stop for a session that it
                // runs, whichever client began it.
                door: Some(harness::LiveDoor::Picks),
                checks_idle: true,
                worktrees: false,
                // `openclaw acp` resumes a key and makes one anew; it has no fork.
                acts: vec![harness::Act::Continue, harness::Act::New],
                touches: false,
            },
            // A turn of this driver's runs Steery's hook at each permission `openclaw acp`
            // relays, and at its end.
            hooks: crate::turn::hooks::table(protocol),
        }
    }
}

impl Program for Driver {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let _ = self.protocol.set(protocol);
        let _ = self.place.set(Place {
            home: params.home.clone(),
            root: root(&params.home),
        });
        self.changes.picked(protocol);
        Ok(wire::Initialized {
            protocol,
            program: Harness::OpenClaw.program()?,
            harness: Some(Box::new(Self::info(protocol))),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::SESSIONS_LIST => {
                let asked: harness::List = steery_sdk::params(params)?;
                let after = asked.after.clone();
                result(
                    self.changes
                        .answer(after.as_deref(), self.store()?.list(asked)),
                )
            }
            method::SESSIONS_LOCATE => result(self.store()?.locate(steery_sdk::params(params)?)?),
            method::SESSIONS_READ => result(self.store()?.read(steery_sdk::params(params)?)?),
            method::SETUP_PLAN => {
                let place = self.place()?;
                result(
                    setup::plan(
                        &place.home,
                        &place.root,
                        steery_sdk::params(params)?,
                        self.needs(),
                    )
                    .await,
                )
            }
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "OpenClaw has no plugin door that sends its records",
                ))
            }
            method::MCP_LIST => {
                let place = self.place()?;
                result(mcp::list(&place.home, &place.root, steery_sdk::params(params)?).await?)
            }
            method::MODELS => {
                let _: wire::Empty = steery_sdk::params(params)?;
                let place = self.place()?;
                let models = models::list(&place.home, &place.root).await?;
                *self.listed.lock().unwrap_or_else(|e| e.into_inner()) = models.models.clone();
                result(models)
            }
            method::START => {
                let place = self.place()?;
                result(drive::start(
                    &place.home,
                    &place.root,
                    &self.listed(),
                    steery_sdk::params(params)?,
                )?)
            }
            method::HELD => {
                let root = &self.place()?.root;
                let held: harness::Held = steery_sdk::params(params)?;
                match self.live.held(root, &held.external_id).await {
                    Some(holder) => result(harness::HeldBy::Holder(holder)),
                    None => result(drive::held(root, &held)?),
                }
            }
            method::SAY => result(
                self.live
                    .say(&self.place()?.root, steery_sdk::params(params)?)
                    .await?,
            ),
            method::PICK => result(
                self.live
                    .pick(
                        &self.place()?.root,
                        &self.listed(),
                        steery_sdk::params(params)?,
                    )
                    .await?,
            ),
            method::IDLE => result(drive::idle(
                &self.place()?.root,
                &steery_sdk::params(params)?,
            )),
            method::TURN_READ => result(drive::turn_line(
                &self.place()?.root,
                steery_sdk::params(params)?,
            )),
            method::STOP => {
                let stop: harness::Stop = steery_sdk::params(params)?;
                match stop.holder {
                    harness::Holder::Person {
                        stop: harness::HolderStop::Owner(_),
                        ..
                    } => result(self.live.stop(&self.place()?.root, &stop).await?),
                    _ => result(crate::turn::drive::stop("OpenClaw", &stop)?),
                }
            }
            method::RECEIPT_MARK => result(drive::mark(
                &self.place()?.root,
                &steery_sdk::params(params)?,
            )?),
            method::RECEIPT_FIND => result(drive::find(
                &self.place()?.root,
                &steery_sdk::params(params)?,
            )?),
            method::TERMINAL => result(drive::terminal(
                &self.place()?.root,
                &steery_sdk::params(params)?,
            )?),
            method::HOOK_READ => result(hooks::read(
                &self.place()?.root,
                steery_sdk::params(params)?,
            )?),
            method::HOOK_ANSWER => result(crate::turn::hooks::answer(
                "OpenClaw",
                steery_sdk::params(params)?,
            )?),
            method::NEEDS_GIVE if self.needs() => {
                let give: harness::Give = steery_sdk::params(params)?;
                result(self.sign_ins.give(&self.place()?.home, give).await?)
            }
            _ => Err(Error::Unknown),
        }
    }
}
