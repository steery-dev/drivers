//! Steery's `amp` driver, at observe. Amp keeps its threads on ampcode.com, not on this
//! host: the driver lists and reads them through Amp's own CLI (`amp threads list`, `amp
//! threads export`), plans the entries that set Amp up for Steery (its MCP entry, the
//! skill), asks its person to sign Amp in where it is not, and lists the MCP servers Amp
//! loads. It starts no turn and reaches no live thread.

mod calls;
mod events;
mod home;
mod mcp;
mod setup;
mod sign_in;
mod threads;

use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::Value;
use steery_driver_common::program::NEEDS;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

/// Amp's mark: a plain letter A, one SVG path's `d` in a 24 by 24 box, filled with the
/// even-odd rule.
const MARK: &str = "M12 2L22 22H17.5L15.75 18H8.25L6.5 22H2ZM9.9 14H14.1L12 9Z";

// Many threads: an export waits on Amp, and on one thread a slow one stops standard input
// being read.
#[tokio::main]
async fn main() -> std::io::Result<()> {
    steery_sdk::serve(Amp::new).await
}

/// How an agent in Amp learns its thread's id, as Steery's skill says it.
///
/// UNVERIFIED until a turn runs: Amp's binary reads `AMP_THREAD_ID` for its orb and CLI
/// subcommands, but a plugin process that Amp started had neither `AMP_THREAD_ID` nor
/// `AMP_CURRENT_THREAD_ID`. So the driver declares no `identity.session_variable`.
const KNOWING: &str = "Your thread's id is the `T-…` in your thread's address \
(`https://ampcode.com/threads/T-…`). Send the whole `T-…`, with its `T-`. Where your environment \
holds `AMP_THREAD_ID`, it holds the same id: read it there when it is set.";

/// The oldest Amp this driver speaks to, and the command that updates it: the release it
/// was checked against, whose `amp threads list` takes `--installation-id`.
const MINIMUM: &str = "0.0.1794225648";
const UPDATE: &str = "amp update";

/// The driver: the protocol it picked and the person's home, from `initialize`, Amp's
/// threads as it last listed them, and the sign-in it started that waits for its person.
struct Amp {
    protocol: OnceLock<u32>,
    home: OnceLock<PathBuf>,
    threads: threads::Threads,
    sign_ins: sign_in::SignIns,
}

impl Amp {
    fn new(_: steery_sdk::Notifier) -> Self {
        Self {
            protocol: OnceLock::new(),
            home: OnceLock::new(),
            threads: threads::Threads::default(),
            sign_ins: sign_in::SignIns::default(),
        }
    }

    /// Whether the protocol picked has needs.
    fn needs(&self) -> bool {
        self.protocol.get().is_some_and(|picked| *picked >= NEEDS)
    }

    fn home(&self) -> Result<&PathBuf, Error> {
        self.home
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }
}

impl Program for Amp {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let protocol = *self.protocol.get_or_init(|| protocol);
        let _ = self.home.set(params.home.clone());
        Ok(wire::Initialized {
            protocol,
            program: wire::ProgramInfo {
                id: "amp".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Harness,
                release: env!("STEERY_PROGRAM_RELEASE").into(),
                label: "Amp".into(),
                short: "Amp".into(),
                tint: Some("#f34e3f".into()),
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
                result(setup::plan(self.home()?, steery_sdk::params(params)?, self.needs()).await)
            }
            method::SESSIONS_LIST => {
                let asked: harness::List = steery_sdk::params(params)?;
                result(self.threads.list(self.home()?, asked).await)
            }
            method::SESSIONS_LOCATE => {
                result(self.threads.locate(steery_sdk::params(params)?).await?)
            }
            method::SESSIONS_READ => result(events::read(steery_sdk::params(params)?).await?),
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Amp has no plugin door that sends its records",
                ))
            }
            method::MCP_LIST => result(mcp::list(self.home()?, steery_sdk::params(params)?).await?),
            method::NEEDS_GIVE if self.needs() => {
                result(self.sign_ins.give(steery_sdk::params(params)?).await?)
            }
            // `hook/read` and `hook/answer` (no hook is declared), and every method of drive
            // and live, which the levels rule out.
            _ => Err(Error::Unknown),
        }
    }
}

/// Amp as Steery reads it.
fn info() -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: vec![wire::Level::Observe],
        minimum: Some(wire::Minimum {
            release: MINIMUM.into(),
            update: Some(UPDATE.into()),
        }),
        sign_in: Some("run `amp login` in a terminal".into()),
        knowing: KNOWING.into(),
        lines: wire::Lines::Agent,
        hosted: Default::default(),
        // Amp keeps its threads on ampcode.com: this host holds no transcript file.
        transcript: None,
        identity: wire::Identity {
            executables: vec!["amp".into()],
            package: Some("@sourcegraph/amp".into()),
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
