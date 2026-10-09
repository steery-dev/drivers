//! Steery's official `codex` driver, at observe, drive and live. It reads Codex's
//! conversations from their rollouts, plans the entries that set Codex up for Steery
//! (its MCP entry, the skill, the hooks), reads Codex's hook documents and prints what
//! they answer, and reaches a live thread through Codex's own doors: the app's router for
//! a thread a window owns, a running app server's queue for one it has loaded. A turn is
//! this binary in its turn mode (`steery-driver-codex turn …`) speaking to `codex
//! app-server`.

mod appserver;
mod calls;
mod drive;
mod events;
mod home;
mod hooks;
mod native;
mod rollout;
mod router;
mod setup;
mod turn;
mod words;

use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Notifier, PROTOCOL, Program, method, refusal, result};

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("turn") {
        let asked = match turn::Asked::parse(args) {
            Ok(asked) => asked,
            Err(why) => {
                eprintln!("{why}");
                std::process::exit(2);
            }
        };
        if let Err(why) = turn::run(asked).await {
            eprintln!("{why}");
            std::process::exit(1);
        }
        return Ok(());
    }
    steery_sdk::serve(Codex::new).await
}

/// How an agent in Codex learns its conversation's id, as Steery's skill says it.
const KNOWING: &str = "Your own environment holds it: `CODEX_SESSION_ID`, already a UUID. It names the\n\
conversation your person is in, and a sub-agent Codex started for you carries that same\n\
value — which is the one to send, because the conversation they are looking at is the one\n\
that spawned you. `CODEX_THREAD_ID` is a different thing, this agent's own thread, and is\n\
sent as `sub_agent` when you are a sub-agent; keep `CODEX_SESSION_ID` as `external_id`.\n\
\n\
Read it out of the environment at the moment you send it, never out of your own context. A\n\
conversation Codex resumed or forked carries the one it came from in what you were handed,\n\
and that identifier names a conversation your person is no longer reading.";

/// How Codex's transcript file reads.
const TRANSCRIPT: &str = "One JSON record per line: `session_meta` opens it, `response_item` records carry \
the messages, tool calls and their output in `payload`, and `event_msg` records repeat them as the \
terminal drew them.";

/// The oldest Codex this driver speaks to, and the command that updates it: 0.155.1 is the
/// first whose app server lists a running server's loaded threads and queue, and whose
/// hooks Steery read; 0.162.0 is the release this driver was checked against.
const MINIMUM: &str = "0.155.1";
const UPDATE: &str = "codex update";

/// The driver: Codex's home and conversations, from `initialize`.
struct Codex {
    notifier: Notifier,
    place: OnceLock<Result<drive::Drive, String>>,
    home: OnceLock<PathBuf>,
}

impl Codex {
    fn new(notifier: Notifier) -> Self {
        Self {
            notifier,
            place: OnceLock::new(),
            home: OnceLock::new(),
        }
    }

    fn drive(&self) -> Result<&drive::Drive, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` came first"))?
            .as_ref()
            .map_err(|why| Error::refused(refusal::REFUSED, why.clone()))
    }

    fn home(&self) -> Result<&PathBuf, Error> {
        self.home
            .get()
            .ok_or_else(|| Error::invalid("`initialize` came first"))
    }
}

impl Program for Codex {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this driver speaks protocol {PROTOCOL} only"
            )));
        }
        let _ = self.home.set(params.home.clone());
        let _ = self
            .place
            .set(home::root(&params.home).map(|root| drive::Drive {
                home: params.home.clone(),
                followers: router::Followers::new(
                    Some(root.join("ipc").join("ipc.sock")),
                    self.notifier.clone(),
                ),
                store: rollout::Store::new(root),
            }));
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "codex".into(),
                kind: wire::Kind::Harness,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "Codex".into(),
                short: Some("Cx".into()),
            },
            harness: Some(Box::new(info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::SETUP_PLAN => {
                result(setup::plan(self.home()?, steery_sdk::params(params)?).await?)
            }
            method::SESSIONS_LIST => result(self.drive()?.store.list(steery_sdk::params(params)?)),
            method::SESSIONS_LOCATE => {
                result(self.drive()?.store.locate(steery_sdk::params(params)?)?)
            }
            method::SESSIONS_READ => result(self.drive()?.store.read(steery_sdk::params(params)?)?),
            method::HOOK_READ => result(hooks::read(steery_sdk::params(params)?)?),
            method::HOOK_ANSWER => result(hooks::answer(steery_sdk::params(params)?)?),
            method::HELD => result(self.drive()?.held(steery_sdk::params(params)?).await?),
            method::IDLE => result(self.drive()?.idle(steery_sdk::params(params)?).await?),
            method::MODELS => {
                let _: wire::Empty = steery_sdk::params(params)?;
                result(self.drive()?.models().await?)
            }
            method::START => result(self.drive()?.start(steery_sdk::params(params)?)?),
            method::TURN_READ => result(drive::turn_line(steery_sdk::params(params)?)),
            method::STOP => result(self.drive()?.stop(steery_sdk::params(params)?).await?),
            method::RECEIPT_MARK => result(self.drive()?.mark(steery_sdk::params(params)?).await?),
            method::RECEIPT_FIND => result(self.drive()?.find(steery_sdk::params(params)?).await?),
            method::READER => result(drive::reader(steery_sdk::params(params)?).await?),
            method::READER_READ => result(drive::reader_read(steery_sdk::params(params)?)?),
            method::TERMINAL => result(self.drive()?.terminal(steery_sdk::params(params)?)?),
            method::SAY => result(self.drive()?.say(steery_sdk::params(params)?).await?),
            method::PICK => result(self.drive()?.pick(steery_sdk::params(params)?).await?),
            // Codex has no way to ask a thread's own model for a recap without running a
            // turn in it.
            _ => Err(Error::Unknown),
        }
    }
}

/// Codex as Steery reads it.
fn info() -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: wire::Level::ALL.to_vec(),
        minimum: Some(wire::Minimum {
            release: MINIMUM.into(),
            update: Some(UPDATE.into()),
        }),
        sign_in: Some("run `codex login`".into()),
        knowing: KNOWING.into(),
        transcript: Some(TRANSCRIPT.into()),
        identity: wire::Identity {
            executables: vec!["codex".into()],
            package: Some("@openai/codex".into()),
            session_variable: Some("CODEX_SESSION_ID".into()),
        },
        setup: wire::SetupFacts {
            reads_skills: true,
            cuts_tool_text: false,
            call_bound: None,
        },
        // Every tool, MCP server, rule file, extension and history write is closed for one
        // run; told no model, it runs the person's own.
        reader: Some(wire::ReaderFacts {
            sealed: wire::Sealed {
                tools: true,
                mcp: true,
                instructions: true,
                extensions: true,
                history: true,
            },
            known: Vec::new(),
            unnamed: true,
        }),
        facts: wire::HarnessFacts {
            by_file: false,
            // `thread/resume` takes no folder: the thread runs where it ran.
            resumes_in_place: false,
            forks_beside: true,
            // A thread holds the model and effort a turn set, from that turn on.
            keeps: wire::Keeps {
                model: true,
                thinking: true,
            },
            door: Some(harness::LiveDoor::Picks),
            checks_idle: false,
            worktrees: true,
            acts: vec![
                harness::Act::Continue,
                harness::Act::Fork,
                harness::Act::New,
            ],
        },
        hooks: hooks::declared(),
    }
}
