//! Where Copilot is: its home folder, its executable, and its own installer.

use std::path::{Path, PathBuf};

use steery_driver_common::program::{executable, on_path};
use steery_sdk::wire::harness::Install;

/// Copilot's home: `COPILOT_HOME`, else `~/.copilot`.
pub(crate) fn root(home: &Path) -> Result<PathBuf, String> {
    match std::env::var_os("COPILOT_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                Ok(dir)
            } else {
                Err(format!(
                    "COPILOT_HOME {} must be an absolute path",
                    dir.display()
                ))
            }
        }
        None => Ok(home.join(".copilot")),
    }
}

/// Copilot's executable: the first `copilot` on `PATH`, else the one its own installer
/// writes, `~/.local/bin/copilot`, which a shell that has not read its profile since may
/// not have on `PATH`.
pub(crate) fn binary() -> Option<PathBuf> {
    on_path("copilot").or_else(|| executable(installer_folder()?.join("copilot")))
}

/// The folder Copilot's own installer writes to where no `PREFIX` moves it.
fn installer_folder() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/bin"))
}

/// A line for a person where `copilot` is installed in more than one place here: which one
/// Steery runs and which it does not.
pub(crate) fn others_note(used: &Path) -> Option<String> {
    steery_driver_common::program::others_note(
        "copilot",
        "copilot",
        installer_folder().as_deref(),
        used,
    )
}

/// Copilot's own installer, where `bash` and `curl` are here: it installs into
/// `~/.local/bin`. Where not, why, in one line.
pub(crate) fn install() -> Result<Install, String> {
    if let Some(why) = steery_driver_common::program::lacks(&["bash", "curl"]) {
        return Err(format!("copilot's installer {why}"));
    }
    let line = "curl -fsSL https://gh.io/copilot-install | bash";
    Ok(Install {
        argv: vec!["bash".into(), "-c".into(), line.into()],
        about: format!("GitHub Copilot CLI's own installer. It installs into ~/.local/bin: {line}"),
    })
}
