//! Steery's `aider` driver, at drive. Aider keeps each conversation as a Markdown chat
//! history file that it appends to, `.aider.chat.history.md` in a repository's root
//! ([`history`]). The driver finds those files below the person's home ([`home`]), lists
//! and reads them ([`store`]), lists Ollama's models, and starts, reads and stops Aider's
//! turns through `aider --message-file` ([`drive`], [`turn`]). Aider has no hooks, MCP
//! client, ACP server or door, so Steery reaches no live Aider window.

mod drive;
mod history;
mod home;
mod setup;
mod store;
mod turn;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

/// Aider's mark: a letter A, one SVG path's `d` in a 24 by 24 box, filled with the even-odd
/// rule.
const MARK: &str =
    "M10.2 3h3.6l6.7 18h-3.9l-1.5-4.3H8.9L7.4 21H3.5L10.2 3Zm-.2 10.6h4l-2-5.8-2 5.8Z";

/// How an agent in Aider learns its conversation's id, as Steery's skill says it. Aider
/// names no id anywhere: the conversation is its history file.
const KNOWING: &str = "Aider names your conversation nowhere. It is the chat history file Aider \
appends to: `.aider.chat.history.md` in the root of your git repository, unless Aider was \
started with `--chat-history-file <path>`. Send as `external_id`, with the harness `aider`, the \
first 32 hex digits of the SHA-256 of that file's absolute path: \
`printf %s \"$PWD/.aider.chat.history.md\" | shasum -a 256 | cut -c1-32` in the repository root.";

/// How Aider keeps a conversation, as Steery's skill says it.
const TRANSCRIPT: &str = "Markdown, appended per run: `# aider chat started at <local time>` \
opens a run, `#### ` lines are the person's words, `> ` lines are Aider's own output (its \
command line, model, edits, commits, errors and `Tokens:` counts), and every other line is the \
model's answer, its reasoning between `<thinking-content-…>` tags. No ids and no other times.";

/// The oldest Aider this driver speaks to, and the command that updates it: the release it
/// was checked against.
const MINIMUM: &str = "0.86.2";
const UPDATE: &str = "uv tool upgrade aider-chat";

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
    match runtime.block_on(steery_sdk::serve(Aider::new)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// The driver: the protocol it picked and the person's home, both from `initialize`, and
/// the history files it knows.
struct Aider {
    protocol: OnceLock<u32>,
    home: OnceLock<PathBuf>,
    store: store::Store,
}

impl Aider {
    fn new(_notifier: steery_sdk::Notifier) -> Self {
        Self {
            protocol: OnceLock::new(),
            home: OnceLock::new(),
            store: store::Store::default(),
        }
    }

    fn home(&self) -> Result<&PathBuf, Error> {
        self.home
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }
}

impl Program for Aider {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let protocol = *self.protocol.get_or_init(|| protocol);
        let _ = self.home.set(params.home.clone());
        Ok(wire::Initialized {
            protocol,
            program: wire::ProgramInfo {
                id: "aider".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Harness,
                release: env!("STEERY_PROGRAM_RELEASE").into(),
                label: "Aider".into(),
                short: "Ai".into(),
                tint: Some("#14b014".into()),
                mark: Some(MARK.into()),
            },
            harness: Some(Box::new(info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let store = &self.store;
        match method {
            method::SETUP_PLAN => {
                result(setup::plan(self.home()?, steery_sdk::params(params)?).await)
            }
            method::SESSIONS_LIST => result(store.list(self.home()?, steery_sdk::params(params)?)),
            method::SESSIONS_LOCATE => {
                result(store.locate(self.home()?, steery_sdk::params(params)?)?)
            }
            method::SESSIONS_READ => result(store.read(self.home()?, steery_sdk::params(params)?)?),
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Aider has no plugin door that sends its records",
                ))
            }
            method::MCP_LIST => Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Aider has no MCP client",
            )),
            method::MODELS => result(drive::models(self.home()?).await?),
            method::START => result(drive::start(
                self.home()?,
                store,
                steery_sdk::params(params)?,
            )?),
            method::TURN_READ => result(drive::read(&steery_sdk::params(params)?)),
            method::HELD => result(drive::held(
                self.home()?,
                store,
                steery_sdk::params(params)?,
            )?),
            method::IDLE => result(drive::idle(
                self.home()?,
                store,
                steery_sdk::params(params)?,
            )?),
            method::STOP => result(drive::stop(&steery_sdk::params(params)?)?),
            method::RECEIPT_MARK => result(drive::mark(
                self.home()?,
                store,
                steery_sdk::params(params)?,
            )?),
            method::RECEIPT_FIND => result(drive::find(
                self.home()?,
                store,
                steery_sdk::params(params)?,
            )?),
            method::TERMINAL => result(drive::terminal(
                self.home()?,
                store,
                steery_sdk::params(params)?,
            )?),
            _ => Err(Error::Unknown),
        }
    }
}

/// Aider as Steery reads it.
fn info() -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: vec![wire::Level::Observe, wire::Level::Drive],
        minimum: Some(wire::Minimum {
            release: MINIMUM.into(),
            update: Some(UPDATE.into()),
        }),
        // Aider calls a model with the key of its provider, from its environment or a
        // `.env`; Ollama's models need none.
        sign_in: None,
        knowing: KNOWING.into(),
        lines: wire::Lines::Agent,
        hosted: Default::default(),
        transcript: Some(TRANSCRIPT.into()),
        // Aider runs as `python …/bin/aider`, which the runtime matches by the script's
        // name (PROTOCOL.md §3.3).
        identity: wire::Identity {
            executables: vec!["aider".into()],
            package: Some("aider-chat".into()),
            session_variable: None,
            call_meta: None,
        },
        setup: wire::SetupFacts {
            reads_skills: false,
            cuts_tool_text: false,
            call_bound: None,
        },
        reader: None,
        // A continue is `--restore-chat-history` with the conversation's file, in its
        // folder; it restores the messages, not the model or its reasoning effort.
        facts: wire::HarnessFacts {
            touches: false,
            by_file: true,
            resumes_in_place: true,
            forks_beside: false,
            keeps: wire::Keeps {
                model: false,
                thinking: false,
            },
            door: None,
            checks_idle: false,
            worktrees: false,
            acts: vec![harness::Act::Continue, harness::Act::New],
        },
        hooks: Vec::new(),
    }
}
