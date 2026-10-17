//! Reads that said nothing new. The runtime reads every conversation and sub-agent on from
//! its cursor on every sweep, and most of them nobody wrote to since: a read that said
//! nothing says nothing again while every file its answer read stands as it did, and the
//! driver's latest whole listing still files the conversation where it was then. So it is
//! answered with one look at each of those files, neither finding the transcript nor
//! walking it. Which files a read stands on and where a conversation is filed are each
//! driver's to say; every `sessions/list` comes before the reads of its sweep, and says
//! which reads are still filed as they were ([`Quiet::keep`]).

use std::collections::HashMap;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

/// A file or folder as it stands: which one, how long, and when it last changed. A folder
/// changes when an entry is added to it, taken from it or renamed in it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    file: (u64, u64),
    length: u64,
    modified: SystemTime,
}

impl Stamp {
    /// The file `meta` describes, as it stood when it was read.
    pub fn of(meta: &Metadata) -> Option<Self> {
        Some(Self {
            file: (meta.dev(), meta.ino()),
            length: meta.len(),
            modified: meta.modified().ok()?,
        })
    }

    /// The file at `path` as it stands; `None` where it cannot be read.
    pub fn at(path: &Path) -> Option<Self> {
        Self::of(&std::fs::metadata(path).ok()?)
    }

    /// Which file it is: its device and inode.
    pub fn file(&self) -> (u64, u64) {
        self.file
    }

    pub fn length(&self) -> u64 {
        self.length
    }

    /// When the file last changed.
    pub fn modified(&self) -> SystemTime {
        self.modified
    }
}

/// By conversation and sub-agent: the read that said nothing — the files its answer stood
/// on and how each stood before it, the transcripts the conversation was filed at, the
/// cursor it went on from and handed back, and whether it read the conversation whole.
#[derive(Default)]
pub struct Quiet {
    kept: Mutex<HashMap<(String, Option<String>), Said>>,
}

struct Said {
    files: Vec<(PathBuf, Stamp)>,
    filed: Vec<PathBuf>,
    cursor: String,
    whole: bool,
}

impl Quiet {
    /// Whether the read of `external_id` (`sub_agent`'s) on from `cursor` said nothing when
    /// it was last made, and every file it stood on stands as it did then: that read's
    /// `whole`.
    pub fn again(&self, external_id: &str, sub_agent: Option<&str>, cursor: &str) -> Option<bool> {
        let kept = self.lock();
        let said = kept.get(&(external_id.to_owned(), sub_agent.map(str::to_owned)))?;
        (said.cursor == cursor
            && said
                .files
                .iter()
                .all(|(file, stamp)| Stamp::at(file) == Some(*stamp)))
        .then_some(said.whole)
    }

    /// The read of `external_id` (`sub_agent`'s) on from `cursor` said nothing and handed
    /// `cursor` back: answered again so while each of `files` stands as stamped, each
    /// stamped before the read, and the conversation stays filed at `filed`.
    pub fn said_nothing(
        &self,
        external_id: &str,
        sub_agent: Option<&str>,
        files: Vec<(PathBuf, Stamp)>,
        filed: Vec<PathBuf>,
        cursor: &str,
        whole: bool,
    ) {
        self.lock().insert(
            (external_id.to_owned(), sub_agent.map(str::to_owned)),
            Said {
                files,
                filed,
                cursor: cursor.to_owned(),
                whole,
            },
        );
    }

    /// Keep the reads whose conversation a whole listing still files where it was:
    /// `still(external_id, sub_agent, filed)` for each. What is no longer filed is let go.
    pub fn keep(&self, mut still: impl FnMut(&str, Option<&str>, &[PathBuf]) -> bool) {
        self.lock().retain(|(external_id, sub_agent), said| {
            still(external_id, sub_agent.as_deref(), &said.filed)
        });
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<(String, Option<String>), Said>> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
