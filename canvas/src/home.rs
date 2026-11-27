//! Where the canvas harness and its sessions are on this host.
//!
//! The canvas keeps each session as `<CANVAS_HOME>/sessions/<id>.jsonl`, and the pid of the
//! turn that holds it as `<id>.pid` beside it while that turn runs. `CANVAS_HOME` defaults to
//! `~/Library/Application Support/steery-canvas` on macOS and
//! `${XDG_DATA_HOME:-~/.local/share}/steery-canvas` elsewhere.

use std::path::{Path, PathBuf};

use steery_driver_common::program::{executable, on_path};
use steery_sdk::{Error, refusal};

/// The variable that moves the canvas's folder.
const CANVAS_HOME: &str = "CANVAS_HOME";
/// The folder's name under the person's data folder.
const FOLDER: &str = "steery-canvas";
/// What ends a transcript's name, and a holder's.
pub const TRANSCRIPT: &str = "jsonl";
const PID: &str = "pid";
/// The canvas's launcher's name.
pub const EXECUTABLE: &str = "canvas";

/// The canvas's folder, for the person whose home is `home`.
pub fn root(home: &Path) -> PathBuf {
    if let Some(moved) = absolute(CANVAS_HOME) {
        return moved;
    }
    if cfg!(target_os = "macos") {
        return home.join("Library/Application Support").join(FOLDER);
    }
    absolute("XDG_DATA_HOME")
        .unwrap_or_else(|| home.join(".local/share"))
        .join(FOLDER)
}

/// The variable `name`, where it names an absolute path.
fn absolute(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os(name)?);
    path.is_absolute().then_some(path)
}

/// The folder of the sessions' transcripts.
pub fn sessions(root: &Path) -> PathBuf {
    root.join("sessions")
}

/// The session a transcript's file name names: its stem, where it ends `.jsonl`.
pub fn id_of(path: &Path) -> Option<String> {
    if path.extension()? != TRANSCRIPT {
        return None;
    }
    let id = path.file_stem()?.to_str()?;
    named(id).then(|| id.to_owned())
}

/// Whether `id` can name a file in the sessions' folder: a UUID's letters, digits and dashes.
fn named(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The transcript of the session `id`, where it is here.
pub fn transcript(root: &Path, id: &str) -> Result<PathBuf, Error> {
    named_file(root, id).filter(|path| path.is_file()).ok_or_else(|| not_found(id))
}

/// Where the transcript of the session `id` is, or will be once its turn writes it.
pub fn transcript_to_be(root: &Path, id: &str) -> Result<PathBuf, Error> {
    named_file(root, id).ok_or_else(|| not_found(id))
}

fn named_file(root: &Path, id: &str) -> Option<PathBuf> {
    named(id).then(|| sessions(root).join(format!("{id}.{TRANSCRIPT}")))
}

fn not_found(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("no canvas session here has the id {id}"),
    )
}

/// The file that holds the pid of the turn that holds the session `id`.
pub fn holder(root: &Path, id: &str) -> PathBuf {
    sessions(root).join(format!("{id}.{PID}"))
}

/// The folder the canvas's installer (`install.sh` of its release) puts `canvas` in.
pub fn installer_folder(home: &Path) -> PathBuf {
    home.join(".canvas/bin")
}

/// The canvas's launcher: the first `canvas` on `PATH`, else the one its installer put in
/// `~/.canvas/bin`.
pub fn binary(home: &Path) -> Option<PathBuf> {
    on_path(EXECUTABLE).or_else(|| executable(installer_folder(home).join(EXECUTABLE)))
}

/// [`binary`], else the refusal that the canvas is not installed here.
pub fn installed(home: &Path) -> Result<PathBuf, Error> {
    binary(home).ok_or_else(|| Error::refused(refusal::NO_BINARY, "canvas is not installed here"))
}

/// Claude Code, through which the canvas runs on a Claude plan: the first `claude` on
/// `PATH`, else the one its own installer puts in `~/.local/bin`.
pub fn claude(home: &Path) -> Option<PathBuf> {
    on_path("claude").or_else(|| executable(home.join(".local/bin/claude")))
}
