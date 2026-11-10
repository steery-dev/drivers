//! Where Cline keeps everything, and the `cline` executable.

use std::path::{Path, PathBuf};

/// Cline's folders, read once from the person's home and the environment.
#[derive(Clone, Debug)]
pub(crate) struct Place {
    /// Cline's own folder: `CLINE_DIR`, else `~/.cline`.
    pub dir: PathBuf,
}

impl Place {
    pub fn new(home: &Path) -> Self {
        let dir = std::env::var_os("CLINE_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cline"));
        Self { dir }
    }

    /// The SQLite file whose `sessions` table lists every conversation.
    pub fn db(&self) -> PathBuf {
        self.dir.join("data/db/sessions.db")
    }

    /// Cline's settings folder.
    pub fn settings(&self) -> PathBuf {
        self.dir.join("data/settings")
    }

    /// The file of the MCP servers Cline loads: `CLINE_MCP_SETTINGS_PATH`, else the settings
    /// folder's `cline_mcp_settings.json`.
    pub fn mcp(&self) -> PathBuf {
        std::env::var_os("CLINE_MCP_SETTINGS_PATH")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.settings().join("cline_mcp_settings.json"))
    }

    /// The file of Cline's providers and their keys, which say whether it is signed in.
    pub fn providers(&self) -> PathBuf {
        self.settings().join("providers.json")
    }
}

/// The `cline` on `PATH`.
pub(crate) fn binary() -> Option<PathBuf> {
    steery_driver_common::program::on_path("cline")
}

/// Cline's own installer, npm, where npm is here; where not, why, in one line.
pub(crate) fn install() -> Result<steery_sdk::wire::harness::Install, String> {
    if let Some(why) = steery_driver_common::program::lacks(&["npm"]) {
        return Err(format!("Cline's installer {why}"));
    }
    Ok(steery_sdk::wire::harness::Install {
        argv: vec![
            "npm".into(),
            "install".into(),
            "--global".into(),
            "cline".into(),
        ],
        about: "npm installs Cline's CLI globally: npm install --global cline".into(),
    })
}
