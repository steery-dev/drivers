//! Steery's driver for Amazon's Kiro CLI at observe and drive: it reads Kiro's conversations
//! from `~/.kiro/sessions/`, plans the entries that set Kiro up for Steery, reads its hooks'
//! moments, lists the MCP servers and models that Kiro loads, and starts its turns through
//! Kiro's own ACP server (`kiro-cli acp`, [`drive`]).
pub(crate) mod drive;
mod events;
mod hooks;
mod mcp;
mod models;
mod setup;
mod store;

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, listing, method, result};

use crate::Harness;

/// Kiro's executable.
pub(crate) const EXECUTABLE: &str = "kiro-cli";

/// The oldest release whose session files, hooks and ACP server this driver reads.
pub(crate) const MINIMUM: &str = "3.3.0";

const KNOWING: &str = "Your shell's environment holds it: `KIRO_SESSION_ID`, which starts \
`sess_`. It names the conversation that your person is in. Read it with your shell tool when \
you send it, for example `echo $KIRO_SESSION_ID`. Do not read it from your own context. If \
the variable is empty, describe nothing.";

const TRANSCRIPT: &str = "One folder per conversation, `sessions/<bucket>/<id>/`: \
`session.json` holds its title, folder, model and the conversation it forks; \
`messages.jsonl` one record on each line, `{id, timestamp, payload}`, where `payload.type` \
is `user`, `turn_start`, `assistant`, `tool_call`, `tool_result`, `turn_end` and others; \
`sub-executions/<id>.jsonl` the records of each sub-agent.";

/// The driver: where Kiro keeps everything, the listings it answered, to answer the next
/// with only what changed, and the models it last listed, which `start` checks a pick
/// against.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
    changes: listing::Changes,
    listed: Mutex<Vec<harness::Model>>,
}

/// The person's home, Kiro's folder, `~/.kiro`, and its conversations.
pub(crate) struct Place {
    pub home: PathBuf,
    pub root: PathBuf,
    store: store::Store,
}

/// Kiro's folder: `~/.kiro`.
pub(crate) fn root(home: &Path) -> PathBuf {
    home.join(".kiro")
}

impl Driver {
    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }

    fn info(protocol: u32) -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: vec![wire::Level::Observe, wire::Level::Drive],
            minimum: Some(wire::Minimum {
                release: MINIMUM.into(),
                update: Some(format!("{EXECUTABLE} update")),
            }),
            sign_in: Some(format!("run `{EXECUTABLE} login` in a terminal")),
            knowing: KNOWING.into(),
            lines: wire::Lines::Agent,
            hosted: Default::default(),
            transcript: Some(TRANSCRIPT.into()),
            identity: wire::Identity {
                executables: vec![EXECUTABLE.into()],
                package: None,
                session_variable: Some("KIRO_SESSION_ID".into()),
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
                    model: false,
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
            hooks: hooks::table(protocol),
        }
    }

    /// The models `start` checks a pick against: the last listed, else a fresh list.
    async fn listed(
        &self,
        place: &Place,
        start: &harness::Start,
    ) -> Result<Vec<harness::Model>, Error> {
        if start.model.is_none() && start.thinking.is_none() {
            return Ok(Vec::new());
        }
        let listed = self
            .listed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if !listed.is_empty() {
            return Ok(listed);
        }
        let models = models::list(&place.home, &place.root).await?.models;
        *self.listed.lock().unwrap_or_else(|e| e.into_inner()) = models.clone();
        Ok(models)
    }
}

impl Program for Driver {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let _ = self.place.set(Place {
            home: params.home.clone(),
            root: root(&params.home),
            store: store::Store::new(&root(&params.home)),
        });
        self.changes.picked(protocol);
        Ok(wire::Initialized {
            protocol,
            program: Harness::Kiro.program()?,
            harness: Some(Box::new(Self::info(protocol))),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let place = self.place()?;
        match method {
            method::SETUP_PLAN => {
                result(setup::plan(&place.home, &place.root, steery_sdk::params(params)?).await)
            }
            method::SESSIONS_LIST => {
                let asked: harness::List = steery_sdk::params(params)?;
                let after = asked.after.clone();
                result(
                    self.changes
                        .answer(after.as_deref(), place.store.list(asked)),
                )
            }
            method::SESSIONS_LOCATE => result(place.store.locate(steery_sdk::params(params)?)?),
            method::SESSIONS_READ => result(place.store.read(steery_sdk::params(params)?)?),
            method::HOOK_READ => {
                let read: harness::HookRead = steery_sdk::params(params)?;
                if hooks::of_turn(&read.event) {
                    result(hooks::turn(&place.store, read)?)
                } else {
                    result(hooks::read(&place.store, read)?)
                }
            }
            method::HOOK_ANSWER => {
                let answer: harness::HookAnswer = steery_sdk::params(params)?;
                if hooks::of_turn(&answer.event) {
                    result(crate::turn::hooks::answer(drive::LABEL, answer)?)
                } else {
                    result(hooks::answer(answer)?)
                }
            }
            method::MCP_LIST => result(mcp::list(&place.root, steery_sdk::params(params)?)),
            method::MODELS => {
                let _: wire::Empty = steery_sdk::params(params)?;
                let models = models::list(&place.home, &place.root).await?;
                *self.listed.lock().unwrap_or_else(|e| e.into_inner()) = models.models.clone();
                result(models)
            }
            method::START => {
                let start: harness::Start = steery_sdk::params(params)?;
                let listed = self.listed(place, &start).await?;
                result(drive::start(
                    &place.home,
                    &place.root,
                    &place.store,
                    &listed,
                    start,
                )?)
            }
            method::TURN_READ => result(crate::turn::drive::turn_line(
                drive::LABEL,
                steery_sdk::params(params)?,
            )),
            method::HELD => result(drive::held(&place.store, &steery_sdk::params(params)?)?),
            method::IDLE => result(drive::idle(&place.store, steery_sdk::params(params)?)?),
            method::STOP => result(crate::turn::drive::stop(
                drive::LABEL,
                &steery_sdk::params(params)?,
            )?),
            method::RECEIPT_MARK => {
                result(drive::mark(&place.store, &steery_sdk::params(params)?)?)
            }
            method::RECEIPT_FIND => {
                result(drive::find(&place.store, &steery_sdk::params(params)?)?)
            }
            method::TERMINAL => {
                result(drive::terminal(&place.store, &steery_sdk::params(params)?)?)
            }
            _ => Err(Error::Unknown),
        }
    }
}
