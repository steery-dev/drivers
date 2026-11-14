//! Steery's `warp` driver, at observe. Warp keeps its agent's conversations in each
//! channel's `warp.sqlite` (see [`home`]): a row of each in `agent_conversations`, its
//! tasks Protocol Buffers records in `agent_tasks`. The driver lists and reads those
//! conversations, plans the entries that set Warp up for Steery (its MCP entry, the
//! skill), and lists the MCP servers Warp starts. It starts no turn and reaches no live
//! session.

mod events;
mod home;
mod mcp;
mod pb;
mod setup;
mod store;
mod task;

use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

/// Warp's mark: a letter W, one SVG path's `d` in a 24 by 24 box, filled with the even-odd
/// rule.
const MARK: &str = "M2 4h3.2l2.3 10.2L10.4 4h3.2l2.9 10.2L18.8 4H22l-4 16h-3.1L12 9.8 9.1 20H6Z";

#[tokio::main]
async fn main() -> std::io::Result<()> {
    steery_sdk::serve(Warp::new).await
}

/// How an agent in Warp learns its conversation's id, as Steery's skill says it. Warp puts
/// no conversation id in a shell command's environment nor in an MCP call.
const KNOWING: &str = "Warp names your conversation nowhere you can read it directly. Run \
`sqlite3 -readonly \"$HOME/Library/Group Containers/2BBY89MBSN.dev.warp/Library/Application \
Support/dev.warp.Warp-Stable/warp.sqlite\" \"select conversation_id from agent_conversations \
order by last_modified_at desc limit 1\"` (on Linux the store is \
`~/.local/state/warp-terminal/warp.sqlite`) and send it as `external_id`, with the harness \
`warp`. That is the conversation Warp wrote to last: where another of Warp's agents runs too, \
check that its `summary` names your first words.";

/// How Warp keeps a conversation, as Steery's skill says it.
const TRANSCRIPT: &str = "One SQLite file per channel, not a transcript: `warp.sqlite` in \
Warp's state folder (macOS: `~/Library/Group Containers/2BBY89MBSN.dev.warp/Library/Application \
Support/dev.warp.Warp-Stable`; Linux: `~/.local/state/warp-terminal`), and `tui/warp.sqlite` \
beside it for the terminal front end. Table `agent_conversations` has a row of each conversation, \
`summary` a JSON of its title and first folder and `conversation_data` a JSON naming the \
conversation that started it; table `agent_tasks` has its tasks, each `task` a Protocol \
Buffers `warp.multi_agent.v1.Task` whose messages are the person's queries, the agent's words \
and reasoning, tool calls, their results and each request's `request_metadata`.";

/// The oldest Warp this driver speaks to, and the command that updates it: the release it
/// was checked against, the first whose requests record how they ended.
const MINIMUM: &str = "2026.11.11";
const UPDATE: &str = "brew upgrade --cask warp";

/// The driver: the protocol it picked and the person's home, both from `initialize`.
struct Warp {
    protocol: OnceLock<u32>,
    home: OnceLock<PathBuf>,
}

impl Warp {
    fn new(_: steery_sdk::Notifier) -> Self {
        Self {
            protocol: OnceLock::new(),
            home: OnceLock::new(),
        }
    }

    fn home(&self) -> Result<&PathBuf, Error> {
        self.home
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }
}

impl Program for Warp {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let protocol = *self.protocol.get_or_init(|| protocol);
        let _ = self.home.set(params.home.clone());
        Ok(wire::Initialized {
            protocol,
            program: wire::ProgramInfo {
                id: "warp".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Harness,
                release: env!("STEERY_PROGRAM_RELEASE").into(),
                label: "Warp".into(),
                short: "Wa".into(),
                tint: Some("#2e5d9e".into()),
                mark: Some(MARK.into()),
            },
            harness: Some(Box::new(info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::SETUP_PLAN => {
                result(setup::plan(self.home()?, steery_sdk::params(params)?).await)
            }
            method::SESSIONS_LIST => {
                let asked: harness::List = steery_sdk::params(params)?;
                result(store::Store::new(self.home()?).list(asked)?)
            }
            method::SESSIONS_LOCATE => {
                result(store::Store::new(self.home()?).locate(steery_sdk::params(params)?)?)
            }
            method::SESSIONS_READ => {
                result(store::Store::new(self.home()?).read(steery_sdk::params(params)?)?)
            }
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Warp has no plugin door that sends its records",
                ))
            }
            method::MCP_LIST => result(mcp::list(self.home()?, steery_sdk::params(params)?)?),
            // `hook/read` and `hook/answer` (Warp has no hooks), and every method of drive
            // and live, which the levels rule out.
            _ => Err(Error::Unknown),
        }
    }
}

/// Warp as Steery reads it.
fn info() -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: vec![wire::Level::Observe],
        minimum: Some(wire::Minimum {
            release: MINIMUM.into(),
            update: Some(UPDATE.into()),
        }),
        // Warp's agent calls models through Warp's servers, under a Warp account.
        sign_in: Some("open Warp and sign in to a Warp account".into()),
        knowing: KNOWING.into(),
        lines: wire::Lines::Agent,
        hosted: Default::default(),
        transcript: Some(TRANSCRIPT.into()),
        identity: wire::Identity {
            executables: vec!["oz".into(), "oz-preview".into(), "warp".into()],
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
        // Observe only: the driver continues, forks and starts nothing, and has no door.
        facts: wire::HarnessFacts {
            touches: false,
            by_file: false,
            resumes_in_place: false,
            forks_beside: false,
            keeps: wire::Keeps {
                model: false,
                thinking: false,
            },
            door: None,
            checks_idle: false,
            worktrees: false,
            acts: Vec::new(),
        },
        hooks: Vec::new(),
    }
}
