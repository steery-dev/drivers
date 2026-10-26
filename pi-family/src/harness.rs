//! The two harnesses of the pi family, and everything that tells them apart: their names,
//! where each keeps its files, and what Steery reads of each.
//!
//! omp is a fork of pi. Both write the same transcript tree, run the same plugin, read
//! the same `PI_*` variables for their folders and run kept in their RPC mode; what differs
//! is said here once, and every other module asks.

use std::path::{Path, PathBuf};

use steery_driver_common::program::{
    ask, executable, helper, lacks, on_path, others_note, release_within,
};
use steery_sdk::wire::{self, harness};

/// A harness of the pi family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    Omp,
    Pi,
}

impl Harness {
    /// The driver's id, which is the harness's executable.
    pub fn id(self) -> &'static str {
        match self {
            Self::Omp => "omp",
            Self::Pi => "pi",
        }
    }

    /// Its name as a person reads it.
    pub fn label(self) -> &'static str {
        self.id()
    }

    /// Its name on a tile.
    pub fn short(self) -> &'static str {
        self.id()
    }

    /// Its colour on a tile.
    pub fn tint(self) -> &'static str {
        match self {
            Self::Omp => "#6e62a8",
            Self::Pi => "#2e8c80",
        }
    }

    /// The package its processes run from, as a path component of a JavaScript runtime's
    /// argument.
    fn package(self) -> &'static str {
        match self {
            Self::Omp => "@oh-my-pi",
            Self::Pi => "pi-coding-agent",
        }
    }

    /// The oldest release this driver speaks to. omp 18.4.12 is the first that dials
    /// Steery's door from every process, writes the run's end the plugin reads and
    /// classifies a context overflow; pi 1.0.0 the first with the extension events and
    /// `registerMcpServer` the plugin needs.
    fn minimum(self) -> &'static str {
        match self {
            Self::Omp => "18.4.12",
            Self::Pi => "1.0.0",
        }
    }

    /// How an agent in this harness learns its conversation's id, as Steery's skill says it.
    fn knowing(self) -> &'static str {
        match self {
            Self::Omp => {
                "omp publishes nothing of its own: no variable in your environment carries\n\
                 the identifier, and nothing is substituted into this text. The Steery server\n\
                 names it instead, when the connection names the conversation: the instructions\n\
                 it announces carry that `harness` and `external_id`. Read them there and send\n\
                 exactly those. Instructions that named no conversation mean this connection\n\
                 named none: describe nothing."
            }
            Self::Pi => {
                "Steery's paragraph, added each time your person writes to you, names it, as `harness` and `external_id`,\n\
                 until this conversation is described: send exactly those. `PI_SESSION_ID`, in the\n\
                 environment of every shell pi starts for you, holds the same `external_id` at any time —\n\
                 pi names this conversation's own file from it. Neither there — no paragraph named one, and\n\
                 the variable is unset or empty, which a person can turn off — is no identifier rather than\n\
                 a different one: describe nothing."
            }
        }
    }

    /// What a person does at the machine to log in, read on their phone. omp has a terminal
    /// counterpart of `/login` (`omp login`: "Log in to a model provider (terminal
    /// counterpart of /login)"). pi has none (its commands are `install`, `remove`,
    /// `uninstall`, `update`, `list`, `config`, `auth`, `mcp`; `auth` prints credentials and
    /// checks readiness), so its login is typed inside it.
    fn sign_in(self) -> &'static str {
        match self {
            Self::Omp => "run `omp login` in a terminal",
            Self::Pi => "run `pi` in a terminal and type /login in it",
        }
    }

    /// The harness as Steery reads it.
    pub fn info(self) -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: wire::Level::ALL.to_vec(),
            minimum: Some(wire::Minimum {
                release: self.minimum().into(),
                update: Some(format!("{} update", self.id())),
            }),
            sign_in: Some(self.sign_in().into()),
            knowing: self.knowing().into(),
            transcript: Some(
                "One JSON record per line: a `session` header, then entries with `id` and \
                 `parentId` whose `message.role` is `user`, `assistant` or `toolResult`; the \
                 last entry's chain of parents is the conversation."
                    .into(),
            ),
            identity: wire::Identity {
                executables: match self {
                    Self::Omp => vec!["omp".into()],
                    // pi renames its process `pi`, and `pi-rpc` in RPC mode.
                    Self::Pi => vec!["pi".into(), "pi-rpc".into()],
                },
                package: Some(self.package().into()),
                // pi puts `PI_SESSION_ID` only on the shells its agent runs, never on its
                // own process, so the variable inherited by a pi names no lead.
                session_variable: None,
            },
            setup: wire::SetupFacts {
                reads_skills: true,
                cuts_tool_text: false,
                call_bound: None,
            },
            reader: Some(wire::ReaderFacts {
                sealed: wire::Sealed {
                    tools: true,
                    // omp's MCP servers arrive through discovery, which no switch for one
                    // run turns off; pi's reader turns its MCP extension off.
                    mcp: self == Self::Pi,
                    instructions: true,
                    // pi's providers may come from extensions, so its reader keeps them.
                    extensions: self == Self::Omp,
                    history: true,
                },
                known: Vec::new(),
                // Told no model, omp runs its own default; pi's reader is always given one.
                unnamed: self == Self::Omp,
            }),
            facts: wire::HarnessFacts {
                // pi continues a conversation by its transcript (`--session <file>`).
                by_file: self == Self::Pi,
                resumes_in_place: true,
                forks_beside: true,
                // pi resumes on the model that answered last; omp on the model its last
                // `model_change` names, and writes none for `--model` given to a
                // conversation it already has. Neither writes `--thinking` given to one.
                keeps: wire::Keeps {
                    model: self == Self::Pi,
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
            hooks: self.hooks(),
        }
    }

    /// The harness's moments, as Steery's plugin says them over the door: `session_start`,
    /// `before_agent_start`, `tool_call`, `tool_result`, the turn's end and
    /// `session_shutdown`. The plugin adds a paragraph beside the person's words where they
    /// arrive; in omp also a line beside a shell command's result through `tool_call`, and
    /// a paragraph for a sub-agent's own copy at a call's result. A turn's end is kept
    /// going through the door's `settle`, not a moment.
    fn hooks(self) -> Vec<harness::Hook> {
        use harness::{HookKind as Kind, Says};
        let omp = self == Self::Omp;
        [
            ("SessionStart", Kind::SessionStart, Vec::new()),
            (
                "UserPromptSubmit",
                Kind::Prompt,
                vec![Says::Context, Says::Notice],
            ),
            (
                "PreToolUse",
                Kind::BeforeCall,
                if omp { vec![Says::Calls] } else { Vec::new() },
            ),
            (
                "PostToolUse",
                Kind::AfterCall,
                if omp { vec![Says::Working] } else { Vec::new() },
            ),
            ("Stop", Kind::TurnEnd, Vec::new()),
            ("SessionEnd", Kind::SessionEnd, Vec::new()),
        ]
        .into_iter()
        .map(|(event, kind, says)| harness::Hook {
            event: event.into(),
            kind,
            via: harness::Via::Door,
            says,
            waits: None,
            env: Vec::new(),
        })
        .collect()
    }

    /// The harness's active agent folder: `PI_CODING_AGENT_DIR`, else for pi `~/.pi/agent`,
    /// and for omp `<base>/agent`, moved to `profiles/<name>/agent` under `OMP_PROFILE` or
    /// `PI_PROFILE`.
    pub fn root(self, home: &Path) -> Result<PathBuf, String> {
        if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR").filter(|v| !v.is_empty()) {
            return absolute(PathBuf::from(dir));
        }
        if self == Self::Pi {
            return Ok(self.default_root(home));
        }
        let base = self.base(home)?;
        let profile = std::env::var_os("OMP_PROFILE")
            .or_else(|| std::env::var_os("PI_PROFILE"))
            .filter(|v| !v.is_empty());
        Ok(match profile {
            Some(name) => base.join("profiles").join(name).join("agent"),
            None => base.join("agent"),
        })
    }

    /// omp's config folder: `PI_CONFIG_DIR`, else `~/.omp`. omp reads the first of several
    /// roots first; guessing among the rest would read the wrong one. pi's is its agent
    /// folder's parent.
    pub fn base(self, home: &Path) -> Result<PathBuf, String> {
        if self == Self::Pi {
            return Ok(home.join(".pi"));
        }
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

    /// The agent folder where no variable moves it.
    pub fn default_root(self, home: &Path) -> PathBuf {
        match self {
            Self::Omp => home.join(".omp/agent"),
            Self::Pi => home.join(".pi/agent"),
        }
    }

    /// The other harness of the family.
    pub fn other(self) -> Self {
        match self {
            Self::Omp => Self::Pi,
            Self::Pi => Self::Omp,
        }
    }

    /// The folder the harness's own installer writes it to — omp's `PI_INSTALL_DIR`, else
    /// `~/.local/bin`; pi's agent folder's `bin` — which a shell that has not read its
    /// profile since may not have on `PATH`.
    fn installer_folder(self) -> Option<PathBuf> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        Some(match self {
            Self::Omp => std::env::var_os("PI_INSTALL_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/bin")),
            Self::Pi => self.root(&home).ok()?.join("bin"),
        })
    }

    /// The harness's executable: the first on `PATH`, else the one in its installer's folder
    /// ([`Self::installer_folder`]).
    pub fn binary(self) -> Option<PathBuf> {
        on_path(self.id()).or_else(|| executable(self.installer_folder()?.join(self.id())))
    }

    /// A line for a person where the harness is installed in more than one place: which
    /// one Steery runs and which it does not.
    pub fn others_note(self, used: &Path) -> Option<String> {
        others_note(
            self.id(),
            self.id(),
            self.installer_folder().as_deref(),
            used,
        )
    }

    /// The harness's own installer, where what it needs is here: omp's prebuilt binary
    /// into `~/.local/bin`, and pi's, which installs its npm package, into a folder on
    /// `PATH` or its agent folder's `bin`. pi's installer needs Node.js 22.19 or newer and
    /// npm, and without them exits 0 having installed nothing, so it is offered only where
    /// both are here. Where it is not offered, why, in one line (`Planned::refused`).
    pub async fn install(self) -> Result<harness::Install, String> {
        let (line, into) = match self {
            Self::Omp => (
                "curl -fsSL https://omp.sh/install | sh -s -- --binary",
                "omp's own installer, its prebuilt binary into ~/.local/bin",
            ),
            Self::Pi => (
                "curl -fsSL https://pi.dev/install.sh | sh",
                "pi's own installer, its npm package into a folder on PATH or \
                 ~/.pi/agent/bin",
            ),
        };
        let mut why: Vec<String> = lacks(&["sh", "curl"]).into_iter().collect();
        if self == Self::Pi {
            let mut parts: Vec<String> = Vec::new();
            if on_path("npm").is_none() {
                parts.push("`npm` is not on PATH".into());
            }
            parts.extend(node_lacks().await);
            if !parts.is_empty() {
                why.push(format!(
                    "needs Node.js 22.19 or newer and npm ({})",
                    parts.join("; ")
                ));
            }
        }
        if !why.is_empty() {
            return Err(format!("{}'s installer {}", self.id(), why.join(", and ")));
        }
        Ok(harness::Install {
            argv: vec!["sh".into(), "-c".into(), line.into()],
            about: format!("{into}: {line}"),
        })
    }

    /// The executable, or `no-binary`.
    pub fn need_binary(self) -> Result<PathBuf, steery_sdk::Error> {
        self.binary().ok_or_else(|| {
            steery_sdk::Error::refused(
                steery_sdk::refusal::NO_BINARY,
                format!("{} is not installed here", self.id()),
            )
        })
    }
}

fn absolute(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!("{} must be an absolute path", path.display()))
    }
}

/// omp's settings files in its agent folder `root`, the one it reads first first. omp
/// writes `config.yml`, but where only `config.yaml` stands it reads that and goes on
/// writing it (`MAIN_CONFIG_FILENAMES`, `Settings`' load, in omp's own code), so both are
/// real.
pub const OMP_CONFIG: [&str; 2] = ["config.yml", "config.yaml"];

/// The value omp's `config.yml` gives `key` in its top-level block `section`
/// (`section:` then indented `key: value`), unquoted.
pub fn omp_setting(root: &Path, section: &str, key: &str) -> Option<String> {
    let text = OMP_CONFIG
        .iter()
        .find_map(|name| std::fs::read_to_string(root.join(name)).ok())?;
    let mut inside = false;
    text.lines().find_map(|line| {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            return None;
        }
        if !line.starts_with([' ', '\t']) {
            inside = line.trim_end().strip_suffix(':') == Some(section);
            return None;
        }
        inside
            .then(|| line.trim_start().strip_prefix(key)?.strip_prefix(':'))
            .flatten()
            .map(|value| value.trim().trim_matches(['"', '\'']).to_owned())
    })
}

/// The oldest Node.js pi's installer accepts.
const PI_NODE: (u64, u64, u64) = (22, 19, 0);

/// Why pi's installer cannot run here, as a line a person reads: no `node` on `PATH`, a
/// `node` older than [`PI_NODE`], one whose `--version` failed (its exit), one that did not
/// start or name a release within the probe's time (a broken shim, a version manager
/// waiting on input), which is then killed, or one that names no release. None where `node`
/// is new enough.
async fn node_lacks() -> Option<String> {
    let Some(node) = on_path("node") else {
        return Some("`node` is not on PATH".into());
    };
    let mut command = helper(&node);
    command.arg("--version");
    let answer = match ask(command).await {
        Ok(answer) => answer,
        Err(why) => return Some(format!("`node --version` {why}")),
    };
    if answer.code != Some(0) {
        return Some(format!(
            "`node --version` exited {}",
            answer.code.map_or("by signal".to_owned(), |code| code.to_string())
        ));
    }
    let said = String::from_utf8_lossy(&answer.stdout).trim().to_owned();
    match release_within(&said) {
        None => Some(format!("`node --version` said `{said}`, which names no release")),
        Some(release) if release < PI_NODE => Some(format!("`node` is {said}")),
        Some(_) => None,
    }
}
