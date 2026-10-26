//! Steery's `claude-code` driver, at observe, drive and live. It reads Claude
//! Code's conversations from their transcripts, plans the entries that set Claude Code up
//! for Steery, answers its hooks, plans and follows its background sessions' turns,
//! reaches a live session through the inbox socket Claude Code opens for it, and says how
//! each live session stands ([`state`]).
//!
//! Run as `steery-driver-claude-code turn …`, it is one turn's follower ([`turn`]), which
//! `start` plans.

mod agents;
mod calls;
mod context;
mod drive;
mod events;
mod hooks;
mod live;
mod mcp;
mod registry;
mod setup;
mod state;
mod transcripts;
mod tree;
mod turn;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use steery_driver_common::program::{executable, on_path};
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, Notifier, Program, method, refusal, result};

/// Claude Code's mark: one SVG path's `d` in a 24 by 24 box, filled with the even-odd rule.
const MARK: &str = "M4.709 15.955l4.72-2.647.08-.23-.08-.128H9.2l-.79-.048-2.698-.073-2.339-.097-2.266-.122-.571-.121L0 11.784l.055-.352.48-.321.686.06 1.52.103 2.278.158 1.652.097 2.449.255h.389l.055-.157-.134-.098-.103-.097-2.358-1.596-2.552-1.688-1.336-.972-.724-.491-.364-.462-.158-1.008.656-.722.881.06.225.061.893.686 1.908 1.476 2.491 1.833.365.304.145-.103.019-.073-.164-.274-1.355-2.446-1.446-2.49-.644-1.032-.17-.619a2.97 2.97 0 01-.104-.729L6.283.134 6.696 0l.996.134.42.364.62 1.414 1.002 2.229 1.555 3.03.456.898.243.832.091.255h.158V9.01l.128-1.706.237-2.095.23-2.695.08-.76.376-.91.747-.492.584.28.48.685-.067.444-.286 1.851-.559 2.903-.364 1.942h.212l.243-.242.985-1.306 1.652-2.064.73-.82.85-.904.547-.431h1.033l.76 1.129-.34 1.166-1.064 1.347-.881 1.142-1.264 1.7-.79 1.36.073.11.188-.02 2.856-.606 1.543-.28 1.841-.315.833.388.091.395-.328.807-1.969.486-2.309.462-3.439.813-.042.03.049.061 1.549.146.662.036h1.622l3.02.225.79.522.474.638-.079.485-1.215.62-1.64-.389-3.829-.91-1.312-.329h-.182v.11l1.093 1.068 2.006 1.81 2.509 2.33.127.578-.322.455-.34-.049-2.205-1.657-.851-.747-1.926-1.62h-.128v.17l.444.649 2.345 3.521.122 1.08-.17.353-.608.213-.668-.122-1.374-1.925-1.415-2.167-1.143-1.943-.14.08-.674 7.254-.316.37-.729.28-.607-.461-.322-.747.322-1.476.389-1.924.315-1.53.286-1.9.17-.632-.012-.042-.14.018-1.434 1.967-2.18 2.945-1.726 1.845-.414.164-.717-.37.067-.662.401-.589 2.388-3.036 1.44-1.882.93-1.086-.006-.158h-.055L4.132 18.56l-1.13.146-.487-.456.061-.746.231-.243 1.908-1.312-.006.006z";

fn main() -> std::process::ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some(turn::MODE) => return turn::run(args),
        Some(turn::HEARD) => return turn::heard(args),
        _ => {}
    }
    // Many threads: most calls read files without yielding, and on one thread a slow one
    // stops standard input being read; the runtime then kills the driver with every call in
    // flight.
    let served = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .and_then(|runtime| runtime.block_on(steery_sdk::serve(ClaudeCode::new)));
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

/// The driver: where Claude Code keeps everything, from `initialize`, the last listing of
/// its background sessions, and where it says how live sessions stand ([`state`]).
struct ClaudeCode {
    place: OnceLock<Place>,
    listing: drive::Listing,
    notifier: Notifier,
}

/// The person's home, and Claude Code's configuration root with its conversations, or
/// why there is none.
struct Place {
    home: PathBuf,
    store: Result<Arc<transcripts::Store>, String>,
}

impl ClaudeCode {
    fn new(notifier: Notifier) -> Self {
        Self {
            place: OnceLock::new(),
            listing: drive::Listing::default(),
            notifier,
        }
    }

    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` came first"))
    }

    fn store(&self) -> Result<&transcripts::Store, Error> {
        self.shared().map(|store| &**store)
    }

    /// The store, to hand to work off the async threads.
    fn shared(&self) -> Result<&Arc<transcripts::Store>, Error> {
        self.place()?
            .store
            .as_ref()
            .map_err(|why| Error::refused(refusal::REFUSED, why.clone()))
    }

    fn root(&self) -> Result<&Path, Error> {
        Ok(self.store()?.root())
    }
}

/// Run blocking work off the async threads.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Error::refused(refusal::REFUSED, e.to_string()))?
}

impl Program for ClaudeCode {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        let protocol = steery_sdk::pick(&params.protocol)
            .ok_or_else(|| Error::invalid("this driver speaks protocols 3 and 4"))?;
        let store = root(&params.home).map(|root| Arc::new(transcripts::Store::new(root)));
        let place = Place {
            home: params.home.clone(),
            store,
        };
        // The first `initialize` places the driver and starts the one watcher, which runs
        // until the process ends; the SDK says nothing to a program as it shuts down.
        if self.place.set(place).is_ok()
            && let Ok(store) = &self.place()?.store
        {
            state::watch(store.root(), Arc::clone(store), self.notifier.clone());
        }
        Ok(wire::Initialized {
            protocol,
            program: wire::ProgramInfo {
                id: "claude-code".parse().map_err(Error::invalid)?,
                kind: wire::Kind::Harness,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "Claude Code".into(),
                short: "CC".into(),
                tint: Some("#c96a4a".into()),
                mark: Some(MARK.into()),
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
            method::SESSIONS_FOLD => {
                let _: harness::Fold = given(params)?;
                Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Claude Code has no plugin door to carry its records",
                ))
            }
            method::MCP_LIST => result(mcp::list(
                &self.place()?.home,
                self.root()?,
                given(params)?,
            )),
            method::HOOK_READ => {
                let store = Arc::clone(self.shared()?);
                let read = given(params)?;
                result(blocking(move || hooks::read(&store, read)).await?)
            }
            method::HOOK_ANSWER => result(hooks::answer(given(params)?)?),
            method::HELD => result(drive::held(self.root()?, self.store()?, given(params)?)),
            method::IDLE => result(
                drive::idle(self.root()?, self.store()?, &self.listing, given(params)?).await,
            ),
            method::MODELS => {
                let _: wire::Empty = given(params)?;
                result(drive::models(&self.place()?.home, self.root()?).await)
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
            method::RECAP => result(live::recap(Arc::clone(self.shared()?), given(params)?).await?),
            // A reader's output is its answer whole: there is no `reader/read`.
            _ => Err(Error::Unknown),
        }
    }
}

/// Claude Code as Steery reads it.
fn info() -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: wire::Level::ALL.to_vec(),
        // The first release whose `claude agents --json --all` lists a background session
        // that has ended (2.1.168's `agents` has no `--all`), which is how a turn is seen
        // to end.
        minimum: Some(wire::Minimum {
            release: "2.1.169".into(),
            update: Some("claude update".into()),
        }),
        // What a person does at the machine, read on their phone: `/login` works only inside
        // an interactive `claude`, and read as Steery's own sign-out (Ali's first run,
        // #product Slack thread 2026-10-19). `claude auth login --help`: "Sign in to your
        // Anthropic account".
        sign_in: Some("run `claude auth login` in a terminal".into()),
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
            // Claude Code's small model, by the alias Claude Code documents
            // (https://code.claude.com/docs/en/model-config): it resolves to the newest Haiku
            // the account's provider serves, whatever its settings name.
            known: vec!["haiku".into()],
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

/// Claude Code's executable: the first `claude` on `PATH`, else the one its own installer
/// writes, `~/.local/bin/claude`, which a shell that has not read its profile since may
/// not have on `PATH`.
pub(crate) fn binary() -> Option<PathBuf> {
    on_path("claude")
        .or_else(|| executable(PathBuf::from(std::env::var_os("HOME")?).join(".local/bin/claude")))
}

/// The file Claude Code keeps its state and its user- and local-scope MCP servers in:
/// `~/.claude.json`, or `.claude.json` inside the folder `CLAUDE_CONFIG_DIR` moves the root
/// to. It also keeps the account a login signed in (`oauthAccount`), so a login rewrites it.
pub(crate) fn state_file(home: &Path, root: &Path) -> PathBuf {
    if root == default_root(home) {
        home.join(".claude.json")
    } else {
        root.join(".claude.json")
    }
}

/// Claude Code's own installer, where `bash` and `curl` are here: its native build into
/// `~/.local/bin`.
pub(crate) fn install() -> Option<steery_sdk::wire::harness::Install> {
    on_path("bash")?;
    on_path("curl")?;
    let line = "curl -fsSL https://claude.ai/install.sh | bash";
    Some(steery_sdk::wire::harness::Install {
        argv: vec!["bash".into(), "-c".into(), line.into()],
        about: format!("Claude Code's own installer, its native build into ~/.local/bin: {line}"),
    })
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
            protocol: 4,
            program: steery_sdk::wire::ProgramInfo {
                id: "claude-code".parse().unwrap(),
                kind: steery_sdk::wire::Kind::Harness,
                release: "0.1.0".into(),
                label: "Claude Code".into(),
                short: "CC".into(),
                tint: Some("#c96a4a".into()),
                mark: Some(super::MARK.into()),
            },
            harness: Some(Box::new(super::info())),
            machine: None,
            settings: Vec::new(),
        };
        initialized.check().unwrap();
    }
}
