//! Where Goose keeps everything, and the `goose` executable.

use std::path::{Path, PathBuf};

use steery_driver_common::program::{executable, lacks, on_path};
use steery_sdk::wire::harness::Install;

/// Goose's installer, which pipes into `bash` with `CONFIGURE=false`: no `goose configure`
/// prompt runs.
const INSTALLER: &str = "curl -fsSL \
    https://github.com/aaif-goose/goose/releases/download/stable/download_cli.sh \
    | CONFIGURE=false bash";

/// Goose's folders, read once from the person's home and the environment, as Goose's own
/// `Paths` reads them (`crates/goose/src/config/paths.rs`).
#[derive(Clone, Debug)]
pub(crate) struct Place {
    /// The person's home.
    pub home: PathBuf,
    /// Goose's configuration folder: `<GOOSE_PATH_ROOT>/config`, else
    /// `<XDG_CONFIG_HOME or ~/.config>/goose`.
    pub config: PathBuf,
    /// Goose's data folder: `<GOOSE_PATH_ROOT>/data`, else
    /// `<XDG_DATA_HOME or ~/.local/share>/goose`.
    pub data: PathBuf,
}

impl Place {
    pub fn new(home: &Path) -> Self {
        // Goose and its XDG strategy both take only an absolute path from the environment.
        let set = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let (config, data) = match set("GOOSE_PATH_ROOT") {
            Some(root) => (root.join("config"), root.join("data")),
            None => (
                set("XDG_CONFIG_HOME")
                    .unwrap_or_else(|| home.join(".config"))
                    .join("goose"),
                set("XDG_DATA_HOME")
                    .unwrap_or_else(|| home.join(".local/share"))
                    .join("goose"),
            ),
        };
        Self {
            home: home.to_owned(),
            config,
            data,
        }
    }

    /// The SQLite file that holds every conversation and its messages.
    pub fn db(&self) -> PathBuf {
        self.data.join("sessions/sessions.db")
    }

    /// The person's configuration, which names the extensions Goose loads.
    pub fn config_yaml(&self) -> PathBuf {
        self.config.join("config.yaml")
    }

    /// Every configuration file Goose reads, in its order: the system's, those that
    /// `GOOSE_ADDITIONAL_CONFIG_FILES` names, then the person's. A later file's entry of an
    /// extension adds to and overrides an earlier one's (`config/base.rs:200-207,320-372`).
    pub fn config_files(&self) -> Vec<PathBuf> {
        let mut files = vec![PathBuf::from("/etc/goose/config.yaml")];
        if let Some(more) = std::env::var_os("GOOSE_ADDITIONAL_CONFIG_FILES") {
            files.extend(std::env::split_paths(&more));
        }
        files.push(self.config_yaml());
        files
    }

    /// Where Goose's installer puts the command: `GOOSE_BIN_DIR`, else `~/.local/bin`.
    pub fn installed(&self) -> Option<PathBuf> {
        let folder = std::env::var_os("GOOSE_BIN_DIR")
            .filter(|folder| !folder.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| self.home.join(".local/bin"));
        executable(folder.join("goose"))
    }

    /// The `goose` on `PATH`, else the one in the installer's folder.
    pub fn binary(&self) -> Option<PathBuf> {
        on_path("goose").or_else(|| self.installed())
    }
}

/// Goose's own installer, where `bash`, `curl` and `tar` are here; where not, why, in one
/// line.
pub(crate) fn install() -> Result<Install, String> {
    if let Some(why) = lacks(&["bash", "curl", "tar"]) {
        return Err(format!("Goose's installer {why}"));
    }
    Ok(Install {
        argv: vec!["bash".into(), "-c".into(), INSTALLER.into()],
        about: "Goose's own installer. It puts the command in GOOSE_BIN_DIR, else \
                ~/.local/bin, and does not run `goose configure`"
            .into(),
    })
}
