//! Where OpenHands keeps everything, and the `openhands` executable.

use std::path::{Path, PathBuf};

use steery_driver_common::program::{executable, lacks, on_path};
use steery_sdk::wire::harness::Install;

/// OpenHands' own installer, which puts the `openhands` executable in `/usr/local/bin` where
/// that folder is writable, else in `~/.local/bin` (`install.openhands.dev/install.sh`).
const INSTALLER: &str = "curl -fsSL https://install.openhands.dev/install.sh | bash";

/// The executable.
pub(crate) const EXECUTABLE: &str = "openhands";

/// OpenHands' folders, read once from the person's home and the environment, as
/// `openhands_cli/locations.py` reads them.
#[derive(Clone, Debug)]
pub(crate) struct Place {
    /// The person's home.
    pub home: PathBuf,
    /// OpenHands' folder: `OPENHANDS_PERSISTENCE_DIR`, else `~/.openhands`.
    pub root: PathBuf,
    /// The folder of every conversation: `OPENHANDS_CONVERSATIONS_DIR`, else
    /// `<root>/conversations`.
    pub conversations: PathBuf,
}

impl Place {
    pub fn new(home: &Path) -> Self {
        let set = |name: &str| {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        };
        let root = set("OPENHANDS_PERSISTENCE_DIR").unwrap_or_else(|| home.join(".openhands"));
        let conversations =
            set("OPENHANDS_CONVERSATIONS_DIR").unwrap_or_else(|| root.join("conversations"));
        Self {
            home: home.to_owned(),
            root,
            conversations,
        }
    }

    /// The settings that name the one model every conversation runs on
    /// (`AGENT_SETTINGS_PATH`).
    pub fn settings(&self) -> PathBuf {
        self.root.join("agent_settings.json")
    }

    /// The person's MCP servers (`MCP_CONFIG_FILE`).
    pub fn mcp(&self) -> PathBuf {
        self.root.join("mcp.json")
    }

    /// The folders the installer puts the command in.
    fn installed_folders(&self) -> [PathBuf; 2] {
        [
            PathBuf::from("/usr/local/bin"),
            self.home.join(".local/bin"),
        ]
    }

    /// The `openhands` in an installer's folder.
    pub fn installed(&self) -> Option<PathBuf> {
        self.installed_folders()
            .into_iter()
            .find_map(|folder| executable(folder.join(EXECUTABLE)))
    }

    /// The `openhands` on `PATH`, else the one in an installer's folder.
    pub fn binary(&self) -> Option<PathBuf> {
        on_path(EXECUTABLE).or_else(|| self.installed())
    }
}

/// OpenHands' own installer, where `bash` and `curl` are here; where not, why, in one line.
pub(crate) fn install() -> Result<Install, String> {
    if let Some(why) = lacks(&["bash", "curl"]) {
        return Err(format!("OpenHands' installer {why}"));
    }
    Ok(Install {
        argv: vec!["bash".into(), "-c".into(), INSTALLER.into()],
        about: format!(
            "OpenHands' own installer. It puts the openhands executable in /usr/local/bin where \
             that folder is writable, else in ~/.local/bin: {INSTALLER}"
        ),
    })
}
