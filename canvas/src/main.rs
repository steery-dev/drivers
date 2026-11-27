//! Steery's `canvas` driver, at drive. The canvas harness keeps each session as a JSONL
//! transcript it appends to, `<CANVAS_HOME>/sessions/<id>.jsonl` ([`home`]). The driver lists
//! and reads those ([`sessions`], [`transcript`]), asks `canvas models` for its models and its
//! sign-in to Claude ([`models`], [`setup`], [`sign_in`]), and starts, reads and stops its
//! turns through `canvas turn` ([`drive`], [`turn`]). A turn dials Steery's plugin door
//! itself, from the door that setup writes to `steery.json` ([`setup`]); the driver folds
//! the records it sends there as it reads the transcript, and answers no live method.

mod drive;
mod home;
mod models;
mod sessions;
mod setup;
mod sign_in;
mod transcript;
mod turn;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

/// The canvas's mark: a pencil with a band near its end, one SVG path's `d` in a 24 by 24
/// box, filled with the even-odd rule.
const MARK: &str = "M3 21l1.5-5.5 11-11 4 4-11 11L3 21Zm11-14 .7-.7 3 3-.7.7-3-3Z";

/// How an agent in the canvas learns its session's id, as Steery's skill says it.
const KNOWING: &str = "The canvas names your session by a UUIDv7: its transcript is \
`<CANVAS_HOME>/sessions/<id>.jsonl`, and `canvas turn` prints `{\"named\":\"<id>\"}` as its \
first line. Send that id as `external_id`, with the harness `canvas`.";

/// How the canvas keeps a session, as Steery's skill says it.
const TRANSCRIPT: &str = "JSONL, append-only, one record per line with `id` (UUIDv7), `at` \
(RFC 3339) and `kind`: `session` (`cwd`, `title`) first, then per turn `turn_began` (`turn`), \
`words` (the person's message), `line` (`message`, `voice` sketcher or plotter, one stroke \
line), `tokens` (`voice`, `model`, `in`, `cached`, `out`) and `turn_ended` (`state`, \
`failure`).";

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
    match runtime.block_on(steery_sdk::serve(Canvas::new)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// The driver: the protocol it picked and the person's home, both from `initialize`, and
/// the sign-in that waits for its person.
struct Canvas {
    protocol: OnceLock<u32>,
    home: OnceLock<PathBuf>,
    sign_ins: sign_in::SignIns,
}

impl Canvas {
    fn new(_notifier: steery_sdk::Notifier) -> Self {
        Self {
            protocol: OnceLock::new(),
            home: OnceLock::new(),
            sign_ins: sign_in::SignIns::default(),
        }
    }

    fn home(&self) -> Result<&PathBuf, Error> {
        self.home
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }

    fn protocol(&self) -> Result<u32, Error> {
        self.protocol
            .get()
            .copied()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }

    /// The canvas's folder.
    fn root(&self) -> Result<PathBuf, Error> {
        Ok(home::root(self.home()?))
    }
}

impl Program for Canvas {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol_call_error(
            &params.protocol,
            steery_driver_common::program::protocol_signed_in_at,
        )
        .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let protocol = *self.protocol.get_or_init(|| protocol);
        let _ = self.home.set(params.home.clone());
        Ok(wire::Initialized {
            protocol,
            program: wire::ProgramInfo {
                id: "canvas".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Harness,
                release: env!("STEERY_PROGRAM_RELEASE").into(),
                label: "Canvas".into(),
                short: "Cv".into(),
                tint: Some("#e8590c".into()),
                mark: Some(MARK.into()),
            },
            harness: Some(Box::new(info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::SETUP_PLAN => result(
                setup::plan(
                    self.home()?,
                    &self.root()?,
                    steery_sdk::params(params)?,
                    self.protocol()?,
                )
                .await,
            ),
            method::NEEDS_GIVE if self.protocol()? >= steery_driver_common::program::NEEDS => {
                result(
                    self.sign_ins
                        .give(
                            self.home()?,
                            steery_sdk::params(params)?,
                            self.protocol()? >= steery_sdk::ASK,
                        )
                        .await?,
                )
            }
            method::SESSIONS_LIST => {
                result(sessions::list(&self.root()?, steery_sdk::params(params)?))
            }
            method::SESSIONS_LOCATE => {
                result(sessions::locate(&self.root()?, steery_sdk::params(params)?)?)
            }
            method::SESSIONS_READ => {
                result(sessions::read(&self.root()?, steery_sdk::params(params)?)?)
            }
            method::SESSIONS_FOLD => {
                result(sessions::fold(&self.root()?, steery_sdk::params(params)?)?)
            }
            method::MCP_LIST => Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "the canvas has no MCP client",
            )),
            method::MODELS => result(models::models(self.home()?).await?),
            method::START => result(drive::start(
                self.home()?,
                &self.root()?,
                steery_sdk::params(params)?,
            )?),
            method::TURN_READ => result(drive::read(&steery_sdk::params(params)?)),
            method::HELD => result(drive::held(&self.root()?, steery_sdk::params(params)?)?),
            method::IDLE => result(drive::idle(&self.root()?, steery_sdk::params(params)?)?),
            method::STOP => result(drive::stop(&steery_sdk::params(params)?).await?),
            method::RECEIPT_MARK => {
                result(sessions::mark(&self.root()?, steery_sdk::params(params)?)?)
            }
            method::RECEIPT_FIND => {
                result(sessions::find(&self.root()?, steery_sdk::params(params)?)?)
            }
            method::TERMINAL => result(drive::terminal(
                self.home()?,
                &self.root()?,
                steery_sdk::params(params)?,
            )?),
            _ => Err(Error::Unknown),
        }
    }
}

/// The canvas as Steery reads it.
fn info() -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: vec![wire::Level::Observe, wire::Level::Drive, wire::Level::Live],
        minimum: None,
        // The plan's `sign-in` need asks it; a person at the machine signs Claude Code in.
        sign_in: Some("run `claude auth login` in a terminal".into()),
        knowing: KNOWING.into(),
        lines: wire::Lines::Agent,
        hosted: Default::default(),
        transcript: Some(TRANSCRIPT.into()),
        // `canvas` is a shell launcher that `exec`s `bun <dir>/bin/canvas.ts`, which the
        // runtime matches by the script's name (PROTOCOL.md §3.3).
        identity: wire::Identity {
            executables: vec!["canvas".into(), "canvas.ts".into()],
            package: None,
            session_variable: None,
            call_meta: None,
        },
        setup: wire::SetupFacts {
            skill: wire::SkillAt::Handshake,
            cuts_tool_text: false,
            call_bound: None,
        },
        reader: None,
        // The canvas picks each model by its role, so a continue keeps them.
        facts: wire::HarnessFacts {
            touches: false,
            by_file: false,
            resumes_in_place: false,
            forks_beside: false,
            keeps: wire::Keeps {
                model: true,
                thinking: true,
            },
            // `canvas turn` dials Steery's plugin door, from `steery.json`.
            door: Some(harness::LiveDoor::Plugin),
            checks_idle: false,
            worktrees: false,
            acts: vec![harness::Act::Continue, harness::Act::New],
        },
        hooks: Vec::new(),
        bare: false,
    }
}
