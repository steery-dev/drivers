//! Reads that said nothing new. The runtime reads every conversation on from its cursor
//! on every sweep, and most of them nobody wrote to since: the same read of a transcript
//! that still stands as it did says nothing again, so it is answered without walking it.
//! Measured 2026-10-17 on a Mac with 411 omp conversations holding 600,000 records, past
//! the trees' bound (`steery_driver_common::tree`): every sweep walked each again from its
//! start, and a sweep's 938 reads took 5.5 s; answered unwalked, 1.5 s.

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

/// By transcript: the read that said nothing — the cursor it went on from and handed
/// back, and whether it read the conversation whole — and the file as it stood before it.
#[derive(Default)]
pub struct Quiet {
    kept: Mutex<HashMap<PathBuf, Said>>,
}

struct Said {
    stamp: Stamp,
    cursor: String,
    whole: bool,
}

impl Quiet {
    /// Whether the read of `file` on from `cursor` said nothing when it was last made and
    /// the file, `stamp` now, stands as it did then: that read's `whole`.
    pub fn again(&self, file: &Path, stamp: Stamp, cursor: &str) -> Option<bool> {
        self.lock()
            .get(file)
            .filter(|said| said.stamp == stamp && said.cursor == cursor)
            .map(|said| said.whole)
    }

    /// The read of `file` on from `cursor` said nothing and handed `cursor` back: answered
    /// again so while the file stands as `stamp`, taken before the read.
    pub fn said_nothing(&self, file: &Path, stamp: Stamp, cursor: &str, whole: bool) {
        self.lock().insert(
            file.to_owned(),
            Said {
                stamp,
                cursor: cursor.to_owned(),
                whole,
            },
        );
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<PathBuf, Said>> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
