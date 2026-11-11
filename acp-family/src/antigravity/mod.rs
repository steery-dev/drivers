//! Steery's driver for Google's Antigravity CLI at live: it reads Antigravity's
//! conversations from `<root>/brain/<id>/`, plans the entries that set Antigravity up for
//! Steery, reads its hooks' moments, lists the MCP servers it loads and the models it
//! offers, starts Antigravity's turns through its own ACP server, and reaches a conversation
//! open in an Antigravity window through that window's own server.
mod door;
pub(crate) mod drive;
mod events;
mod hooks;
mod live;
mod mcp;
mod models;
mod setup;
mod store;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::Harness;
use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

/// The oldest release whose transcripts, hooks and settings this driver reads.
pub(crate) const MINIMUM: &str = "1.5.5";

/// The variable Antigravity sets to the conversation's id for its hooks and for every
/// command its shell tool runs.
pub(crate) const SESSION_VARIABLE: &str = "ANTIGRAVITY_CONVERSATION_ID";

/// The driver: what `initialize` said, the listings it answered, Antigravity's models as
/// `models` last listed them, which `start` checks a pick against, whether that listing
/// found Antigravity signed out, and the Antigravity windows whose own door it follows.
pub struct Driver {
    place: OnceLock<Place>,
    changes: steery_sdk::listing::Changes,
    listed: Mutex<Vec<harness::Model>>,
    signed_out: AtomicBool,
    live: Arc<live::Live>,
}

/// The person's home, Antigravity's root, and what was read of its conversations.
pub(crate) struct Place {
    pub home: PathBuf,
    pub root: PathBuf,
    store: store::Store,
}

/// Antigravity CLI's root: no variable moves it, and its app and IDE keep theirs beside it
/// (`antigravity`, `antigravity-ide`).
pub(crate) fn root(home: &Path) -> PathBuf {
    home.join(".gemini/antigravity-cli")
}

/// The folder Antigravity shares with its app and IDE: hooks, MCP servers, skills.
pub(crate) fn config(home: &Path) -> PathBuf {
    home.join(".gemini/config")
}

/// The `agy` that Steery runs: the first on `PATH`, else where its installer puts it, which
/// a runtime started outside a login shell may not have on its `PATH`.
pub(crate) fn binary(home: &Path) -> Option<PathBuf> {
    steery_driver_common::program::on_path("agy")
        .or_else(|| steery_driver_common::program::executable(home.join(".local/bin/agy")))
}

impl Driver {
    pub fn new(notifier: steery_sdk::Notifier) -> Self {
        Self {
            place: OnceLock::new(),
            changes: steery_sdk::listing::Changes::default(),
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

    fn store(&self) -> Result<&store::Store, Error> {
        Ok(&self.place()?.store)
    }

    fn info(protocol: u32) -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: vec![wire::Level::Observe, wire::Level::Drive, wire::Level::Live],
            minimum: Some(wire::Minimum {
                release: MINIMUM.into(),
                update: Some("agy update".into()),
            }),
            sign_in: Some(
                "run `agy` in a terminal and sign in with Google, or set `GEMINI_API_KEY`".into(),
            ),
            knowing: format!(
                "Antigravity sets `{SESSION_VARIABLE}` to the conversation's id in the\n\
                 environment of every command it runs. Describe with `harness` `antigravity`\n\
                 and `external_id` that value. Where it is unset, describe nothing."
            ),
            lines: wire::Lines::Agent,
            hosted: Default::default(),
            transcript: Some(
                "One JSON step on each line of `brain/<id>/.system_generated/logs/\
                 transcript_full.jsonl`, appended in place: `step_index`, `source`, `type` \
                 (`USER_INPUT`, `PLANNER_RESPONSE`, `GENERIC`, `SYSTEM_MESSAGE`, \
                 `ERROR_MESSAGE`), `status`, `created_at`, and its `content`, `thinking`, \
                 `tool_calls`, token counts or `error`."
                    .into(),
            ),
            identity: wire::Identity {
                executables: vec!["agy".into()],
                package: None,
                session_variable: Some(SESSION_VARIABLE.into()),
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
                    model: false,
                    thinking: false,
                },
                door: Some(harness::LiveDoor::Keeps),
                checks_idle: false,
                worktrees: false,
                acts: vec![harness::Act::Continue, harness::Act::New],
                touches: false,
            },
            // Antigravity's own hooks, and the moments of a turn this driver runs.
            hooks: hooks::table()
                .into_iter()
                .chain(crate::turn::hooks::table(protocol))
                .collect(),
        }
    }
}

impl Program for Driver {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let root = root(&params.home);
        let _ = self.place.set(Place {
            home: params.home.clone(),
            store: store::Store::new(&root),
            root,
        });
        self.changes.picked(protocol);
        Ok(wire::Initialized {
            protocol,
            program: Harness::Antigravity.program()?,
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
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Antigravity CLI has no plugin door that sends its records",
                ))
            }
            method::SETUP_PLAN => {
                let place = self.place()?;
                result(setup::plan(&place.home, &place.root, steery_sdk::params(params)?).await)
            }
            method::MCP_LIST => result(mcp::list(&self.place()?.home, steery_sdk::params(params)?)),
            method::HOOK_READ => {
                let read: harness::HookRead = steery_sdk::params(params)?;
                if [crate::turn::hooks::PERMISSION, crate::turn::hooks::ENDED]
                    .contains(&read.event.as_str())
                {
                    result(hooks::turn(self.store()?, read)?)
                } else {
                    result(hooks::read(&self.place()?.root, read)?)
                }
            }
            method::HOOK_ANSWER => {
                let answer: harness::HookAnswer = steery_sdk::params(params)?;
                if [crate::turn::hooks::PERMISSION, crate::turn::hooks::ENDED]
                    .contains(&answer.event.as_str())
                {
                    result(crate::turn::hooks::answer(drive::LABEL, answer)?)
                } else {
                    result(hooks::answer(answer)?)
                }
            }
            method::MODELS => {
                let _: wire::Empty = steery_sdk::params(params)?;
                let place = self.place()?;
                let models = models::list(&place.home, &place.root).await?;
                *self.listed.lock().unwrap_or_else(|e| e.into_inner()) = models.models.clone();
                self.signed_out
                    .store(models.signed_in == Some(false), Ordering::Relaxed);
                result(models)
            }
            method::HELD => {
                let held: harness::Held = steery_sdk::params(params)?;
                match self.live.held(self.store()?, &held.external_id).await {
                    Some(holder) => result(harness::HeldBy::Holder(holder)),
                    None => result(drive::held(self.place()?, &held)?),
                }
            }
            method::SAY => result(
                self.live
                    .say(self.store()?, steery_sdk::params(params)?)
                    .await?,
            ),
            method::PICK => {
                let _: harness::Pick = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "an Antigravity window runs the model its person picked in it; its server \
                     takes a model only together with words",
                ))
            }
            method::IDLE => result(drive::idle(self.place()?, &steery_sdk::params(params)?)),
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
                drive::LABEL,
                steery_sdk::params(params)?,
            )),
            method::STOP => {
                let stop: harness::Stop = steery_sdk::params(params)?;
                match stop.holder {
                    harness::Holder::Person {
                        stop: harness::HolderStop::Owner(_),
                        ..
                    } => result(self.live.stop(self.store()?, &stop).await?),
                    _ => result(crate::turn::drive::stop(drive::LABEL, &stop)?),
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
            _ => Err(Error::Unknown),
        }
    }
}
