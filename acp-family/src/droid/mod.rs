//! Steery's driver for Factory's Droid at observe: it reads Droid's conversations from
//! `~/.factory/sessions/`, plans the entries that set Droid up for Steery, reads its hooks'
//! moments, and lists the MCP servers that Droid loads.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, listing};

use crate::Harness;

/// Droid's executable.
pub(crate) const EXECUTABLE: &str = "droid";

/// The oldest release whose session files, hooks and settings this driver reads.
pub(crate) const MINIMUM: &str = "1.9.0";

const KNOWING: &str = "Your shell's environment holds it: `FACTORY_SESSION_ID`, which is \
already a UUID. It names the conversation that your person is in. Read it with your shell \
tool when you send it, for example `echo $FACTORY_SESSION_ID`. Do not read it from your own \
context. If the variable is empty, describe nothing.";

const TRANSCRIPT: &str = "One JSON record on each line of `<id>.jsonl`: `session_start` \
first (title, folder, and the conversation it forks), then each `message`, which names its \
parent by `parentId`, and an `agent_turn_outcome` after each turn, which names the prompt \
that began it by `turnId`. `<id>.settings.json` beside it holds the model and the tokens \
spent.";

/// The driver: where Droid keeps everything, and the listings it answered, to answer the
/// next with only what changed.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
    changes: listing::Changes,
}

/// The person's home and Droid's folder, `~/.factory`.
pub(crate) struct Place {
    pub home: PathBuf,
    pub root: PathBuf,
}

/// Droid's folder: `~/.factory`, as `droid doctor` reports it (`factoryDir`).
pub(crate) fn root(home: &Path) -> PathBuf {
    home.join(".factory")
}

impl Driver {
    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }

    fn info() -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: vec![wire::Level::Observe],
            minimum: Some(wire::Minimum {
                release: MINIMUM.into(),
                update: Some(format!("{EXECUTABLE} update")),
            }),
            // A custom model in Droid's settings runs with no Factory account; Factory's own
            // models need one.
            sign_in: Some(format!(
                "run `{EXECUTABLE}` in a terminal and sign in, or set `FACTORY_API_KEY`; a \
                 custom model of your own runs without a Factory account"
            )),
            knowing: KNOWING.into(),
            lines: wire::Lines::Agent,
            hosted: Default::default(),
            transcript: Some(TRANSCRIPT.into()),
            identity: wire::Identity {
                executables: vec![EXECUTABLE.into()],
                package: None,
                session_variable: Some("FACTORY_SESSION_ID".into()),
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
                acts: Vec::<harness::Act>::new(),
                touches: false,
            },
            hooks: Vec::new(),
        }
    }
}

impl Program for Driver {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let _ = self.place.set(Place {
            home: params.home.clone(),
            root: root(&params.home),
        });
        self.changes.picked(protocol);
        Ok(wire::Initialized {
            protocol,
            program: Harness::Droid.program()?,
            harness: Some(Box::new(Self::info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, _params: Value) -> Result<Value, Error> {
        let _ = (method, self.place()?);
        Err(Error::Unknown)
    }
}
