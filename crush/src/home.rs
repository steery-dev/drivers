//! Where Crush keeps what it keeps, and the `crush` binary. Crush 0.100.0 reads these paths
//! in `internal/config/load.go` and `internal/projects/projects.go`: one configuration
//! folder, one data folder that lists the projects, and each project's own store in its
//! `.crush` folder.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use steery_driver_common::program::{executable, on_path};

/// A variable that holds a path, where it is set and absolute.
fn var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

/// `$XDG_CONFIG_HOME`, else `~/.config`: Crush's `home.Config()`.
fn config_home(home: &Path) -> PathBuf {
    var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"))
}

/// The folder of Crush's own configuration file: `$CRUSH_GLOBAL_CONFIG`, else
/// `<config home>/crush`.
pub(crate) fn config(home: &Path) -> PathBuf {
    var("CRUSH_GLOBAL_CONFIG").unwrap_or_else(|| config_home(home).join("crush"))
}

/// The person's `crush.json`, where Steery's entries go.
pub(crate) fn settings(home: &Path) -> PathBuf {
    config(home).join("crush.json")
}

/// The folder Crush reads skills from first: `$CRUSH_SKILLS_DIR`, else
/// `<config home>/crush/skills`. `CRUSH_GLOBAL_CONFIG` does not move it.
pub(crate) fn skills(home: &Path) -> PathBuf {
    var("CRUSH_SKILLS_DIR").unwrap_or_else(|| config_home(home).join("crush/skills"))
}

/// Crush's data folder: `$CRUSH_GLOBAL_DATA`, else `$XDG_DATA_HOME/crush`, else
/// `~/.local/share/crush`. It holds `projects.json` and a `crush.json` that Crush writes
/// itself.
pub(crate) fn data(home: &Path) -> PathBuf {
    var("CRUSH_GLOBAL_DATA")
        .or_else(|| var("XDG_DATA_HOME").map(|dir| dir.join("crush")))
        .unwrap_or_else(|| home.join(".local/share/crush"))
}

/// One project Crush has run in: its folder, and the folder of its store.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Project {
    pub path: PathBuf,
    pub data_dir: PathBuf,
}

impl Project {
    /// The project's store, `crush.db`.
    pub fn db(&self) -> PathBuf {
        self.data_dir.join("crush.db")
    }
}

#[derive(Deserialize)]
struct Projects {
    #[serde(default)]
    projects: Vec<Project>,
}

/// The file that lists the projects.
pub(crate) fn projects_file(home: &Path) -> PathBuf {
    data(home).join("projects.json")
}

/// Every project `projects.json` lists, then every project the person's Crush server has
/// open ([`crate::door::open_projects`]), each once by its store, with a store of absolute
/// path; none where Crush never ran here.
pub(crate) fn projects(home: &Path) -> Vec<Project> {
    let listed = steery_driver_common::mcp::json(&projects_file(home))
        .and_then(|value| serde_json::from_value::<Projects>(value).ok())
        .map(|listed| listed.projects)
        .unwrap_or_default();
    let open = crate::door::open_projects(home)
        .into_iter()
        .map(|(path, data_dir)| Project { path, data_dir });
    let mut projects: Vec<Project> = Vec::new();
    let mut stores: Vec<PathBuf> = Vec::new();
    for project in listed.into_iter().chain(open) {
        if !project.path.is_absolute() || !project.data_dir.is_absolute() {
            continue;
        }
        // The server names a project by its real path (`/private/tmp`), `projects.json` by
        // the one it was opened at (`/tmp`).
        let store = std::fs::canonicalize(&project.data_dir).unwrap_or(project.data_dir.clone());
        if !stores.contains(&store) {
            stores.push(store);
            projects.push(project);
        }
    }
    projects
}

/// The folders where Homebrew puts what it installs.
const BREW: [&str; 3] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/home/linuxbrew/.linuxbrew/bin",
];

/// Crush's executable: the first `crush` on `PATH`, else Homebrew's.
pub(crate) fn binary() -> Option<PathBuf> {
    on_path("crush").or_else(|| {
        BREW.iter()
            .find_map(|dir| executable(Path::new(dir).join("crush")))
    })
}

/// A line for a person where `crush` is installed in more than one place here.
pub(crate) fn others_note(used: &Path) -> Option<String> {
    let brew = BREW
        .iter()
        .map(Path::new)
        .find(|dir| dir.join("crush").is_file());
    steery_driver_common::program::others_note("Crush", "crush", brew, used)
}

/// Crush's own installer: Homebrew's tap where `brew` is here, else npm's package. Where
/// neither, why, in one line.
pub(crate) fn install() -> Result<steery_sdk::wire::harness::Install, String> {
    let (argv, about): (&[&str], _) = if on_path("brew").is_some() {
        (
            &["brew", "install", "charmbracelet/tap/crush"],
            "Crush from Charm's Homebrew tap: brew install charmbracelet/tap/crush",
        )
    } else if on_path("npm").is_some() {
        (
            &["npm", "install", "--global", "@charmland/crush"],
            "Crush's npm package: npm install --global @charmland/crush",
        )
    } else {
        return Err("Crush's installers need Homebrew (`brew`) or npm on PATH".into());
    };
    Ok(steery_sdk::wire::harness::Install {
        argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
        about: about.into(),
    })
}
