//! Reads that said nothing new. The runtime reads every conversation and sub-agent on from
//! its cursor on every sweep, and most of them nobody wrote to since: the same read of a
//! transcript that still stands as it did says nothing again, so it is answered with one
//! look at the file, neither finding it among every folder nor walking it.
//! Measured 2026-10-17 on a Mac with 411 omp conversations holding 600,000 records, past
//! the trees' bound (`steery_driver_common::tree`): every sweep walked each again from its
//! start, and a sweep's 938 reads took 5.5 s; answered unwalked, 1.8 s.
//!
//! The transcript a read found stands for its conversation while it stands as it did: a
//! second transcript filed under the same id later is seen once the first one changes.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

/// A file as it stands: which file, how long, and when it last changed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    file: (u64, u64),
    length: u64,
    modified: SystemTime,
}

impl Stamp {
    /// The file at `path` as it stands; `None` where it cannot be read.
    pub fn at(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            file: (meta.dev(), meta.ino()),
            length: meta.len(),
            modified: meta.modified().ok()?,
        })
    }

    /// When the file last changed.
    pub fn modified(&self) -> SystemTime {
        self.modified
    }
}

/// By conversation and sub-agent: the read that said nothing — the transcript it read and
/// how it stood before, the cursor it went on from and handed back, and whether it read
/// the conversation whole.
#[derive(Default)]
pub struct Quiet {
    kept: Mutex<HashMap<(String, Option<String>), Said>>,
}

struct Said {
    file: PathBuf,
    stamp: Stamp,
    cursor: String,
    whole: bool,
}

impl Quiet {
    /// Whether the read of `external_id` (`sub_agent`'s) on from `cursor` said nothing when
    /// it was last made, and its transcript stands as it did then: that read's `whole`.
    pub fn again(&self, external_id: &str, sub_agent: Option<&str>, cursor: &str) -> Option<bool> {
        let kept = self.lock();
        let said = kept.get(&(external_id.to_owned(), sub_agent.map(str::to_owned)))?;
        (said.cursor == cursor && Stamp::at(&said.file) == Some(said.stamp)).then_some(said.whole)
    }

    /// The read of `external_id` (`sub_agent`'s) on from `cursor` said nothing and handed
    /// `cursor` back: answered again so while `file` stands as `stamp`, taken before the
    /// read.
    pub fn said_nothing(
        &self,
        external_id: &str,
        sub_agent: Option<&str>,
        file: &Path,
        stamp: Stamp,
        cursor: &str,
        whole: bool,
    ) {
        self.lock().insert(
            (external_id.to_owned(), sub_agent.map(str::to_owned)),
            Said {
                file: file.to_owned(),
                stamp,
                cursor: cursor.to_owned(),
                whole,
            },
        );
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<(String, Option<String>), Said>> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
