//! Steery's driver for Qwen Code at observe: it reads Qwen's conversations from
//! `<base>/projects/<project>/chats/`, plans the entries that set Qwen up for Steery, reads
//! its hooks' moments and lists the MCP servers it loads.

mod events;
mod hooks;
mod mcp;
mod setup;
mod store;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Program, method, refusal, result};

use crate::Harness;

/// The oldest release whose chat files, hooks and settings this driver reads.
pub(crate) const MINIMUM: &str = "0.27.2";

/// npm's package of Qwen Code.
pub(crate) const PACKAGE: &str = "@qwen-code/qwen-code";

/// The driver: what `initialize` said, and the listings it answered.
#[derive(Default)]
pub struct Driver {
    place: OnceLock<Place>,
    changes: steery_sdk::listing::Changes,
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
            levels: vec![wire::Level::Observe],
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
            _ => Err(Error::Unknown),
        }
    }
}
