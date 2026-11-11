//! Steery's driver for Google's Antigravity CLI at observe: it reads Antigravity's
//! conversations from `<root>/brain/<id>/`, plans the entries that set Antigravity up for
//! Steery, reads its hooks' moments, lists the MCP servers it loads and the models it
//! offers.
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

/// The oldest release whose transcripts, hooks and settings this driver reads.
pub(crate) const MINIMUM: &str = "1.5.5";

/// The variable Antigravity sets to the conversation's id for its hooks and for every
/// command its shell tool runs.
pub(crate) const SESSION_VARIABLE: &str = "ANTIGRAVITY_CONVERSATION_ID";

/// The driver: what `initialize` said, and the listings it answered.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
    changes: steery_sdk::listing::Changes,
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
            program: Harness::Antigravity.program()?,
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
                    "Antigravity CLI has no plugin door that sends its records",
                ))
            }
            method::SETUP_PLAN => {
                let place = self.place()?;
                result(setup::plan(&place.home, &place.root, steery_sdk::params(params)?).await)
            }
            method::MCP_LIST => result(mcp::list(&self.place()?.home, steery_sdk::params(params)?)),
            method::HOOK_READ => result(hooks::read(
                &self.place()?.root,
                steery_sdk::params(params)?,
            )?),
            method::HOOK_ANSWER => result(hooks::answer(steery_sdk::params(params)?)?),
            method::MODELS => {
                let _: wire::Empty = steery_sdk::params(params)?;
                let place = self.place()?;
                result(models::list(&place.home, &place.root).await?)
            }
            _ => Err(Error::Unknown),
        }
    }
}
