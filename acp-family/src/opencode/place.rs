//! Where OpenCode keeps everything, and the `opencode` executable of its second line.

use std::path::{Path, PathBuf};

use steery_driver_common::program::executable;

/// OpenCode's folders, read once from the person's home and the environment.
#[derive(Clone, Debug)]
pub(crate) struct Place {
    pub home: PathBuf,
    /// Its database: `OPENCODE_DB` where that is absolute, else that name (or
    /// `opencode.db`) in `$XDG_DATA_HOME/opencode`, `~/.local/share/opencode` without it.
    pub db: PathBuf,
    /// Its global configuration folder: `OPENCODE_CONFIG_DIR`, else
    /// `$XDG_CONFIG_HOME/opencode`, `~/.config/opencode` without it.
    pub config: PathBuf,
}

impl Place {
    pub fn new(home: &Path) -> Self {
        let xdg = |name: &str, fallback: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
        };
        let data = xdg("XDG_DATA_HOME", ".local/share").join("opencode");
        let db = match std::env::var_os("OPENCODE_DB").filter(|v| !v.is_empty()) {
            Some(named) if Path::new(&named).is_absolute() => PathBuf::from(named),
            Some(named) => data.join(named),
            None => data.join("opencode.db"),
        };
        let config = std::env::var_os("OPENCODE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("opencode"));
        Self {
            home: home.to_owned(),
            db,
            config,
        }
    }

    /// The folder OpenCode's own installer writes its executable to.
    pub fn installer_folder(&self) -> PathBuf {
        self.home.join(".opencode/bin")
    }
}

/// Each `opencode` this machine has, in the order Steery prefers them: every one on `PATH`,
/// then the one in the installer's folder. brew and npm still ship OpenCode 1 as
/// `opencode`, so the first is not always the second line.
pub(crate) fn candidates(place: &Place) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|dir| executable(dir.join("opencode")))
        .collect();
    found.extend(executable(place.installer_folder().join("opencode")));
    let mut unique: Vec<PathBuf> = Vec::new();
    for path in found {
        let real = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !unique
            .iter()
            .any(|seen| seen.canonicalize().unwrap_or_else(|_| seen.clone()) == real)
        {
            unique.push(path);
        }
    }
    unique
}

/// A found `opencode`: the executable and the release it named.
pub(crate) struct Binary {
    pub path: PathBuf,
    pub release: Option<String>,
}

/// The first `opencode` whose `--version` names release 2 or newer; else, where some
/// `opencode` is here, the first of them, which is OpenCode 1 or named no release.
pub(crate) async fn binary(place: &Place) -> (Option<Binary>, Option<Binary>) {
    let mut older = None;
    for path in candidates(place) {
        let named = steery_driver_common::program::version(&path).await;
        let release = named
            .as_deref()
            .and_then(steery_driver_common::program::release_within);
        let found = Binary {
            path,
            release: release.map(|(a, b, c)| format!("{a}.{b}.{c}")),
        };
        if release.is_some_and(|(major, _, _)| major >= 2) {
            return (Some(found), older);
        }
        older.get_or_insert(found);
    }
    (None, older)
}

/// OpenCode's own installer for its second line, where `bash` and `curl` are here; where
/// not, why, in one line.
pub(crate) fn install() -> Result<steery_sdk::wire::harness::Install, String> {
    if let Some(why) = steery_driver_common::program::lacks(&["bash", "curl", "tar"]) {
        return Err(format!("OpenCode's installer {why}"));
    }
    let line = "curl -fsSL https://opencode.ai/v2/install | bash -s -- --no-modify-path";
    Ok(steery_sdk::wire::harness::Install {
        argv: vec!["bash".into(), "-c".into(), line.into()],
        about: format!(
            "OpenCode's own installer for OpenCode 2. It installs the release into \
             ~/.opencode/bin and leaves shell profiles alone: {line}"
        ),
    })
}
