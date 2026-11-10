//! Steery's driver for GitHub Copilot CLI at observe: it reads Copilot's conversations from
//! `$COPILOT_HOME/session-state/<id>/events.jsonl`, plans the entries that set Copilot up for
//! Steery, and reads its hooks' moments. Copilot is reached only through what it publishes:
//! its files, its hooks, its CLI, and its own server (`copilot --headless --stdio`).

mod calls;
mod drive;
mod events;
mod home;
mod hooks;
mod mcp;
mod models;
mod reader;
mod server;
mod setup;
mod sign_in;
mod transcripts;
mod turn;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

use crate::Harness;
pub(crate) use reader::{MODE as READER_MODE, main as reader_main};
pub(crate) use turn::{MODE as TURN_MODE, main as turn_main};

/// Steery's MCP server's name in Copilot's configuration.
pub(crate) const STEERY: &str = "steery";

/// How an agent in Copilot learns its conversation's id, as Steery's skill says it.
/// Steery's `sessionStart` hook gives the agent the id in its `additionalContext`, the one
/// output Copilot reads from that hook.
const KNOWING: &str = "Copilot gives the identifier to the agent when its conversation starts or resumes:\n\
                       Steery's paragraph at the start of the conversation names the `harness` and the\n\
                       `external_id`. Send exactly those values. If no such paragraph is in the\n\
                       conversation, describe nothing.";

/// How Copilot's transcript file reads.
const TRANSCRIPT: &str = "One JSON event on each line of `events.jsonl`, with `id`, `parentId`, `timestamp`, \
                          `type` and `data`. The first is `session.start`. `user.message` and \
                          `assistant.message` hold `data.content`; `tool.execution_start` and \
                          `tool.execution_complete` hold the calls.";

/// The driver: the protocol it picked from `initialize`, where Copilot keeps everything, the
/// sign-ins it started that wait for their person, and the listings it answered, to answer
/// the next with only what changed.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
    sign_ins: sign_in::SignIns,
    turns: turn::Turns,
    changes: steery_sdk::listing::Changes,
}

/// The protocol the driver picked, the person's home, and Copilot's home with its
/// conversations, or why there is none.
struct Place {
    protocol: u32,
    home: PathBuf,
    store: Result<Arc<transcripts::Store>, String>,
}

impl Driver {
    /// Whether the protocol picked has needs.
    fn needs(&self) -> Result<bool, Error> {
        Ok(self.place()?.protocol >= steery_driver_common::program::NEEDS)
    }

    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }

    fn shared(&self) -> Result<&Arc<transcripts::Store>, Error> {
        self.place()?
            .store
            .as_ref()
            .map_err(|why| Error::refused(refusal::REFUSED, why.clone()))
    }

    fn store(&self) -> Result<&transcripts::Store, Error> {
        self.shared().map(|store| &**store)
    }
}

/// Run blocking work off the async threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Error::refused(refusal::REFUSED, e.to_string()))?
}

impl Program for Driver {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let store = home::root(&params.home).map(|root| Arc::new(transcripts::Store::new(root)));
        let _ = self.place.set(Place {
            protocol,
            home: params.home.clone(),
            store,
        });
        self.changes.picked(self.place()?.protocol);
        Ok(wire::Initialized {
            protocol: self.place()?.protocol,
            program: Harness::Copilot.program()?,
            harness: Some(Box::new(info(self.place()?.protocol))),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        use steery_sdk::params as given;
        match method {
            method::SETUP_PLAN => {
                let place = self.place()?;
                result(
                    setup::plan(
                        &place.home,
                        self.store()?.root(),
                        given(params)?,
                        self.needs()?,
                    )
                    .await?,
                )
            }
            method::NEEDS_GIVE if self.needs()? => {
                result(self.sign_ins.give(given(params)?).await?)
            }
            method::SESSIONS_LIST => {
                let asked: harness::List = given(params)?;
                let after = asked.after.clone();
                let store = Arc::clone(self.shared()?);
                let listed = blocking(move || Ok(store.list(asked))).await?;
                result(self.changes.answer(after.as_deref(), listed))
            }
            method::SESSIONS_LOCATE => result(self.store()?.locate(given(params)?)?),
            method::SESSIONS_READ => {
                let store = Arc::clone(self.shared()?);
                let read = given(params)?;
                let mut events = blocking(move || store.read(read)).await?;
                // A runtime on a protocol older than `waited` rejects the field.
                if self.place()?.protocol < steery_driver_common::program::WAITED {
                    for event in &mut events.events {
                        if let harness::Event::CallEnded { waited, .. } = event {
                            waited.clear();
                        }
                    }
                }
                result(events)
            }
            method::SESSIONS_FOLD => {
                let _: harness::Fold = given(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Copilot has no plugin door that sends its records",
                ))
            }
            method::MCP_LIST => result(mcp::list(self.store()?.root(), given(params)?).await?),
            method::HOOK_READ => {
                let store = Arc::clone(self.shared()?);
                let read = given(params)?;
                result(blocking(move || hooks::read(&store, read)).await?)
            }
            method::HOOK_ANSWER => result(hooks::answer(given(params)?)?),
            method::HELD => result(drive::held(self.store()?, given(params)?)?),
            method::IDLE => result(drive::idle(self.store()?, given(params)?)?),
            method::MODELS => result(models::models(self.store()?.root()).await?),
            method::START => result(turn::start(self.store()?, given(params)?)?),
            method::TURN_READ => result(self.turns.read(self.store()?, given(params)?)?),
            method::STOP => result(drive::stop(self.store()?, given(params)?).await?),
            method::RECEIPT_MARK => result(drive::mark(self.store()?, given(params)?)?),
            method::RECEIPT_FIND => result(drive::find(self.store()?, given(params)?)?),
            method::READER => result(reader::plan(given(params)?)?),
            method::READER_READ => result(reader::read(given(params)?)?),
            method::TERMINAL => result(drive::terminal(
                &self.place()?.home,
                self.store()?,
                given(params)?,
            )?),
            _ => Err(Error::Unknown),
        }
    }
}

/// Copilot as Steery reads it, on `protocol`.
fn info(protocol: u32) -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: vec![wire::Level::Observe, wire::Level::Drive],
        // The release whose hooks, event schema and own server this driver was read against.
        minimum: Some(wire::Minimum {
            release: "1.0.107".into(),
            update: Some("copilot update".into()),
        }),
        sign_in: Some("run `copilot login` in a terminal".into()),
        knowing: KNOWING.into(),
        lines: wire::Lines::Agent,
        hosted: Default::default(),
        transcript: Some(TRANSCRIPT.into()),
        identity: wire::Identity {
            executables: vec!["copilot".into()],
            package: Some("@github/copilot".into()),
            session_variable: None,
            call_meta: None,
        },
        setup: setup::facts(),
        reader: Some(wire::ReaderFacts {
            sealed: wire::Sealed {
                tools: true,
                // A user's MCP servers still start, but `--excluded-tools mcp:*` or
                // `--available-tools` leaves the read none of their tools.
                mcp: true,
                instructions: true,
                // Plugins and their hooks still load; Copilot has no flag that shuts them out.
                extensions: false,
                history: true,
            },
            known: Vec::new(),
            unnamed: true,
        }),
        facts: wire::HarnessFacts {
            touches: false,
            // `--resume=<id>` names a conversation by its id.
            by_file: false,
            // `--resume` runs in the folder the conversation recorded.
            resumes_in_place: true,
            forks_beside: false,
            // `session.resume` restores the conversation's `selectedModel` and
            // `reasoningEffort`.
            keeps: wire::Keeps {
                model: true,
                thinking: true,
            },
            door: None,
            checks_idle: false,
            worktrees: false,
            acts: vec![
                harness::Act::Continue,
                harness::Act::Fork,
                harness::Act::New,
            ],
        },
        hooks: hooks::table(protocol),
    }
}
