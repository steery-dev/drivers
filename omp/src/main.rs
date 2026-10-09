//! Steery's official `omp` driver. It reads omp's conversations from their transcripts,
//! plans the entries that set omp up for Steery, and says how to start, read, stop and
//! resume an omp turn. A live conversation is reached through Steery's plugin door, which
//! setup writes into omp and the runtime speaks directly, so this driver answers no live
//! method.

mod drive;
mod setup;
mod transcripts;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{Value, json};
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
pub(crate) const MINIMUM: &str = "18.4.12";
pub(crate) const UPDATE: &str = "omp update";

/// The driver. It keeps the person's home and the runtime's config folder, given in
/// `initialize`, the models omp listed last, and what each turn's lines said so far.
#[derive(Default)]
struct Omp {
    home: OnceLock<PathBuf>,
    config: OnceLock<PathBuf>,
    catalog: drive::Catalog,
    runs: drive::Runs,
}

impl Omp {
    fn home(&self) -> Result<&Path, Error> {
        self.home
            .get()
            .map(PathBuf::as_path)
            .ok_or_else(|| Error::invalid("`initialize` gave no home"))
    }

    /// omp's agent folder for the person's home.
    fn root(&self) -> Result<PathBuf, Error> {
        root(self.home()?).map_err(|why| Error::refused(refusal::REFUSED, why))
    }
}

impl Program for Omp {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this driver speaks protocol {PROTOCOL} only"
            )));
        }
        let _ = self.home.set(params.home.clone());
        let _ = self.config.set(params.config.clone());
        let facts = wire::HarnessFacts {
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
            acts: vec![
                harness::Act::Continue,
                harness::Act::Fork,
                harness::Act::New,
            ],
        };
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: "omp".into(),
                kind: wire::Kind::Harness,
                release: env!("CARGO_PKG_VERSION").into(),
                label: "omp".into(),
                short: Some("omp".into()),
            },
            levels: vec![wire::Level::Observe, wire::Level::Drive],
            harness: Some(wire::HarnessInfo {
                minimum: Some(wire::Minimum {
                    release: MINIMUM.into(),
                    update: Some(UPDATE.into()),
                }),
                sign_in: Some("run /login".into()),
                knowing: Some(KNOWING.into()),
                transcript: Some(TRANSCRIPT.into()),
            }),
            provides: None,
            facts: Some(serde_json::to_value(facts).map_err(|e| Error::invalid(e.to_string()))?),
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        match method {
            method::SETUP_PLAN => {
                let config = self
                    .config
                    .get()
                    .ok_or_else(|| Error::invalid("`initialize` gave no config folder"))?;
                result(setup::plan(config, steery_sdk::params(params)?).await?)
            }
            method::SESSIONS_LIST => result(transcripts::list(
                &self.root()?,
                steery_sdk::params(params)?,
            )),
            method::SESSIONS_READ => result(transcripts::read(
                &self.root()?,
                steery_sdk::params(params)?,
            )?),
            // omp runs no hooks: Steery's plugin says its moments through the door.
            method::HOOK_READ => {
                let _: harness::HookRead = steery_sdk::params(params)?;
                Ok(json!({ "moment": null }))
            }
            method::HOOK_ANSWER => {
                let _: harness::HookAnswer = steery_sdk::params(params)?;
                result(harness::HookAnswered {
                    stdout: String::new(),
                    exit_code: 0,
                })
            }
            method::HELD => result(drive::held(&self.root()?, steery_sdk::params(params)?)?),
            method::MODELS => {
                let _: wire::Empty = steery_sdk::params(params)?;
                result(self.catalog.list().await)
            }
            method::START => result(
                drive::start(&self.root()?, &self.catalog, steery_sdk::params(params)?).await?,
            ),
            method::TURN_READ => result(self.runs.read(steery_sdk::params(params)?)),
            // The plugin door is omp's graceful step, and the runtime speaks it itself.
            method::STOP => {
                let _: harness::Stop = steery_sdk::params(params)?;
                result(harness::Stopped { stopped: false })
            }
            method::READER => result(drive::reader(steery_sdk::params(params)?)?),
            method::TERMINAL => result(drive::terminal(
                self.home()?,
                &self.root()?,
                steery_sdk::params(params)?,
            )?),
            _ => Err(Error::Unknown),
        }
    }
}

/// omp's active agent folder: `PI_CODING_AGENT_DIR`, else `<PI_CONFIG_DIR|~/.omp>/agent`,
/// moved to `profiles/<name>/agent` under `OMP_PROFILE` or `PI_PROFILE`. omp is a fork of
/// pi and still reads pi's variables.
pub(crate) fn root(home: &Path) -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR").filter(|v| !v.is_empty()) {
        return absolute(PathBuf::from(dir));
    }
    let base = match std::env::var_os("PI_CONFIG_DIR").filter(|v| !v.is_empty()) {
        // omp reads the first of several roots first; guessing among the rest would set up
        // the wrong one.
        Some(value) => absolute(PathBuf::from(
            value
                .to_str()
                .ok_or("PI_CONFIG_DIR is not UTF-8")?
                .split(',')
                .next()
                .unwrap_or_default(),
        ))?,
        None => home.join(".omp"),
    };
    let profile = std::env::var_os("OMP_PROFILE")
        .or_else(|| std::env::var_os("PI_PROFILE"))
        .filter(|v| !v.is_empty());
    Ok(match profile {
        Some(name) => base.join("profiles").join(name).join("agent"),
        None => base.join("agent"),
    })
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
