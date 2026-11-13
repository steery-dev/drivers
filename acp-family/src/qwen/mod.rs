//! Steery's driver for Qwen Code at observe, drive and live: it reads Qwen's conversations
//! from `<base>/projects/<project>/chats/`, plans the entries that set Qwen up for Steery,
//! reads its hooks' moments, lists the MCP servers and models it loads, plans the turns that
//! the runtime runs as `qwen` itself ([`drive`]), and reaches a conversation that Qwen's
//! daemon runs through that daemon ([`live`]).

mod door;
mod drive;
mod events;
mod hooks;
mod live;
mod mcp;
mod models;
mod setup;
mod store;
mod turn;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

use crate::Harness;

/// The oldest release whose chat files, hooks and settings this driver reads.
pub(crate) const MINIMUM: &str = "0.27.2";

/// npm's package of Qwen Code.
pub(crate) const PACKAGE: &str = "@qwen-code/qwen-code";

/// The driver: what `initialize` said, the listings it answered, the turns it read, and the
/// conversations it follows in a daemon.
pub struct Driver {
    place: OnceLock<Place>,
    changes: steery_sdk::listing::Changes,
    turns: turn::Turns,
    live: Arc<live::Live>,
}

impl Driver {
    pub fn new(notifier: steery_sdk::Notifier) -> Self {
        Self {
            place: OnceLock::new(),
            changes: Default::default(),
            turns: Default::default(),
            live: Arc::new(live::Live::new(notifier)),
        }
    }
}

/// The person's home, Qwen's root, and what was read of its chat files.
pub(crate) struct Place {
    pub home: PathBuf,
    pub root: PathBuf,
    store: store::Store,
}

/// Qwen's root: `$QWEN_HOME`, else `.qwen` in the person's home (`Storage.getGlobalQwenDir`).
pub(crate) fn root(home: &Path) -> PathBuf {
    std::env::var_os("QWEN_HOME")
        .filter(|root| !root.is_empty())
        .map_or_else(|| home.join(".qwen"), PathBuf::from)
}

/// Where Qwen writes its conversations (`Storage.getRuntimeBaseDir`): `$QWEN_RUNTIME_DIR`,
/// else the person's setting `advanced.runtimeOutputDir`, else the root. A `~` opens the
/// person's home; a relative folder, which Qwen reads from where it runs, is not followed.
pub(crate) fn base(home: &Path, root: &Path) -> PathBuf {
    let set = std::env::var("QWEN_RUNTIME_DIR")
        .ok()
        .filter(|dir| !dir.is_empty())
        .or_else(|| {
            crate::jsonc::jsonc(&root.join("settings.json"))?["advanced"]["runtimeOutputDir"]
                .as_str()
                .filter(|dir| !dir.is_empty())
                .map(str::to_owned)
        });
    let resolved = set.and_then(|dir| match dir.strip_prefix('~') {
        Some(rest) => Some(home.join(rest.trim_start_matches('/'))),
        None => Some(PathBuf::from(dir)).filter(|dir| dir.is_absolute()),
    });
    resolved.unwrap_or_else(|| root.to_path_buf())
}

/// The `qwen` that Steery runs: the first on `PATH`.
pub(crate) fn binary() -> Option<PathBuf> {
    steery_driver_common::program::on_path("qwen")
}

impl Driver {
    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` must come first"))
    }

    fn store(&self) -> Result<&store::Store, Error> {
        Ok(&self.place()?.store)
    }

    fn info() -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: vec![wire::Level::Observe, wire::Level::Drive, wire::Level::Live],
            minimum: Some(wire::Minimum {
                release: MINIMUM.into(),
                update: Some(format!("npm install -g {PACKAGE}@latest")),
            }),
            sign_in: Some("run `qwen` in a terminal and choose how to sign in".into()),
            knowing: "Qwen Code sets `QWEN_CODE_SESSION_ID` to the conversation's id in the\n\
                      environment of each shell command and each process it starts, an MCP\n\
                      server among them. Read it there: `harness` is `qwen`, `external_id` is\n\
                      its value. Where it is not set, describe nothing."
                .into(),
            lines: wire::Lines::Agent,
            hosted: Default::default(),
            transcript: Some(
                "One JSON record on each line of `<id>.jsonl`: each record names its parent \
                 by `uuid` and `parentUuid`, and the conversation is the path from the last \
                 record back to a root: prompts (`user`), answers (`assistant`), tools' \
                 results (`tool_result`) and Qwen's own records (`system`)."
                    .into(),
            ),
            identity: wire::Identity {
                executables: vec!["qwen".into()],
                package: Some(PACKAGE.into()),
                session_variable: Some("QWEN_CODE_SESSION_ID".into()),
                call_meta: None,
            },
            setup: wire::SetupFacts {
                reads_skills: true,
                cuts_tool_text: false,
                call_bound: None,
            },
            // `--core-tools` allows only the tools named, `--exclude-tools` holds back the
            // rest, `--allowed-mcp-server-names ""` starts no MCP server and
            // `--chat-recording=false` saves nothing. Qwen still reads `QWEN.md` and its
            // extensions.
            reader: Some(wire::ReaderFacts {
                sealed: wire::Sealed {
                    tools: true,
                    mcp: true,
                    instructions: false,
                    extensions: false,
                    history: true,
                },
                known: Vec::new(),
                unnamed: true,
            }),
            facts: wire::HarnessFacts {
                by_file: false,
                resumes_in_place: true,
                forks_beside: false,
                keeps: wire::Keeps {
                    model: true,
                    thinking: false,
                },
                door: Some(harness::LiveDoor::Picks),
                checks_idle: false,
                worktrees: false,
                acts: vec![
                    harness::Act::Continue,
                    harness::Act::Fork,
                    harness::Act::New,
                ],
                touches: false,
            },
            hooks: hooks::table(),
        }
    }
}

impl Program for Driver {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_driver_common::program::protocol(&params.protocol)
            .ok_or_else(|| Error::invalid(steery_driver_common::program::unspoken()))?;
        let root = root(&params.home);
        let _ = self.place.set(Place {
            home: params.home.clone(),
            store: store::Store::new(&base(&params.home, &root)),
            root,
        });
        self.changes.picked(protocol);
        Ok(wire::Initialized {
            protocol,
            program: Harness::Qwen.program()?,
            harness: Some(Box::new(Self::info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::SETUP_PLAN => {
                result(setup::plan(&self.place()?.root, steery_sdk::params(params)?).await)
            }
            method::SESSIONS_LIST => {
                let asked: harness::List = steery_sdk::params(params)?;
                let after = asked.after.clone();
                result(
                    self.changes
                        .answer(after.as_deref(), self.store()?.list(asked)),
                )
            }
            method::SESSIONS_LOCATE => result(self.store()?.locate(steery_sdk::params(params)?)?),
            method::SESSIONS_READ => result(self.store()?.read(steery_sdk::params(params)?)?),
            method::SESSIONS_FOLD => {
                let _: harness::Fold = steery_sdk::params(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Qwen Code has no plugin door that sends its records",
                ))
            }
            method::HOOK_READ => result(hooks::read(steery_sdk::params(params)?)?),
            method::HOOK_ANSWER => result(hooks::answer(steery_sdk::params(params)?)?),
            method::MCP_LIST => {
                let place = self.place()?;
                result(mcp::list(
                    &place.home,
                    &place.root,
                    steery_sdk::params(params)?,
                )?)
            }
            method::MODELS => {
                let _: wire::Empty = steery_sdk::params(params)?;
                result(models::list(&self.place()?.root).await?)
            }
            method::HELD => {
                let held: harness::Held = steery_sdk::params(params)?;
                let place = self.place()?;
                match self
                    .live
                    .held(&place.home, &place.root, &held.external_id)
                    .await
                {
                    Some(holder) => result(harness::HeldBy::Holder(holder)),
                    None => result(drive::held(&place.store, &held)),
                }
            }
            method::SAY => {
                let place = self.place()?;
                let say = steery_sdk::params(params)?;
                result(self.live.say(&place.home, &place.root, say).await?)
            }
            method::PICK => {
                let place = self.place()?;
                let pick = steery_sdk::params(params)?;
                result(self.live.pick(&place.home, &place.root, pick).await?)
            }
            method::IDLE => result(drive::idle(self.store()?, steery_sdk::params(params)?)),
            method::START => result(
                drive::start(
                    &self.place()?.root,
                    self.store()?,
                    steery_sdk::params(params)?,
                )
                .await?,
            ),
            method::TURN_READ => result(self.turns.read(steery_sdk::params(params)?)),
            method::STOP => {
                let stop: harness::Stop = steery_sdk::params(params)?;
                match stop.holder {
                    harness::Holder::Person {
                        stop: harness::HolderStop::Owner(_),
                        ..
                    } => {
                        let place = self.place()?;
                        result(self.live.stop(&place.home, &place.root, &stop).await?)
                    }
                    _ => result(crate::turn::drive::stop("Qwen Code", &stop)?),
                }
            }
            method::RECEIPT_MARK => result(drive::mark(steery_sdk::params(params)?)?),
            method::RECEIPT_FIND => result(drive::find(steery_sdk::params(params)?)?),
            method::READER => result(drive::reader(steery_sdk::params(params)?)?),
            method::TERMINAL => {
                let place = self.place()?;
                let terminal: harness::Terminal = steery_sdk::params(params)?;
                let id = terminal.external_id.clone();
                let mut command =
                    drive::terminal(&place.home, &place.root, &place.store, terminal)?;
                if let Some(attach) = self.live.attach(&place.home, &place.root, &id).await {
                    command.command = attach;
                    command.line = None;
                }
                result(command)
            }
            _ => Err(Error::Unknown),
        }
    }
}
