//! Steery's driver for Amazon's Kiro CLI at observe and drive: it reads Kiro's conversations
//! from `~/.kiro/sessions/`, and starts its turns through Kiro's own ACP server
//! (`kiro-cli acp`, [`drive`]).
pub(crate) mod drive;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program};

use crate::Harness;

/// Kiro's executable.
pub(crate) const EXECUTABLE: &str = "kiro-cli";

/// The oldest release whose session files, hooks and ACP server this driver reads.
pub(crate) const MINIMUM: &str = "3.3.0";

const KNOWING: &str = "Your shell's environment holds it: `KIRO_SESSION_ID`, which starts \
`sess_`. It names the conversation that your person is in. Read it with your shell tool when \
you send it, for example `echo $KIRO_SESSION_ID`. Do not read it from your own context. If \
the variable is empty, describe nothing.";

/// The driver: where Kiro keeps everything.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
}

/// The person's home and Kiro's folder, `~/.kiro`.
pub(crate) struct Place {
    pub home: PathBuf,
    pub root: PathBuf,
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
            transcript: None,
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
            hooks: crate::turn::hooks::table(protocol),
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
        Ok(wire::Initialized {
            protocol,
            program: Harness::Kiro.program()?,
            harness: Some(Box::new(Self::info(protocol))),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, _params: Value) -> Result<Value, Error> {
        let _ = self.place()?;
        let _ = method;
        Err(Error::Unknown)
    }
}
