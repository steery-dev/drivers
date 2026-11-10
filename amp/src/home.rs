//! Where Amp keeps what this host holds of it, the `amp` binary, and a bounded run of it.
//! Amp keeps its threads on ampcode.com: this host holds only its settings, its skills and
//! the id of its installation.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use steery_driver_common::program::{executable, helper, on_path};
use tokio::io::AsyncReadExt as _;

/// Amp's configuration folder: `$XDG_CONFIG_HOME/amp`, else `~/.config/amp`. Amp names
/// `$XDG_CONFIG_HOME/amp/settings.json` as its default settings file where the variable is
/// set (`amp --help`, 0.0.1794225648).
pub(crate) fn config(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("amp")
}

/// Amp's settings file, where its MCP servers are (`amp.mcpServers`).
pub(crate) fn settings(home: &Path) -> PathBuf {
    config(home).join("settings.json")
}

/// The settings file of a folder, whose servers Amp adds to those of `settings`.
pub(crate) fn workspace_settings(cwd: &Path) -> PathBuf {
    cwd.join(".amp").join("settings.json")
}

/// Amp's data folder: `~/.local/share/amp` on macOS and Windows, else `$XDG_DATA_HOME/amp`
/// where it is set, as Amp's own code reads it.
fn data(home: &Path) -> PathBuf {
    let default = home.join(".local/share");
    let base = match std::env::consts::OS {
        "macos" | "windows" => default,
        _ => std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .unwrap_or(default),
    };
    base.join("amp")
}

/// The id of Amp's installation on this host (`device-id.json`'s `installationID`): the
/// threads made here carry it. None where Amp never ran here. The driver reads nothing
/// else of that folder: it also holds Amp's accounts and secrets.
pub(crate) fn installation(home: &Path) -> Option<String> {
    let value = steery_driver_common::mcp::json(&data(home).join("device-id.json"))?;
    value
        .get("installationID")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// The folder Amp's own installer writes its executable to: `$AMP_HOME/bin`, else
/// `~/.amp/bin`. It links `~/.local/bin/amp` to it.
fn installer_folder() -> Option<PathBuf> {
    let amp_home = match std::env::var_os("AMP_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".amp"),
    };
    Some(amp_home.join("bin"))
}

/// Amp's executable: the first `amp` on `PATH`, else the one in its installer's folder,
/// else its link in `~/.local/bin`.
pub(crate) fn binary() -> Option<PathBuf> {
    on_path("amp")
        .or_else(|| executable(installer_folder()?.join("amp")))
        .or_else(|| executable(PathBuf::from(std::env::var_os("HOME")?).join(".local/bin/amp")))
}

/// A line for a person where `amp` is installed in more than one place here.
pub(crate) fn others_note(used: &Path) -> Option<String> {
    steery_driver_common::program::others_note("amp", "amp", installer_folder().as_deref(), used)
}

/// Amp's own installer, where `bash` and `curl` are here: it installs into `~/.amp/bin` and
/// links `~/.local/bin/amp`. Where not, why, in one line.
pub(crate) fn install() -> Result<steery_sdk::wire::harness::Install, String> {
    if let Some(why) = steery_driver_common::program::lacks(&["bash", "curl"]) {
        return Err(format!("amp's installer {why}"));
    }
    let line = "curl -fsSL https://ampcode.com/install.sh | bash";
    Ok(steery_sdk::wire::harness::Install {
        argv: vec!["bash".into(), "-c".into(), line.into()],
        about: format!(
            "Amp's own installer. It installs the release into ~/.amp/bin and links ~/.local/bin/amp: {line}"
        ),
    })
}

/// Now, in milliseconds since 1970.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// What one run of `amp` printed and how it ended.
pub(crate) struct Ran {
    pub success: bool,
    pub stdout: Vec<u8>,
    /// Its standard error, as text.
    pub said: String,
}

impl Ran {
    /// The last line of its own on standard error, for a person.
    pub fn line(&self) -> Option<&str> {
        self.said.lines().map(str::trim).rfind(|line| !line.is_empty())
    }
}

/// Why a run of `amp` gave no answer.
pub(crate) enum Failed {
    /// It did not start.
    Start(std::io::Error),
    /// It did not end within its time; it is killed.
    Late(Duration),
    /// It printed more than `cap` bytes; it is killed.
    Over(usize),
}

impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Start(e) => write!(f, "amp did not start: {e}"),
            Self::Late(within) => write!(f, "amp did not answer within {} s", within.as_secs()),
            Self::Over(cap) => write!(f, "amp printed more than {} MiB", cap >> 20),
        }
    }
}

/// `amp args` (in `cwd` where given, with `env` added) run to its end within `within`, its
/// standard output read up to `cap` bytes.
pub(crate) async fn run(
    binary: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    env: &[(&str, &str)],
    within: Duration,
    cap: usize,
) -> Result<Ran, Failed> {
    let mut command = helper(binary);
    command
        .args(args)
        .envs(env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command.spawn().map_err(Failed::Start)?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let ran = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let read_out = async {
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                match stdout.read(&mut chunk).await {
                    Ok(0) | Err(_) => return Ok(()),
                    Ok(read) if out.len() + read > cap => return Err(Failed::Over(cap)),
                    Ok(read) => out.extend_from_slice(&chunk[..read]),
                }
            }
        };
        let read_err = async {
            let _ = (&mut stderr).take(64 * 1024).read_to_end(&mut err).await;
        };
        let (read, ()) = tokio::join!(read_out, read_err);
        read?;
        let status = child.wait().await.map_err(Failed::Start)?;
        Ok(Ran {
            success: status.success(),
            stdout: out,
            said: String::from_utf8_lossy(&err).into_owned(),
        })
    };
    tokio::time::timeout(within, ran)
        .await
        .map_err(|_| Failed::Late(within))?
}
