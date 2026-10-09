//! The two harnesses of the pi family, and everything that tells them apart: their names,
//! where each keeps its files, and what Steery reads of each.
//!
//! omp is a fork of pi. Both write the same transcript tree, run the same plugin, read
//! the same `PI_*` variables for their folders and speak the same JSON lines in a print
//! run; what differs is said here once, and every other module asks.

use std::path::{Path, PathBuf};

use steery_driver_common::program::{executable, on_path, release_within, version};
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
                 named none: describe nothing and raise your requests without a `session` —\n\
                 they are first-class requests and your person reads every one of them."
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

    /// The harness as Steery reads it.
    pub fn info(self) -> wire::HarnessInfo {
        wire::HarnessInfo {
            levels: wire::Level::ALL.to_vec(),
            minimum: Some(wire::Minimum {
                release: self.minimum().into(),
                update: Some(format!("{} update", self.id())),
            }),
            sign_in: Some("run /login".into()),
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

    /// The harness's executable: the first on `PATH`, else the one in the folder its own
    /// installer writes it to — omp's `PI_INSTALL_DIR`, else `~/.local/bin`; pi's agent
    /// folder's `bin` — which a shell that has not read its profile since may not have on
    /// `PATH`.
    pub fn binary(self) -> Option<PathBuf> {
        on_path(self.id()).or_else(|| {
            let home = PathBuf::from(std::env::var_os("HOME")?);
            let installed = match self {
                Self::Omp => std::env::var_os("PI_INSTALL_DIR")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/bin")),
                Self::Pi => self.root(&home).ok()?.join("bin"),
            };
            executable(installed.join(self.id()))
        })
    }

    /// The harness's own installer, where what it needs is here: omp's prebuilt binary
    /// into `~/.local/bin`, and pi's, which installs its npm package, into a folder on
    /// `PATH` or its agent folder's `bin`. pi's installer needs Node.js 22.19 or newer and
    /// npm, and without them exits 0 having installed nothing, so it is offered only where
    /// both are here.
    pub async fn install(self) -> Option<harness::Install> {
        on_path("sh")?;
        on_path("curl")?;
        let (line, into) = match self {
            Self::Omp => (
                "curl -fsSL https://omp.sh/install | sh -s -- --binary",
                "omp's own installer, its prebuilt binary into ~/.local/bin",
            ),
            Self::Pi => {
                on_path("npm")?;
                if !node_at_least(PI_NODE).await? {
                    return None;
                }
                (
                    "curl -fsSL https://pi.dev/install.sh | sh",
                    "pi's own installer, its npm package into a folder on PATH or \
                     ~/.pi/agent/bin",
                )
            }
        };
        Some(harness::Install {
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

/// The oldest Node.js pi's installer accepts.
const PI_NODE: (u64, u64, u64) = (22, 19, 0);

/// Whether the `node` on `PATH` is `least` or newer; none where there is no `node`, or it
/// names no release within the probe's time (a broken shim, a version manager waiting on
/// input), which is then killed.
async fn node_at_least(least: (u64, u64, u64)) -> Option<bool> {
    let node = on_path("node")?;
    Some(release_within(&version(&node).await?)? >= least)
}
