//! Where Aider and its conversations are on this host.
//!
//! Aider keeps no index of its conversations: each is a chat history file it appends to,
//! `.aider.chat.history.md` in the root of the git repository it ran in (`args.py`,
//! `default_chat_history_file`), or the file `--chat-history-file` named. Spotlight does not
//! index these dotfiles, so the driver walks the home folder for them: to [`DEPTH`] folders
//! down, past hidden folders, the person's library and build output, and never below a
//! repository's root, where Aider puts the file. The conversation's id is a hash of the
//! file's path, since the file names nothing of its own.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use steery_driver_common::program::{executable, on_path};

/// What Aider names its chat history file, and the start and end of the name of one this
/// driver starts beside it (`.aider.chat.history.<ms>.md`).
pub const NAME: &str = ".aider.chat.history.md";
const STEM: &str = ".aider.chat.history.";
const EXTENSION: &str = ".md";

/// How many folders below home the walk reads.
const DEPTH: usize = 6;

/// Folders the walk never enters: what a person keeps apart from their code, and what a
/// build writes.
const SKIPPED: [&str; 13] = [
    "Library",
    "Applications",
    "Movies",
    "Music",
    "Pictures",
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "venv",
    "__pycache__",
    "Pods",
];

/// Whether a file name is one of Aider's chat histories.
pub fn is_history(name: &str) -> bool {
    name == NAME
        || (name.starts_with(STEM)
            && name.ends_with(EXTENSION)
            && name.len() > STEM.len() + EXTENSION.len())
}

/// The conversation's id: the first 32 hex digits of the SHA-256 of its file's path.
pub fn id(path: &Path) -> String {
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Every chat history file in the repositories below `home`.
pub fn walk(home: &Path) -> BTreeSet<PathBuf> {
    let mut found = BTreeSet::new();
    let mut stack = vec![(home.to_owned(), 0)];
    while let Some((folder, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        let entries: Vec<_> = entries.flatten().collect();
        let root = entries.iter().any(|entry| entry.file_name() == ".git");
        for entry in &entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_file() && is_history(name) {
                found.insert(entry.path());
            } else if kind.is_dir()
                && !root
                && depth < DEPTH
                && !name.starts_with('.')
                && !SKIPPED.contains(&name)
            {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    found
}

/// The repository root that holds `folder`, else `folder` itself: where Aider puts the
/// history of a run in `folder`.
pub fn root(folder: &Path) -> PathBuf {
    folder
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(folder)
        .to_owned()
}

/// Aider's executable: the first `aider` on `PATH`, else the one `uv tool install` and
/// `pipx` put in `~/.local/bin`.
pub fn binary(home: &Path) -> Option<PathBuf> {
    on_path("aider").or_else(|| executable(home.join(".local/bin/aider")))
}

/// Aider's own installer: `uv tool` where `uv` is here, else `pipx`. Where neither, why.
pub fn install() -> Result<steery_sdk::wire::harness::Install, String> {
    let (argv, about): (&[&str], _) = if on_path("uv").is_some() {
        (
            &[
                "uv",
                "tool",
                "install",
                "--python",
                "python3.12",
                "aider-chat@latest",
            ],
            "Aider as a uv tool: uv tool install --python python3.12 aider-chat@latest",
        )
    } else if on_path("pipx").is_some() {
        (
            &["pipx", "install", "aider-chat"],
            "Aider with pipx: pipx install aider-chat",
        )
    } else {
        return Err("Aider installs with uv or pipx, and neither is here".into());
    };
    Ok(steery_sdk::wire::harness::Install {
        argv: argv.iter().map(|&word| word.to_owned()).collect(),
        about: about.into(),
    })
}

/// Ollama's executable and the folder of its models' manifests, which changes as a model
/// is pulled or removed.
pub fn ollama() -> Option<PathBuf> {
    on_path("ollama").or_else(|| {
        ["/opt/homebrew/bin", "/usr/local/bin"]
            .iter()
            .find_map(|dir| executable(Path::new(dir).join("ollama")))
    })
}

pub fn manifests(home: &Path) -> PathBuf {
    home.join(".ollama/models/manifests")
}
