//! Steery's driver for Gemini CLI at observe: it reads Gemini's conversations from
//! `<root>/tmp/<project>/chats/`, plans the entries that set Gemini up for Steery, reads
//! its hooks' moments, lists the MCP servers it loads and the models it offers.

mod events;
mod hooks;
mod mcp;
mod models;
mod setup;
mod store;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

use crate::Harness;

/// The oldest release whose chat files, hooks and settings this driver reads.
pub(crate) const MINIMUM: &str = "0.67.0";

/// npm's package of Gemini CLI.
pub(crate) const PACKAGE: &str = "@google/gemini-cli";

/// The driver: what `initialize` said, and the listings it answered.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
    changes: steery_sdk::listing::Changes,
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

    fn info() -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: vec![wire::Level::Observe],
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
                door: None,
                checks_idle: false,
                worktrees: false,
                acts: Vec::<harness::Act>::new(),
                touches: false,
            },
            hooks: hooks::table(),
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
            harness: Some(Box::new(Self::info())),
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
                    "Gemini CLI has no plugin door that sends its records",
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
            method::HOOK_READ => result(hooks::read(steery_sdk::params(params)?)?),
            method::HOOK_ANSWER => result(hooks::answer(steery_sdk::params(params)?)?),
            method::MODELS => {
                let _: wire::Empty = steery_sdk::params(params)?;
                result(models::list(&self.place()?.root).await?)
            }
            _ => Err(Error::Unknown),
        }
    }
}
