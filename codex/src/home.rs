//! Where Codex keeps everything, the `codex` binary, and how this driver starts Codex for
//! something that writes no conversation.

use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Codex's home: `CODEX_HOME`, else `~/.codex`. A value naming several roots is
/// ambiguous; the first is the one Codex reads first, and guessing among the rest would
/// read the wrong one.
pub(crate) fn root(home: &Path) -> Result<PathBuf, String> {
    match std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        Some(value) => {
            let first = value
                .to_str()
                .ok_or("CODEX_HOME is not UTF-8")?
                .split(',')
                .next()
                .unwrap_or_default();
            let path = PathBuf::from(first);
            if path.is_absolute() {
                Ok(path)
            } else {
                Err(format!("{} must be an absolute path", path.display()))
            }
        }
        None => Ok(default_root(home)),
    }
}

/// Codex's home where no variable moves it.
pub(crate) fn default_root(home: &Path) -> PathBuf {
    home.join(".codex")
}

/// The first executable named `codex` on `PATH`.
pub(crate) fn binary() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("codex"))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Codex, started by this driver for something that writes no conversation: a release, a
/// model list, a look at a thread's turns. `STEERY_STARTED` says so (PROTOCOL.md §3.3).
pub(crate) fn helper(binary: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(binary);
    command
        .env("STEERY_STARTED", format!("{}:nothing", std::process::id()))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

/// Now, in milliseconds since 1970.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
