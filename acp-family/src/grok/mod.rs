//! Steery's driver for Grok Build at observe: it reads Grok's conversations from
//! `$GROK_HOME/sessions/<encoded folder>/<id>/`, plans the entries that set Grok up for
//! Steery, and reads its hooks' moments.

use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program};

use crate::Harness;

/// The driver: the protocol it picked from `initialize`.
#[derive(Default)]
pub struct Driver {
    protocol: OnceLock<u32>,
}

impl Driver {
    fn info() -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: vec![wire::Level::Observe],
            minimum: Some(wire::Minimum {
                release: "1.0.77".into(),
                update: Some("grok update".into()),
            }),
            sign_in: Some("run `grok login`".into()),
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
            hooks: Vec::new(),
        }
    }
}

impl Program for Driver {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let _ = self.protocol.set(protocol);
        Ok(wire::Initialized {
            protocol,
            program: Harness::Grok.program()?,
            harness: Some(Box::new(Self::info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, _method: &str, _params: Value) -> Result<Value, Error> {
        Err(Error::Unknown)
    }
}
