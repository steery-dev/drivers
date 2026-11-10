//! Steery's driver for Grok Build at observe and drive: it reads Grok's conversations from
//! `$GROK_HOME/sessions/<encoded folder>/<id>/`, plans the entries that set Grok up for
//! Steery, reads its hooks' moments, and plans the turns that the runtime runs as Grok.

mod drive;
mod events;
mod hooks;
mod mcp;
mod models;
mod setup;
mod sign_in;
mod store;
mod turn;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

use crate::Harness;

/// The driver: the protocol it picked from `initialize`, where Grok keeps everything, the
/// listings it answered, to answer the next with only what changed, and the sign-in that
/// waits for its person.
#[derive(Default)]
pub struct Driver {
    protocol: OnceLock<u32>,
    place: OnceLock<Place>,
    changes: steery_sdk::listing::Changes,
    sign_ins: sign_in::SignIns,
    turns: turn::Turns,
}

/// The person's home, and Grok's home.
pub(crate) struct Place {
    pub home: PathBuf,
    pub root: PathBuf,
}

/// Grok's home: `$GROK_HOME`, else `<home>/.grok`.
pub(crate) fn root(home: &Path) -> PathBuf {
    std::env::var_os("GROK_HOME")
        .filter(|root| !root.is_empty())
        .map_or_else(|| home.join(".grok"), PathBuf::from)
}

/// The `grok` that Steery runs: the one on `PATH`, else the one its installer links in
/// `<root>/bin`, a folder that is not yet on `PATH` right after an install.
pub(crate) fn binary(root: &Path) -> Option<PathBuf> {
    steery_driver_common::program::on_path("grok")
        .or_else(|| steery_driver_common::program::executable(root.join("bin").join("grok")))
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

    fn info() -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: vec![wire::Level::Observe, wire::Level::Drive],
            minimum: Some(wire::Minimum {
                release: "1.0.77".into(),
                update: Some("grok update".into()),
            }),
            sign_in: Some("run `grok login` in a terminal".into()),
            knowing: "Grok Build gives no identifier of its own to the agent. The Steery server\n\
                      names the identifier when the connection names the conversation: the\n\
                      instructions that the server announces hold that `harness` and\n\
                      `external_id`. Read them there and send exactly those values. If the\n\
                      instructions name no conversation, describe nothing."
                .into(),
            lines: wire::Lines::Agent,
            hosted: Default::default(),
            transcript: Some(
                "One JSON record on each line of `updates.jsonl`: an ACP `session/update` \
                 notification with its time, appended as the conversation goes."
                    .into(),
            ),
            identity: wire::Identity {
                executables: vec!["grok".into()],
                package: None,
                session_variable: None,
                call_meta: None,
            },
            setup: wire::SetupFacts {
                reads_skills: true,
                cuts_tool_text: false,
                call_bound: None,
            },
            // `--tools ""` allows no built-in tool, `--no-subagents` and
            // `--disable-web-search` reach nothing beyond. Grok 1.0.77 has no flag that
            // leaves out MCP servers, rules files, hooks, or the saved run.
            reader: Some(wire::ReaderFacts {
                sealed: wire::Sealed {
                    tools: true,
                    mcp: false,
                    instructions: false,
                    extensions: false,
                    history: false,
                },
                known: Vec::new(),
                unnamed: true,
            }),
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
                acts: vec![harness::Act::Continue, harness::Act::Fork, harness::Act::New],
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
        let _ = self.protocol.set(protocol);
        let _ = self.place.set(Place {
            home: params.home.clone(),
            root: root(&params.home),
        });
        self.changes.picked(protocol);
        Ok(wire::Initialized {
            protocol,
            program: Harness::Grok.program()?,
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
            method::SETUP_PLAN => result(
                setup::plan(
                    &self.place()?.root,
                    steery_sdk::params(params)?,
                    self.needs(),
                )
                .await,
            ),
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Grok Build has no plugin door that sends its records",
                ))
            }
            method::MCP_LIST => {
                let place = self.place()?;
                result(mcp::list(&place.home, &place.root, steery_sdk::params(params)?).await?)
            }
            method::HOOK_READ => result(hooks::read(steery_sdk::params(params)?)?),
            method::HELD => result(drive::held(
                &self.place()?.root,
                &self.store()?,
                steery_sdk::params(params)?,
            )),
            method::IDLE => result(drive::idle(&self.store()?, steery_sdk::params(params)?)),
            method::START => result(
                drive::start(
                    &self.place()?.root,
                    &self.store()?,
                    steery_sdk::params(params)?,
                )
                .await?,
            ),
            method::TURN_READ => result(
                self.turns
                    .read(&self.store()?, steery_sdk::params(params)?),
            ),
            method::STOP => result(drive::stop(steery_sdk::params(params)?)?),
            method::RECEIPT_MARK => result(drive::mark(steery_sdk::params(params)?)?),
            method::RECEIPT_FIND => result(drive::find(steery_sdk::params(params)?)?),
            method::READER => result(drive::reader(
                &self.place()?.root,
                steery_sdk::params(params)?,
            )?),
            method::TERMINAL => {
                let place = self.place()?;
                result(drive::terminal(
                    &place.home,
                    &place.root,
                    &self.store()?,
                    steery_sdk::params(params)?,
                )?)
            }
            method::HOOK_ANSWER => result(hooks::answer(steery_sdk::params(params)?)?),
            method::MODELS => {
                let _: wire::Empty = steery_sdk::params(params)?;
                result(models::list(&self.place()?.root).await?)
            }
            method::NEEDS_GIVE if self.needs() => {
                let give: harness::Give = steery_sdk::params(params)?;
                result(self.sign_ins.give(&self.place()?.root, give).await?)
            }
            _ => Err(Error::Unknown),
        }
    }
}
