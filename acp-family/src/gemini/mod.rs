//! Steery's driver for Gemini CLI at drive and live: it reads Gemini's conversations from
//! `<root>/tmp/<project>/chats/`, plans the entries that set Gemini up for Steery, reads
//! its hooks' moments, lists the MCP servers it loads and the models it offers, and starts
//! Gemini's turns through its own ACP server, each kept behind Steery's plugin door
//! ([`drive`]).

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
use steery_sdk::{Error, Program, method, refusal, result};

use crate::Harness;

/// The oldest release whose chat files, hooks and settings this driver reads.
pub(crate) const MINIMUM: &str = "0.67.0";

/// npm's package of Gemini CLI.
pub(crate) const PACKAGE: &str = "@google/gemini-cli";

/// The driver: what `initialize` said, the listings it answered, and the models Gemini
/// listed last.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
    changes: steery_sdk::listing::Changes,
    listed: Mutex<Vec<harness::Model>>,
}

/// The person's home, Gemini's root, and what was read of its chat files.
pub(crate) struct Place {
    pub home: PathBuf,
    pub root: PathBuf,
    store: store::Store,
}

/// Gemini's root: `.gemini` in `$GEMINI_CLI_HOME`, which replaces the home folder for
/// Gemini alone, else in the person's home.
pub(crate) fn root(home: &Path) -> PathBuf {
    std::env::var_os("GEMINI_CLI_HOME")
        .filter(|home| !home.is_empty())
        .map_or_else(|| home.to_path_buf(), PathBuf::from)
        .join(".gemini")
}

/// The `gemini` that Steery runs: the first on `PATH`.
pub(crate) fn binary() -> Option<PathBuf> {
    steery_driver_common::program::on_path("gemini")
}

impl Driver {
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
            levels: wire::Level::ALL.to_vec(),
            minimum: Some(wire::Minimum {
                release: MINIMUM.into(),
                update: Some(format!("npm install -g {PACKAGE}@latest")),
            }),
            sign_in: Some("run `gemini` in a terminal and choose how to sign in".into()),
            knowing: "Gemini CLI gives no identifier of its own to the agent. The Steery server\n\
                      names the identifier when the connection names the conversation: the\n\
                      instructions that the server announces hold that `harness` and\n\
                      `external_id`. Read them there and send exactly those values. If the\n\
                      instructions name no conversation, describe nothing."
                .into(),
            lines: wire::Lines::Agent,
            hosted: Default::default(),
            transcript: Some(
                "One JSON record on each line of `session-<time>-<id>.jsonl`: the \
                 conversation's metadata first, then each message, and records that set \
                 metadata, rewrite or remove messages (`$patch`), or rewind to a message \
                 (`$rewindTo`)."
                    .into(),
            ),
            identity: wire::Identity {
                executables: vec!["gemini".into()],
                package: Some(PACKAGE.into()),
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
                acts: vec![harness::Act::Continue, harness::Act::New],
                touches: false,
            },
            // Gemini's own hooks, and the permissions a turn of this driver's asks for.
            hooks: hooks::table()
                .into_iter()
                .chain(
                    crate::turn::hooks::table(protocol)
                        .into_iter()
                        .filter(|hook| hook.event == crate::turn::hooks::PERMISSION),
                )
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
            program: Harness::Gemini.program()?,
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
                    "Gemini CLI's door sends no records: its chat file has them",
                ))
            }
            method::SETUP_PLAN => {
                result(setup::plan(&self.place()?.root, steery_sdk::params(params)?).await)
            }
            method::MCP_LIST => {
                let place = self.place()?;
                result(mcp::list(
                    &place.home,
                    &place.root,
                    steery_sdk::params(params)?,
                )?)
            }
            method::HOOK_READ => {
                let read: harness::HookRead = steery_sdk::params(params)?;
                if read.event == crate::turn::hooks::PERMISSION {
                    result(drive::permission(self.store()?, &read)?)
                } else {
                    result(hooks::read(read)?)
                }
            }
            method::HOOK_ANSWER => {
                let answer: harness::HookAnswer = steery_sdk::params(params)?;
                if answer.event == crate::turn::hooks::PERMISSION {
                    result(crate::turn::hooks::answer("Gemini CLI", answer)?)
                } else {
                    result(hooks::answer(answer)?)
                }
            }
            method::MODELS => {
                let _: wire::Empty = steery_sdk::params(params)?;
                let models = models::list(&self.place()?.root).await?;
                *self.listed.lock().unwrap_or_else(|e| e.into_inner()) = models.models.clone();
                result(models)
            }
            method::HELD => result(drive::held(self.store()?, &steery_sdk::params(params)?)?),
            method::IDLE => result(drive::idle(self.store()?, &steery_sdk::params(params)?)),
            method::START => {
                let listed = self
                    .listed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let place = self.place()?;
                result(drive::start(
                    &place.root,
                    &place.store,
                    &listed,
                    steery_sdk::params(params)?,
                )?)
            }
            method::TURN_READ => result(crate::turn::drive::turn_line(
                "Gemini CLI",
                steery_sdk::params(params)?,
            )),
            method::STOP => result(crate::turn::drive::stop(
                "Gemini CLI",
                &steery_sdk::params(params)?,
            )?),
            method::RECEIPT_MARK => {
                result(drive::mark(self.store()?, &steery_sdk::params(params)?)?)
            }
            method::RECEIPT_FIND => {
                result(drive::find(self.store()?, &steery_sdk::params(params)?)?)
            }
            method::TERMINAL => result(drive::terminal(
                self.store()?,
                &steery_sdk::params(params)?,
            )?),
            _ => Err(Error::Unknown),
        }
    }
}
