//! Where Codex keeps everything, and the `codex` binary.

use std::path::{Path, PathBuf};

use steery_driver_common::program::{executable, on_path};

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

/// The folder Codex's own installer writes its executable to: `CODEX_INSTALL_DIR`, else
/// `~/.local/bin`, which a shell that has not read its profile since may not have on
/// `PATH`.
fn installer_folder() -> Option<PathBuf> {
    match std::env::var_os("CODEX_INSTALL_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/bin")),
    }
}

/// Codex's executable: the first `codex` on `PATH`, else the one in its installer's folder.
pub(crate) fn binary() -> Option<PathBuf> {
    on_path("codex").or_else(|| executable(installer_folder()?.join("codex")))
}

/// A line for a person where `codex` is installed in more than one place here: which one
/// Steery runs and which it does not.
pub(crate) fn others_note(used: &Path) -> Option<String> {
    steery_driver_common::program::others_note(
        "codex",
        "codex",
        installer_folder().as_deref(),
        used,
    )
}

/// Codex's own installer, where `sh` and `curl` are here: its release into
/// `~/.local/bin`, asking nothing (`CODEX_NON_INTERACTIVE`: it would offer to start Codex).
/// Where not, why, in one line.
pub(crate) fn install() -> Result<steery_sdk::wire::harness::Install, String> {
    if let Some(why) = steery_driver_common::program::lacks(&["sh", "curl"]) {
        return Err(format!("codex's installer {why}"));
    }
    let line = "curl -fsSL https://chatgpt.com/codex/install.sh | CODEX_NON_INTERACTIVE=1 sh";
    Ok(steery_sdk::wire::harness::Install {
        argv: vec!["sh".into(), "-c".into(), line.into()],
        about: format!("Codex's own installer, its release into ~/.local/bin: {line}"),
    })
}

/// Now, in milliseconds since 1970.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
