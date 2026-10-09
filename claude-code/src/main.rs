//! Steery's official `claude-code` driver, at observe, drive and live. It reads Claude
//! Code's conversations from their transcripts, plans the entries that set Claude Code up
//! for Steery, answers its hooks, plans and follows its background sessions' turns, and
//! reaches a live session through the inbox socket Claude Code opens for it.
//!
//! Run as `steery-driver-claude-code turn …`, it is one turn's follower ([`turn`]), which
//! `start` plans.

mod agents;
mod calls;
mod drive;
mod events;
mod hooks;
mod live;
mod registry;
mod setup;
mod transcripts;
mod turn;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, PROTOCOL, Program, method, refusal, result};

fn main() -> std::process::ExitCode {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some(turn::MODE) {
        return turn::run(args);
    }
    let served = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .and_then(|runtime| runtime.block_on(steery_sdk::serve(|_notifier| ClaudeCode::default())));
    match served {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Steery's MCP server's name in Claude Code's configuration.
pub(crate) const STEERY: &str = "steery";

/// How an agent in Claude Code learns its conversation's id, as Steery's skill says it.
/// Claude Code substitutes `${CLAUDE_SESSION_ID}` into a skill as it loads it: a skill
/// carrying that name was read back holding a UUID, and a transcript named for that same
/// UUID was written under the project. No variable in the agent's environment carries it;
/// the only other place it appears is the hook document, which a hook reads and an agent
/// cannot.
const KNOWING: &str = "Claude Code substitutes the identifier into this text as it loads it, so it is already\n\
                       here: `${CLAUDE_SESSION_ID}`. Send what you are reading there, if what you are reading is\n\
                       a UUID. If it still reads as that name in braces then nothing replaced it, and nothing\n\
                       else on this machine reaches the identifier — no variable in your environment carries it\n\
                       — so describe nothing.";

/// How Claude Code's transcript file reads.
const TRANSCRIPT: &str = "One JSON record per line: `user` and `assistant` records carry `message.content`, \
                          and each names the one before it in `parentUuid`, so the last record's chain is \
                          the conversation.";

/// The driver: where Claude Code keeps everything, from `initialize`, and the last
/// listing of its background sessions.
#[derive(Default)]
struct ClaudeCode {
    place: OnceLock<Place>,
    listing: drive::Listing,
}

/// The person's home, and Claude Code's configuration root with its conversations, or
/// why there is none.
struct Place {
    home: PathBuf,
    store: Result<transcripts::Store, String>,
}

impl ClaudeCode {
    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` came first"))
    }

    fn store(&self) -> Result<&transcripts::Store, Error> {
        self.place()?
            .store
            .as_ref()
            .map_err(|why| Error::refused(refusal::REFUSED, why.clone()))
    }

    fn root(&self) -> Result<&Path, Error> {
        Ok(self.store()?.root())
    }
}

impl Program for ClaudeCode {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this driver speaks protocol {PROTOCOL} only"
            )));
        }
        let _ = self.place.set(Place {
            home: params.home.clone(),
            store: root(&params.home).map(transcripts::Store::new),
        });
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "claude-code".into(),
                kind: wire::Kind::Harness,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "Claude Code".into(),
                short: Some("CC".into()),
            },
            harness: Some(Box::new(info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        use steery_sdk::params as given;
        match method {
            method::SETUP_PLAN => {
                let place = self.place()?;
                let root = place
                    .store
                    .as_ref()
                    .map_err(|why| Error::refused(refusal::REFUSED, why.clone()))?
                    .root();
                result(setup::plan(&place.home, root, given(params)?).await?)
            }
            method::SESSIONS_LIST => result(self.store()?.list(given(params)?)),
            method::SESSIONS_LOCATE => result(self.store()?.locate(given(params)?)?),
            method::SESSIONS_READ => result(self.store()?.read(given(params)?)?),
            method::HOOK_READ => result(hooks::read(given(params)?)?),
            method::HOOK_ANSWER => result(hooks::answer(given(params)?)?),
            method::HELD => result(drive::held(self.root()?, self.store()?, given(params)?)),
            method::IDLE => result(drive::idle(self.root()?, &self.listing, given(params)?).await),
            method::MODELS => {
                let _: wire::Empty = given(params)?;
                result(drive::models(self.root()?))
            }
            method::START => result(drive::start(self.store()?, given(params)?)?),
            method::TURN_READ => result(turn::read(given(params)?)),
            method::STOP => result(drive::stop(self.root()?, given(params)?).await?),
            method::RECEIPT_MARK => {
                result(drive::mark(self.root()?, self.store()?, given(params)?)?)
            }
            method::RECEIPT_FIND => result(drive::find(given(params)?)?),
            method::READER => result(drive::reader(given(params)?)?),
            method::TERMINAL => result(drive::terminal(
                &self.place()?.home,
                self.store()?,
                given(params)?,
            )?),
            method::SAY => result(live::say(self.root()?, given(params)?).await?),
            method::PICK => {
                let _: harness::Pick = given(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Claude Code's inbox keeps the session's own model",
                ))
            }
            // A reader's output is its answer whole, and Claude Code has no recap of its
            // own a live session gives.
            _ => Err(Error::Unknown),
        }
    }
}

/// Claude Code as Steery reads it.
fn info() -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: wire::Level::ALL.to_vec(),
        minimum: None,
        sign_in: Some("run /login".into()),
        knowing: KNOWING.into(),
        transcript: Some(TRANSCRIPT.into()),
        identity: wire::Identity {
            executables: vec!["claude".into()],
            package: Some("@anthropic-ai/claude-code".into()),
            session_variable: None,
        },
        // Claude Code 2.1.280, read in its own code: a tool's description is kept to
        // `CLAUDE_CODE_MAX_MCP_DESCRIPTION_LENGTH` characters, 2,048 when unset, then
        // `… [truncated]`, so the escalation policies are said whole at its start.
        setup: wire::SetupFacts {
            reads_skills: true,
            cuts_tool_text: true,
            call_bound: None,
        },
        reader: Some(wire::ReaderFacts {
            sealed: wire::Sealed {
                tools: true,
                mcp: true,
                instructions: true,
                extensions: true,
                history: true,
            },
            // Anthropic's small model, under the short name Claude Code documents: Claude
            // Code lists no models, and runs this one whatever its settings name.
            known: vec!["claude-haiku-4-5".into()],
            unnamed: true,
        }),
        facts: wire::HarnessFacts {
            by_file: false,
            // Claude Code resumes a conversation by its id from any folder.
            resumes_in_place: false,
            // `--resume` beside a live session is refused, and with `--bg` starts a copy.
            forks_beside: false,
            keeps: wire::Keeps {
                model: true,
                thinking: true,
            },
            door: Some(harness::LiveDoor::Keeps),
            checks_idle: false,
            worktrees: true,
            acts: vec![
                harness::Act::Continue,
                harness::Act::Fork,
                harness::Act::New,
            ],
        },
        hooks: hooks::table(),
    }
}

/// Claude Code's configuration root: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub(crate) fn root(home: &Path) -> Result<PathBuf, String> {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                Ok(dir)
            } else {
                Err(format!("{} must be an absolute path", dir.display()))
            }
        }
        None => Ok(default_root(home)),
    }
}

/// Claude Code's configuration root where no variable moves it.
pub(crate) fn default_root(home: &Path) -> PathBuf {
    home.join(".claude")
}

/// The first executable named `claude` on `PATH`.
pub(crate) fn binary() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("claude"))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Claude Code, started by this driver for something that writes no conversation: a
/// release, a listing of its sessions, a stop. `STEERY_STARTED` says so.
pub(crate) fn helper(binary: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(binary);
    command
        .env("STEERY_STARTED", format!("{}:nothing", std::process::id()))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    command
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// An executable script at `path`, written whole before it is made executable, so a
    /// test never runs one half written.
    pub fn stand_in(path: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let written = path.with_extension("new");
        std::fs::write(&written, script).unwrap();
        std::fs::set_permissions(&written, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&written, path).unwrap();
    }

    #[test]
    fn claude_code_states_itself_as_the_protocol_checks() {
        let initialized = steery_sdk::wire::Initialized {
            protocol: steery_sdk::PROTOCOL,
            program: steery_sdk::wire::ProgramInfo {
                id: "claude-code".into(),
                kind: steery_sdk::wire::Kind::Harness,
                release: "0.1.0".into(),
                label: "Claude Code".into(),
                short: Some("CC".into()),
            },
            harness: Some(Box::new(super::info())),
            machine: None,
            settings: Vec::new(),
        };
        initialized.check().unwrap();
    }
}
