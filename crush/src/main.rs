//! Steery's `crush` driver, at drive. Crush keeps each project's sessions in that project's
//! SQLite store, `<project>/.crush/crush.db`, and lists the projects in its data folder's
//! `projects.json`. The driver lists and reads those sessions, plans the entries that set
//! Crush up for Steery (its MCP entry, the skill, the hooks), reads and answers Crush's tool
//! hooks ([`hooks`]), lists the MCP servers Crush loads, lists Crush's models, and starts,
//! reads and stops Crush's turns through `crush run` ([`drive`], [`turn`]). It reaches no
//! live session.

mod calls;
mod drive;
mod events;
mod home;
mod hooks;
mod mcp;
mod setup;
mod store;
mod turn;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

/// Crush's mark: a letter C, one SVG path's `d` in a 24 by 24 box, filled with the even-odd
/// rule.
const MARK: &str = "M12 2a10 10 0 1 0 8.66 15h-4.04A6 6 0 1 1 16.62 7h4.04A10 10 0 0 0 12 2Z";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    if args.next().as_deref() == Some(turn::MODE) {
        return runtime.block_on(turn::run(args));
    }
    match runtime.block_on(steery_sdk::serve(Crush::new)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// How an agent in Crush learns its session's id, as Steery's skill says it. Crush puts no
/// session id in a shell command's environment (only `CRUSH=1`, `AGENT=crush`) nor in an MCP
/// call, so the agent asks Crush for the session it last wrote.
const KNOWING: &str = "Crush names your session nowhere you can read it directly. In your \
working folder, run `crush session last --json` and send its `meta.uuid` as `external_id`, with \
the harness `crush`. That is the session Crush wrote to last in that folder: where another Crush \
session runs there too, check that its last messages are yours.";

/// How Crush keeps a session, as Steery's skill says it.
const TRANSCRIPT: &str = "One SQLite file per project, not a transcript: `<project>/.crush/crush.db`, \
the projects listed in `projects.json` of Crush's data folder (`$CRUSH_GLOBAL_DATA`, else \
`<XDG_DATA_HOME or ~/.local/share>/crush`). Table `sessions` lists the sessions \
(`parent_session_id` names a sub-agent's lead); each message is a row of `messages`, in \
`created_at` order, with a `role` and its `parts` a JSON array of `{type, data}` (`text`, \
`reasoning`, `tool_call`, `tool_result`, `finish`, `image_url`, `binary`). An answer's `finish` \
part comes when the model stops.";

/// The oldest Crush this driver speaks to, and the command that updates it: the release it
/// was checked against.
const MINIMUM: &str = "0.100.0";
const UPDATE: &str = "brew upgrade charmbracelet/tap/crush";

/// The driver: the protocol it picked and the person's home, both from `initialize`.
struct Crush {
    protocol: OnceLock<u32>,
    home: OnceLock<PathBuf>,
}

impl Crush {
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

impl Program for Crush {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let protocol = *self.protocol.get_or_init(|| protocol);
        let _ = self.home.set(params.home.clone());
        Ok(wire::Initialized {
            protocol,
            program: wire::ProgramInfo {
                id: "crush".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Harness,
                release: env!("STEERY_PROGRAM_RELEASE").into(),
                label: "Crush".into(),
                short: "Cr".into(),
                tint: Some("#6b50ff".into()),
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
                let home = self.home()?;
                let read: harness::Read = steery_sdk::params(params)?;
                result(store::Store::new(home).read(read, |cwd| mcp::names(home, cwd))?)
            }
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Crush has no plugin door that sends its records",
                ))
            }
            method::MCP_LIST => result(mcp::list(self.home()?, steery_sdk::params(params)?)?),
            method::MODELS => result(drive::models(self.home()?).await?),
            method::START => result(drive::start(self.home()?, steery_sdk::params(params)?)?),
            method::TURN_READ => result(drive::read(&steery_sdk::params(params)?)),
            method::HELD => result(drive::held(self.home()?, steery_sdk::params(params)?)?),
            method::IDLE => result(drive::idle(self.home()?, steery_sdk::params(params)?)?),
            method::STOP => result(drive::stop(&steery_sdk::params(params)?)?),
            method::RECEIPT_MARK => result(drive::mark(self.home()?, steery_sdk::params(params)?)?),
            method::RECEIPT_FIND => result(drive::find(self.home()?, steery_sdk::params(params)?)?),
            method::TERMINAL => result(drive::terminal(self.home()?, steery_sdk::params(params)?)?),
            method::HOOK_READ => result(hooks::read(self.home()?, steery_sdk::params(params)?)?),
            method::HOOK_ANSWER => result(hooks::answer(steery_sdk::params(params)?)?),
            // Every method of live, which the levels rule out.
            _ => Err(Error::Unknown),
        }
    }
}

/// Crush as Steery reads it.
fn info() -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: vec![wire::Level::Observe, wire::Level::Drive],
        minimum: Some(wire::Minimum {
            release: MINIMUM.into(),
            update: Some(UPDATE.into()),
        }),
        // Crush calls a model with the person's own provider key or local model, chosen in
        // its first run.
        sign_in: Some("run `crush` in a terminal and pick a provider".into()),
        knowing: KNOWING.into(),
        lines: wire::Lines::Agent,
        hosted: Default::default(),
        transcript: Some(TRANSCRIPT.into()),
        identity: wire::Identity {
            executables: vec!["crush".into()],
            package: Some("@charmland/crush".into()),
            session_variable: None,
            call_meta: None,
        },
        setup: wire::SetupFacts {
            reads_skills: true,
            cuts_tool_text: false,
            call_bound: None,
        },
        reader: None,
        // A turn is `crush run`: a new session, or a continue in the project that keeps it,
        // which restores the model of the session's last answer (`restoreModelFromSession`)
        // but not its reasoning effort. No door.
        facts: wire::HarnessFacts {
            touches: false,
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
            acts: vec![harness::Act::Continue, harness::Act::New],
        },
        hooks: hooks::table(),
    }
}
