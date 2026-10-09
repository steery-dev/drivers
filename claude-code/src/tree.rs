//! A Claude Code transcript is a tree: every message, attachment and system record names
//! the record before it by its `uuid` (`parentUuid`, `null` at a root), and the
//! conversation is the path from the last such record back to a root. A rewind writes the
//! next record under an earlier one and leaves the other path in the file, read by nobody.
//! A compaction starts the chain again at its boundary record, which names the record it
//! follows as `logicalParentUuid`: the conversation goes on through it. Records that name
//! no `uuid` of their own — the mode, the queue, the title, the costs — are no part of it.
//! In a lead's transcript a sub-agent's records (`isSidechain`), which older releases
//! inlined there, are its own tree and none of the lead's.
//!
//! What is kept of a transcript between reads is its tree — where each record lies and
//! which record is its parent, never a word of a record — read on from where the last read
//! left the file. A leaf whose ancestry does not reach a beginning is an error, never a
//! shorter conversation.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead as _, Read as _, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Deserializer};

/// Why a walk stopped short of a beginning. Said, rather than handing back what had been
/// walked, which would read exactly like a whole short conversation.
const CARRIED_ON: &str = "that conversation carries on from one this machine does not hold, so only part of it can be read";

/// The fields of a record the tree is made of, and nothing of what was said in it.
#[derive(Deserialize)]
struct Head {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    /// Absent, `null` — a root — or the parent's uuid.
    #[serde(rename = "parentUuid", default, deserialize_with = "present")]
    parent: Option<Option<String>>,
    #[serde(rename = "logicalParentUuid", default)]
    logical: Option<String>,
    #[serde(rename = "isSidechain", default)]
    sidechain: Option<bool>,
}

/// A field that is there, `null` or not.
fn present<'de, D: Deserializer<'de>>(given: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(given).map(Some)
}

/// One record of the tree: where it lies, and its parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    pub start: u64,
    pub end: u64,
    parent: u32,
}

/// The parent of a record that has none: where a conversation begins.
const ROOT: u32 = u32::MAX;
/// The parent of a record whose parent is no record before it in the file.
const UNHELD: u32 = u32::MAX - 1;

/// The tree of one transcript. A parent is looked up among the records before it when a
/// record is added, so every parent comes before its child and no walk can go round in a
/// circle; a uuid written twice names the later record.
#[derive(Default)]
pub struct Tree {
    ids: HashMap<Box<str>, u32>,
    nodes: Vec<Node>,
}

impl Tree {
    /// One more record, at `start..end` (its newline not included). `own`: the transcript
    /// is a sub-agent's, whose sidechain records are its own. A message or system record
    /// that names no uuid follows the record before it.
    fn push(&mut self, head: &Head, start: u64, end: u64, own: bool) {
        if !own && head.sidechain == Some(true) {
            return;
        }
        let Ok(at) = u32::try_from(self.nodes.len()) else {
            return;
        };
        if at >= UNHELD {
            return;
        }
        let held = |id: &str| self.ids.get(id).copied();
        let parent = match head.uuid.as_deref().filter(|id| !id.is_empty()) {
            Some(_) => match (&head.parent, head.logical.as_deref()) {
                (Some(Some(parent)), _) => held(parent).unwrap_or(UNHELD),
                (_, Some(logical)) => held(logical).unwrap_or(ROOT),
                _ => ROOT,
            },
            None if matches!(head.kind.as_deref(), Some("user" | "assistant" | "system")) => {
                at.checked_sub(1).unwrap_or(ROOT)
            }
            None => return,
        };
        if let Some(id) = head.uuid.as_deref().filter(|id| !id.is_empty()) {
            self.ids.insert(id.into(), at);
        }
        self.nodes.push(Node { start, end, parent });
    }

    /// The records from a root to the leaf, the last record, which is where the
    /// conversation was when it was last written to; an error where the leaf's ancestry
    /// stops at a parent the file does not hold.
    pub fn path(&self) -> Result<Vec<u32>, String> {
        let mut path = Vec::new();
        let mut at = self.nodes.len().checked_sub(1).map(|at| at as u32);
        while let Some(step) = at {
            path.push(step);
            at = match self.nodes[step as usize].parent {
                ROOT => None,
                UNHELD => return Err(CARRIED_ON.into()),
                parent => Some(parent),
            };
        }
        path.reverse();
        Ok(path)
    }

    pub fn node(&self, at: u32) -> Node {
        self.nodes[at as usize]
    }

    /// Every record of it, in the order the file holds them.
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
}

/// One transcript's tree, as far as the file has been read.
#[derive(Default)]
struct Index {
    /// Its device and inode: a transcript replaced under the same name is another file.
    file: (u64, u64),
    /// How long the file was when it was last read.
    len: u64,
    /// Just past the newline of the last record indexed.
    through: u64,
    /// The last bytes indexed, up to [`SEAL`] of them. Claude Code only appends; bytes
    /// there that changed are a file written whole again, read again from its start.
    seal: Vec<u8>,
    tree: Tree,
    /// When this tree was last walked, by the clock of [`Trees`].
    used: u64,
}

/// How much of what was indexed last is read again to know the file was only appended to.
const SEAL: u64 = 64;

impl Index {
    /// Read what was appended since the last read, up to its last newline. A file
    /// replaced, shortened or written whole again is read again from its start.
    fn update(&mut self, open: &mut File, file: (u64, u64), len: u64, own: bool) -> io::Result<()> {
        let rewritten = self.file != file
            || len < self.len
            || bytes(open, self.through - self.seal.len() as u64, self.through)? != self.seal;
        if rewritten {
            *self = Index {
                file,
                used: self.used,
                ..Index::default()
            };
        }
        if len != self.len {
            self.len = len;
            self.read_on(open, len, own)?;
            self.seal = bytes(open, self.through.saturating_sub(SEAL), self.through)?;
        }
        Ok(())
    }

    fn read_on(&mut self, open: &mut File, len: u64, own: bool) -> io::Result<()> {
        open.seek(SeekFrom::Start(self.through))?;
        let mut lines = io::BufReader::new((&*open).take(len - self.through));
        let mut line = Vec::new();
        loop {
            line.clear();
            if lines.read_until(b'\n', &mut line)? == 0 || line.last() != Some(&b'\n') {
                return Ok(());
            }
            let start = self.through;
            self.through += line.len() as u64;
            // A line that is no JSON object says nothing.
            if let Ok(head) = serde_json::from_slice::<Head>(&line[..line.len() - 1]) {
                self.tree.push(&head, start, self.through - 1, own);
            }
        }
    }
}

/// How many records the trees kept may hold between them; past it, the tree walked
/// longest ago is dropped first. A record is under a hundred bytes: about 45 MiB at the
/// bound.
const ENTRIES: usize = 1 << 19;

/// Every tree kept, by transcript, with a clock that says which was walked last.
#[derive(Default)]
pub struct Trees {
    kept: Mutex<(u64, HashMap<PathBuf, Index>)>,
}

/// A transcript's tree as it stands now, and the file open to read its records from.
pub struct Walk<'a> {
    pub tree: &'a Tree,
    pub open: &'a mut File,
    /// The file's device and inode.
    pub file: (u64, u64),
    /// How long the file is.
    pub len: u64,
}

impl Trees {
    /// Read `file`'s tree on to its end, and hand it to `read`. `own`: the transcript is a
    /// sub-agent's.
    pub fn walk<T>(
        &self,
        file: &Path,
        own: bool,
        read: impl FnOnce(Walk<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        let failed = |e: io::Error| format!("{}: {e}", file.display());
        let mut open = File::open(file).map_err(failed)?;
        let meta = open.metadata().map_err(failed)?;
        let identity = (meta.dev(), meta.ino());
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        let (clock, trees) = &mut *kept;
        *clock += 1;
        let index = trees.entry(file.to_owned()).or_default();
        index.used = *clock;
        index
            .update(&mut open, identity, meta.len(), own)
            .map_err(failed)?;
        let result = read(Walk {
            tree: &index.tree,
            open: &mut open,
            file: identity,
            len: index.len,
        });
        shed(trees, file);
        result
    }
}

/// Drop the trees walked longest ago until those left hold at most [`ENTRIES`] records;
/// the one just walked stays whatever it holds.
fn shed(trees: &mut HashMap<PathBuf, Index>, walked: &Path) {
    let mut held: usize = trees.values().map(|index| index.tree.nodes.len() + 1).sum();
    while held > ENTRIES {
        let Some(oldest) = trees
            .iter()
            .filter(|(path, _)| path.as_path() != walked)
            .min_by_key(|(_, index)| index.used)
            .map(|(path, _)| path.clone())
        else {
            return;
        };
        if let Some(index) = trees.remove(&oldest) {
            held -= index.tree.nodes.len() + 1;
        }
    }
}

/// The bytes of the file from `start` to `end`.
pub fn bytes(open: &mut File, start: u64, end: u64) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; usize::try_from(end - start).map_err(io::Error::other)?];
    open.seek(SeekFrom::Start(start))?;
    open.read_exact(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn file(text: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "steery-cc-tree-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A Claude Code record, carrying its place in the test as `n`.
    fn record(uuid: &str, parent: Option<&str>, n: i64) -> String {
        format!(
            "{}\n",
            json!({"type": "user", "uuid": uuid, "parentUuid": parent, "n": n,
                "message": {"role": "user", "content": "words"}})
        )
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    /// The active path, leaf first, as each record's `n`.
    fn walked(trees: &Trees, path: &Path) -> Result<Vec<i64>, String> {
        trees.walk(path, false, |walk| {
            let mut seen = Vec::new();
            for at in walk.tree.path()?.into_iter().rev() {
                let node = walk.tree.node(at);
                let record: Value =
                    serde_json::from_slice(&bytes(walk.open, node.start, node.end).unwrap())
                        .unwrap();
                seen.push(record["n"].as_i64().unwrap_or(-1));
            }
            Ok(seen)
        })
    }

    /// A tree is read on as it grows, from whichever record was written last: a rewind
    /// moves the leaf and leaves the other path in the file, half a line is no record yet,
    /// a record naming no uuid of its own is no part of it, and a transcript written whole
    /// again under the same name is read again from its start.
    #[test]
    fn a_tree_is_read_on_as_it_grows_and_walked_from_its_leaf() {
        let trees = Trees::default();
        let path = file(&format!(
            "{{\"type\":\"queue-operation\",\"operation\":\"enqueue\"}}\n{}{}",
            record("a", None, 1),
            record("b", Some("a"), 2)
        ));
        assert_eq!(walked(&trees, &path).unwrap(), [2, 1]);
        append(&path, &record("c", Some("a"), 3));
        let half = record("d", Some("c"), 4);
        append(&path, &half[..half.len() / 2]);
        assert_eq!(
            walked(&trees, &path).unwrap(),
            [3, 1],
            "the other branch, and half a line is none"
        );
        append(&path, &half[half.len() / 2..]);
        append(
            &path,
            "not json\n{\"type\":\"last-prompt\",\"leafUuid\":\"d\"}\n",
        );
        assert_eq!(walked(&trees, &path).unwrap(), [4, 3, 1]);
        std::fs::write(
            &path,
            format!("{}{}", record("x", None, 8), record("y", Some("x"), 9)),
        )
        .unwrap();
        assert_eq!(
            walked(&trees, &path).unwrap(),
            [9, 8],
            "written whole again"
        );
    }

    /// A compaction's boundary starts the chain again and names the record it follows as
    /// its logical parent, through which the conversation goes on; a sub-agent's records
    /// inlined into its lead's transcript are none of the lead's; records that name no uuid
    /// follow the one before them.
    #[test]
    fn a_compaction_goes_on_and_a_sidechain_is_aside() {
        let trees = Trees::default();
        let compacted = file(&format!(
            "{}{}{}{}",
            record("a", None, 1),
            record("b", Some("a"), 2),
            format_args!(
                "{}\n",
                json!({"type": "system", "subtype": "compact_boundary", "uuid": "c",
                    "parentUuid": null, "logicalParentUuid": "b", "n": 3})
            ),
            record("d", Some("c"), 4)
        ));
        assert_eq!(walked(&trees, &compacted).unwrap(), [4, 3, 2, 1]);
        let inlined = file(&format!(
            "{}{}{}",
            record("a", None, 1),
            format_args!(
                "{}\n",
                json!({"type": "user", "uuid": "s", "parentUuid": null, "isSidechain": true, "n": 2})
            ),
            record("b", Some("a"), 3),
        ));
        assert_eq!(walked(&trees, &inlined).unwrap(), [3, 1]);
        let lined = file(
            "{\"type\":\"user\",\"n\":1}\n{\"type\":\"custom-title\",\"customTitle\":\"t\"}\n{\"type\":\"assistant\",\"n\":2}\n",
        );
        assert_eq!(walked(&trees, &lined).unwrap(), [2, 1]);
    }

    /// A walk that cannot reach a beginning says so: a parent the file does not hold, or a
    /// parent written after its child, which is how records naming each other in a circle
    /// read (the runtime's `a_chain_that_does_not_reach_a_beginning_is_refused`).
    #[test]
    fn a_chain_that_does_not_reach_a_beginning_is_refused() {
        let trees = Trees::default();
        let elsewhere = file(&record("b", Some("somewhere-else"), 1));
        assert!(walked(&trees, &elsewhere).is_err());
        let circle = file(&format!(
            "{}{}",
            record("a", Some("b"), 1),
            record("b", Some("a"), 2)
        ));
        assert!(walked(&trees, &circle).is_err());
        let aside = file(&format!(
            "{}{}{}",
            record("a", None, 1),
            record("b", Some("z"), 2),
            record("c", Some("a"), 3)
        ));
        assert_eq!(
            walked(&trees, &aside).unwrap(),
            [3, 1],
            "a hole off the path is not the leaf's"
        );
    }
}
