//! Answers kept while what they were made from stands. The runtime asks of every
//! conversation and sub-agent on every sweep, and most of them nobody wrote to since: an
//! answer made from files is the same again while every file it read stands as it did,
//! and the driver's latest whole listing still files the conversation where it was then.
//! So it is given again after one look at each of those files ([`Memo`]); a read that
//! said nothing is one ([`Quiet`]). Which files an answer stands on and where a
//! conversation is filed are each driver's to say; every `sessions/list` comes before the
//! calls of its sweep, and says which answers are still filed as they were
//! ([`Memo::keep`]).

use std::borrow::Borrow;
use std::collections::HashMap;
use std::fs::Metadata;
use std::hash::Hash;
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

/// By `K`: an answer `V`, the files it was made from and how each stood before, and the
/// transcripts its conversation was filed at.
pub struct Memo<K, V> {
    kept: Mutex<HashMap<K, Kept<V>>>,
}

struct Kept<V> {
    files: Vec<(PathBuf, Stamp)>,
    filed: Vec<PathBuf>,
    value: V,
}

impl<K, V> Default for Memo<K, V> {
    fn default() -> Self {
        Self {
            kept: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Hash + Eq, V> Memo<K, V> {
    /// What `look` takes of the answer kept for `key`, while every file it was made from
    /// stands as it did then.
    pub fn get<Q, R>(&self, key: &Q, look: impl FnOnce(&V) -> Option<R>) -> Option<R>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let kept = self.lock();
        let kept = kept.get(key)?;
        let taken = look(&kept.value)?;
        kept.files
            .iter()
            .all(|(file, stamp)| Stamp::at(file) == Some(*stamp))
            .then_some(taken)
    }

    /// Keep `value` for `key`, made from `files`, each stamped before it was read, of a
    /// conversation filed at `filed`.
    pub fn put(&self, key: K, files: Vec<(PathBuf, Stamp)>, filed: Vec<PathBuf>, value: V) {
        self.lock().insert(
            key,
            Kept {
                files,
                filed,
                value,
            },
        );
    }

    /// Keep the answers whose conversation a whole listing still files where it was:
    /// `still(key, filed)` for each. What is no longer filed is let go.
    pub fn keep(&self, mut still: impl FnMut(&K, &[PathBuf]) -> bool) {
        self.lock().retain(|key, kept| still(key, &kept.filed));
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<K, Kept<V>>> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// By conversation and sub-agent: the read that said nothing — the cursor it went on from
/// and handed back, and whether it read the conversation whole.
#[derive(Default)]
pub struct Quiet {
    said: Memo<(String, Option<String>), (String, bool)>,
}

impl Quiet {
    /// Whether the read of `external_id` (`sub_agent`'s) on from `cursor` said nothing when
    /// it was last made, and every file it stood on stands as it did then: that read's
    /// `whole`.
    pub fn again(&self, external_id: &str, sub_agent: Option<&str>, cursor: &str) -> Option<bool> {
        self.said.get(
            &(external_id.to_owned(), sub_agent.map(str::to_owned)),
            |(said, whole)| (said == cursor).then_some(*whole),
        )
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
        self.said.put(
            (external_id.to_owned(), sub_agent.map(str::to_owned)),
            files,
            filed,
            (cursor.to_owned(), whole),
        );
    }

    /// Keep the reads whose conversation a whole listing still files where it was:
    /// `still(external_id, sub_agent, filed)` for each. What is no longer filed is let go.
    pub fn keep(&self, mut still: impl FnMut(&str, Option<&str>, &[PathBuf]) -> bool) {
        self.said.keep(|(external_id, sub_agent), filed| {
            still(external_id, sub_agent.as_deref(), filed)
        });
    }
}
