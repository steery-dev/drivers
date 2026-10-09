//! Steery's official `omp` driver, at observe, drive and live. It reads omp's
//! conversations from their transcripts, plans the entries that set omp up for Steery, and
//! says how to start, read and resume an omp turn. A live conversation is reached through
//! Steery's plugin door, which setup writes into omp and the runtime speaks directly: so
//! this driver answers no live method, and omp's moments come over that door, never as hook
//! commands.

mod calls;
mod drive;
mod events;
mod setup;
mod transcripts;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire::{self, harness};
use steery_sdk::{Error, PROTOCOL, Program, method, refusal, result};

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    steery_sdk::serve(|_notifier| Omp::default()).await
}

/// How an agent in omp learns its conversation's id, as Steery's skill says it.
const KNOWING: &str = "omp publishes nothing of its own: no variable in your environment carries\n\
                       the identifier, and nothing is substituted into this text. The Steery server\n\
                       names it instead, when the connection names the conversation: the instructions\n\
                       it announces carry that `harness` and `external_id`. Read them there and send\n\
                       exactly those. Instructions that named no conversation mean this connection\n\
                       named none: describe nothing and raise your requests without a `session` —\n\
                       they are first-class requests and your person reads every one of them.";

/// How omp's transcript file reads.
const TRANSCRIPT: &str = "One JSON record per line: a `session` header, then entries with `id` and \
                          `parentId` whose `message.role` is `user`, `assistant` or `toolResult`; the last \
                          entry's chain of parents is the conversation.";

/// The oldest omp this driver speaks to, and the command that updates it. omp 18.4.12 is
/// the first that dials Steery's door from every process, writes the run's end the plugin
/// reads and classifies a context overflow.
const MINIMUM: &str = "18.4.12";
const UPDATE: &str = "omp update";

/// The driver: where omp keeps everything, from `initialize`, the models omp listed last,
/// and what each turn's lines said so far.
#[derive(Default)]
struct Omp {
    place: OnceLock<Place>,
    catalog: drive::Catalog,
    runs: drive::Runs,
}

/// The person's home, the runtime's config folder, and omp's conversations under its
/// agent folder, or why there is none.
struct Place {
    home: PathBuf,
    config: PathBuf,
    store: Result<transcripts::Store, String>,
}

impl Omp {
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
}

impl Program for Omp {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this driver speaks protocol {PROTOCOL} only"
            )));
        }
        let _ = self.place.set(Place {
            home: params.home.clone(),
            config: params.config.clone(),
            store: root(&params.home).map(transcripts::Store::new),
        });
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "omp".into(),
                kind: wire::Kind::Harness,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "omp".into(),
                short: Some("omp".into()),
            },
            harness: Some(Box::new(info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::SETUP_PLAN => {
                let place = self.place()?;
                result(setup::plan(&place.home, &place.config, steery_sdk::params(params)?).await?)
            }
            method::SESSIONS_LIST => result(self.store()?.list(steery_sdk::params(params)?)),
            method::SESSIONS_LOCATE => result(self.store()?.locate(steery_sdk::params(params)?)?),
            method::SESSIONS_READ => result(self.store()?.read(steery_sdk::params(params)?)?),
            method::HELD => result(drive::held(self.store()?, steery_sdk::params(params)?)),
            method::IDLE => result(drive::idle(self.store()?, steery_sdk::params(params)?)),
            method::MODELS => {
                let _: wire::Empty = steery_sdk::params(params)?;
                let place = self.place()?;
                result(self.catalog.list(&place.home, self.store()?.root()).await?)
            }
            method::START => result(
                drive::start(self.store()?, &self.catalog, steery_sdk::params(params)?).await?,
            ),
            method::TURN_READ => result(self.runs.read(steery_sdk::params(params)?)),
            method::READER => result(drive::reader(steery_sdk::params(params)?)?),
            method::TERMINAL => result(drive::terminal(
                &self.place()?.home,
                self.store()?,
                steery_sdk::params(params)?,
            )?),
            // Hooks come over the plugin door, and `stop`, `receipt/*`, `say`, `pick` and
            // `recap` are the door's, which the runtime speaks itself; a reader's output is
            // its answer whole.
            _ => Err(Error::Unknown),
        }
    }
}

/// omp as Steery reads it.
fn info() -> wire::HarnessInfo {
    wire::HarnessInfo {
        levels: wire::Level::ALL.to_vec(),
        minimum: Some(wire::Minimum {
            release: MINIMUM.into(),
            update: Some(UPDATE.into()),
        }),
        sign_in: Some("run /login".into()),
        knowing: KNOWING.into(),
        transcript: Some(TRANSCRIPT.into()),
        identity: wire::Identity {
            executables: vec!["omp".into()],
            package: Some("@oh-my-pi".into()),
            session_variable: None,
        },
        setup: wire::SetupFacts {
            reads_skills: true,
            cuts_tool_text: false,
            call_bound: None,
        },
        // Everything a flag closes is closed; omp's MCP servers arrive through discovery,
        // which no switch for one run turns off. Told no model, it runs its own default.
        reader: Some(wire::ReaderFacts {
            sealed: wire::Sealed {
                tools: true,
                mcp: false,
                instructions: true,
                extensions: true,
                history: true,
            },
            known: Vec::new(),
            unnamed: true,
        }),
        facts: wire::HarnessFacts {
            by_file: false,
            resumes_in_place: true,
            forks_beside: true,
            // omp resumes on the model its transcript's last `model_change` names and the
            // level of its last `thinking_level_change`, and writes neither for `--model` or
            // `--thinking` given to a conversation it already has (omp 18.4.12).
            keeps: wire::Keeps {
                model: false,
                thinking: false,
            },
            door: Some(harness::LiveDoor::Plugin),
            checks_idle: true,
            worktrees: false,
            acts: vec![
                harness::Act::Continue,
                harness::Act::Fork,
                harness::Act::New,
            ],
        },
        hooks: hooks(),
    }
}

/// omp's moments, as Steery's plugin says them over the door: `session_start`,
/// `before_agent_start`, `tool_call`, `tool_result`, the turn's end and
/// `session_shutdown`. The plugin adds a paragraph beside the person's words where they
/// arrive, a line beside a shell command's result through `tool_call`, and a paragraph
/// for a sub-agent's own copy at a call's result; a turn's end is kept going through the
/// door's `settle`, not a moment.
fn hooks() -> Vec<harness::Hook> {
    use harness::{HookKind as Kind, Says};
    [
        ("SessionStart", Kind::SessionStart, &[][..]),
        (
            "UserPromptSubmit",
            Kind::Prompt,
            &[Says::Context, Says::Notice][..],
        ),
        ("PreToolUse", Kind::BeforeCall, &[Says::Calls][..]),
        ("PostToolUse", Kind::AfterCall, &[Says::Working][..]),
        ("Stop", Kind::TurnEnd, &[][..]),
        ("SessionEnd", Kind::SessionEnd, &[][..]),
    ]
    .into_iter()
    .map(|(event, kind, says)| harness::Hook {
        event: event.into(),
        kind,
        via: harness::Via::Door,
        says: says.to_vec(),
        waits: None,
        env: Vec::new(),
    })
    .collect()
}

/// omp's active agent folder: `PI_CODING_AGENT_DIR`, else `<base>/agent`, moved to
/// `profiles/<name>/agent` under `OMP_PROFILE` or `PI_PROFILE`. omp is a fork of pi and
/// still reads pi's variables.
pub(crate) fn root(home: &Path) -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR").filter(|v| !v.is_empty()) {
        return absolute(PathBuf::from(dir));
    }
    let base = base(home)?;
    let profile = std::env::var_os("OMP_PROFILE")
        .or_else(|| std::env::var_os("PI_PROFILE"))
        .filter(|v| !v.is_empty());
    Ok(match profile {
        Some(name) => base.join("profiles").join(name).join("agent"),
        None => base.join("agent"),
    })
}

/// omp's config folder: `PI_CONFIG_DIR`, else `~/.omp`. omp reads the first of several
/// roots first; guessing among the rest would read the wrong one.
pub(crate) fn base(home: &Path) -> Result<PathBuf, String> {
    match std::env::var_os("PI_CONFIG_DIR").filter(|v| !v.is_empty()) {
        Some(value) => absolute(PathBuf::from(
            value
                .to_str()
                .ok_or("PI_CONFIG_DIR is not UTF-8")?
                .split(',')
                .next()
                .unwrap_or_default(),
        )),
        None => Ok(home.join(".omp")),
    }
}

/// omp's agent folder where no variable moves it.
pub(crate) fn default_root(home: &Path) -> PathBuf {
    home.join(".omp/agent")
}

fn absolute(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!("{} must be an absolute path", path.display()))
    }
}

/// The first executable named `omp` on `PATH`.
pub(crate) fn binary() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("omp"))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// omp, started by this driver for something that writes no conversation: a release, a
/// model catalog. `STEERY_STARTED` says so to Steery's plugin, which then stays out.
pub(crate) fn helper(binary: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(binary);
    command
        .env("STEERY_STARTED", format!("{}:nothing", std::process::id()))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    command
}
