//! Steery's `codex` driver, at observe, drive and live. It reads Codex's
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
mod lines;
mod mcp;
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

/// Codex's mark, OpenAI's: one SVG path's `d` in a 24 by 24 box, filled with the even-odd rule.
const MARK: &str = "M9.205 8.658v-2.26c0-.19.072-.333.238-.428l4.543-2.616c.619-.357 1.356-.523 2.117-.523 2.854 0 4.662 2.212 4.662 4.566 0 .167 0 .357-.024.547l-4.71-2.759a.797.797 0 00-.856 0l-5.97 3.473zm10.609 8.8V12.06c0-.333-.143-.57-.429-.737l-5.97-3.473 1.95-1.118a.433.433 0 01.476 0l4.543 2.617c1.309.76 2.189 2.378 2.189 3.948 0 1.808-1.07 3.473-2.76 4.163zM7.802 12.703l-1.95-1.142c-.167-.095-.239-.238-.239-.428V5.899c0-2.545 1.95-4.472 4.591-4.472 1 0 1.927.333 2.712.928L8.23 5.067c-.285.166-.428.404-.428.737v6.898zM12 15.128l-2.795-1.57v-3.33L12 8.658l2.795 1.57v3.33L12 15.128zm1.796 7.23c-1 0-1.927-.332-2.712-.927l4.686-2.712c.285-.166.428-.404.428-.737v-6.898l1.974 1.142c.167.095.238.238.238.428v5.233c0 2.545-1.974 4.472-4.614 4.472zm-5.637-5.303l-4.544-2.617c-1.308-.761-2.188-2.378-2.188-3.948A4.482 4.482 0 014.21 6.327v5.423c0 .333.143.571.428.738l5.947 3.449-1.95 1.118a.432.432 0 01-.476 0zm-.262 3.9c-2.688 0-4.662-2.021-4.662-4.519 0-.19.024-.38.047-.57l4.686 2.71c.286.167.571.167.856 0l5.97-3.448v2.26c0 .19-.07.333-.237.428l-4.543 2.616c-.619.357-1.356.523-2.117.523zm5.899 2.83a5.947 5.947 0 005.827-4.756C22.287 18.339 24 15.84 24 13.296c0-1.665-.713-3.282-1.998-4.448.119-.5.19-.999.19-1.498 0-3.401-2.759-5.947-5.946-5.947-.642 0-1.26.095-1.88.31A5.962 5.962 0 0010.205 0a5.947 5.947 0 00-5.827 4.757C1.713 5.447 0 7.945 0 10.49c0 1.666.713 3.283 1.998 4.448-.119.5-.19 1-.19 1.499 0 3.401 2.759 5.946 5.946 5.946.642 0 1.26-.095 1.88-.309a5.96 5.96 0 004.162 1.713z";

// Many threads: most calls read files without yielding, and on one thread a slow one stops
// standard input being read; the runtime then kills the driver with every call in flight.
#[tokio::main]
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
                id: "codex".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Harness,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "Codex".into(),
                short: "Cx".into(),
                tint: Some("#2f7d66".into()),
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
                result(setup::plan(self.home()?, steery_sdk::params(params)?).await?)
            }
            method::SESSIONS_LIST => result(self.drive()?.store.list(steery_sdk::params(params)?)),
            method::SESSIONS_LOCATE => {
                result(self.drive()?.store.locate(steery_sdk::params(params)?)?)
            }
            method::SESSIONS_READ => result(self.drive()?.store.read(steery_sdk::params(params)?)?),
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Codex has no plugin door to carry its records",
                ))
            }
            method::HOOK_READ => result(hooks::read(steery_sdk::params(params)?)?),
            method::HOOK_ANSWER => result(hooks::answer(steery_sdk::params(params)?)?),
            method::MCP_LIST => result(
                mcp::list(home::binary().as_deref(), steery_sdk::params(params)?).await?,
            ),
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
