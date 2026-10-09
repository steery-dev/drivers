//! A pi or omp transcript is a tree: after its `session` header every entry names its
//! parent by id (`parentId`, `null` at a root), and the conversation is the path from the
//! last entry back to a root. A branch moves the leaf and leaves the other path in the
//! file, read by nobody. Only a first-version header, which could leave its version out,
//! makes the entries a line, each the parent of the next.
//!
//! What is kept of a transcript between reads is its tree — where each entry's record
//! lies and which entry is its parent, never a word of a record — read on from where the
//! last read left the file. A transcript that cannot be read as a tree, or whose leaf's
//! ancestry does not reach a beginning, is an error and never a shorter conversation.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead as _, Read as _, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Deserializer};

/// Why a walk stopped short of a beginning. Said, rather than handing back what had been
/// walked, which would read exactly like a whole short conversation.
const CARRIED_ON: &str = "that conversation carries on from one this machine does not hold, so only part of it can be read";

/// The fields of a record the tree is made of, and nothing of what was said in it.
#[derive(Deserialize)]
struct Head {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    /// Absent, `null` — a root — or the parent's id: three different things.
    #[serde(rename = "parentId", default, deserialize_with = "present")]
    parent: Option<Option<String>>,
    #[serde(default)]
    version: Option<u64>,
}

/// A field that is there, `null` or not.
fn present<'de, D: Deserializer<'de>>(given: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(given).map(Some)
}

/// One entry: where its record lies, and its parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    pub start: u64,
    pub end: u64,
    parent: u32,
}

/// The parent of an entry that has none: where a conversation begins.
const ROOT: u32 = u32::MAX;
/// The parent of an entry whose parent is no entry before it in the file.
const UNHELD: u32 = u32::MAX - 1;

/// The tree of one transcript. A parent is looked up among the entries before it when an
/// entry is added, so every parent comes before its child and no walk can go round in a
/// circle. pi and omp append a child to a leaf already written, so a parent named later in
/// the file is a transcript this does not read.
#[derive(Default)]
pub struct Tree {
    /// The header's `version` once the header is read; 1 where it names none.
    version: Option<u64>,
    ids: HashMap<Box<str>, u32>,
    nodes: Vec<Node>,
}

impl Tree {
    /// One more record, at `start..end` (its newline not included). omp's `title` slot,
    /// rewritten in place above the header, is no entry. Every version past the first
    /// names each entry's parent, and an entry naming none, or one id given twice, is a
    /// transcript that cannot be read as a tree.
    fn push(&mut self, head: &Head, start: u64, end: u64) -> Result<(), String> {
        match head.kind.as_str() {
            "title" => return Ok(()),
            "session" => {
                if self.version.is_some() {
                    return Err("that transcript has more than one session header".into());
                }
                self.version = Some(Some(head.version.unwrap_or(1)).filter(|v| *v > 0).ok_or(
                    "that transcript's session header names no version there is".to_owned(),
                )?);
                return Ok(());
            }
            _ => {}
        }
        let version = self
            .version
            .ok_or("that transcript has no session header")?;
        let at = u32::try_from(self.nodes.len())
            .ok()
            .filter(|at| *at < UNHELD)
            .ok_or("that transcript holds more entries than can be read")?;
        let parent = if version == 1 {
            at.checked_sub(1).unwrap_or(ROOT)
        } else {
            let id = head
                .id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or("that transcript has an entry with no id")?;
            let parent = match &head.parent {
                Some(None) => ROOT,
                Some(Some(parent)) if !parent.is_empty() => {
                    self.ids.get(parent.as_str()).copied().unwrap_or(UNHELD)
                }
                _ => return Err("that transcript has an entry that names no parent".into()),
            };
            if self.ids.insert(id.into(), at).is_some() {
                return Err("that transcript gives two entries one id".into());
            }
            parent
        };
        self.nodes.push(Node { start, end, parent });
        Ok(())
    }

    /// The entries from a root to the leaf, the last entry, which is where the
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
    /// The last bytes indexed, up to [`SEAL`] of them. pi and omp only append; bytes there
    /// that changed are a file written whole again — pi migrating it to a newer version —
    /// and read again from its start.
    seal: Vec<u8>,
    tree: Tree,
    /// Why the file cannot be read as a tree, once it cannot: kept until it is replaced.
    broken: Option<String>,
    /// When this tree was last walked, by the clock of [`Trees`].
    used: u64,
}

/// How much of what was indexed last is read again to know the file was only appended to.
const SEAL: u64 = 64;

impl Index {
    /// Read what was appended since the last read, up to its last newline. A file
    /// replaced, shortened or written whole again is read again from its start.
    fn update(&mut self, open: &mut File, file: (u64, u64), len: u64) -> io::Result<()> {
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
        if self.broken.is_none() && len != self.len {
            self.len = len;
            self.read_on(open, len)?;
            self.seal = bytes(open, self.through.saturating_sub(SEAL), self.through)?;
        }
        Ok(())
    }

    fn read_on(&mut self, open: &mut File, len: u64) -> io::Result<()> {
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
            let head = match serde_json::from_slice::<Head>(&line[..line.len() - 1]) {
                Ok(head) => head,
                // A line that is not JSON says nothing, as pi reads it; one that is JSON
                // and no entry is a transcript that cannot be read as a tree.
                Err(error) if !error.is_data() => continue,
                Err(_) => {
                    self.broke("that transcript has a record that is no entry".to_owned());
                    return Ok(());
                }
            };
            if let Err(why) = self.tree.push(&head, start, self.through - 1) {
                self.broke(why);
                return Ok(());
            }
        }
    }

    fn broke(&mut self, why: String) {
        self.broken = Some(why);
        self.tree = Tree::default();
    }

    fn weight(&self) -> usize {
        self.tree.nodes.len() + 1
    }
}

/// How many entries the trees kept may hold between them; past it, the tree walked
/// longest ago is dropped first. An entry is under a hundred bytes: about 45 MiB at the
/// bound. Counted in entries rather than transcripts because a listing walks every
/// transcript written to in its window, however many that is.
const ENTRIES: usize = 1 << 19;

/// Every tree kept, by transcript, with a clock that says which was walked last.
#[derive(Default)]
pub struct Trees {
    kept: Mutex<(u64, Kept)>,
}

type Kept = HashMap<PathBuf, Arc<Mutex<Index>>>;

/// A transcript's tree as it stands now, and the file open to read its records from.
pub struct Walk<'a> {
    pub tree: &'a Tree,
    pub open: &'a mut File,
    /// The file's device and inode.
    pub file: (u64, u64),
}

impl Trees {
    /// Read `file`'s tree on to its end, and hand it to `read`; an error where the file
    /// cannot be read as a tree.
    pub fn walk<T>(
        &self,
        file: &Path,
        read: impl FnOnce(Walk<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        let failed = |e: io::Error| format!("{}: {e}", file.display());
        let mut open = File::open(file).map_err(failed)?;
        let meta = open.metadata().map_err(failed)?;
        let index = {
            let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
            let (clock, trees) = &mut *kept;
            *clock += 1;
            let index = trees.entry(file.to_owned()).or_default().clone();
            index.lock().unwrap_or_else(PoisonError::into_inner).used = *clock;
            index
        };
        let mut index = index.lock().unwrap_or_else(PoisonError::into_inner);
        let identity = (meta.dev(), meta.ino());
        index
            .update(&mut open, identity, meta.len())
            .map_err(failed)?;
        let weight = index.weight();
        if let Some(broken) = &index.broken {
            return Err(broken.clone());
        }
        let result = read(Walk {
            tree: &index.tree,
            open: &mut open,
            file: identity,
        });
        drop(index);
        self.shed(file, weight);
        result
    }

    /// Drop the trees walked longest ago until those left hold at most [`ENTRIES`]
    /// entries; the one just walked stays whatever it holds.
    fn shed(&self, walked: &Path, weight: usize) {
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        let trees = &mut kept.1;
        let weigh = |index: &Arc<Mutex<Index>>| {
            index
                .try_lock()
                .map_or((0, u64::MAX), |index| (index.weight(), index.used))
        };
        let mut held: usize = trees
            .iter()
            .map(|(path, index)| {
                if path == walked {
                    weight
                } else {
                    weigh(index).0
                }
            })
            .sum();
        while held > ENTRIES && trees.len() > 1 {
            let Some((oldest, weight)) = trees
                .iter()
                .filter(|(path, _)| path.as_path() != walked)
                .map(|(path, index)| (path, weigh(index)))
                .min_by_key(|(_, (_, used))| *used)
                .map(|(path, (weight, _))| (path.clone(), weight))
            else {
                return;
            };
            trees.remove(&oldest);
            held -= weight;
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
    use super::*;
    use serde_json::Value;

    fn file(text: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "steery-tree-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, text).unwrap();
        path
    }

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    const V3: &str = "{\"type\":\"session\",\"version\":3,\"id\":\"s\"}\n";

    /// A pi or omp entry, carrying its place in the test as `n`.
    fn entry(id: &str, parent: Option<&str>, n: i64) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "message", "id": id, "parentId": parent, "n": n,
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

    /// The active path, leaf first, as each entry's `n`.
    fn walked(trees: &Trees, path: &Path) -> Result<Vec<i64>, String> {
        trees.walk(path, |walk| {
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

    /// A tree is read on as it grows, from whichever leaf was written last: a branch moves
    /// the leaf and leaves the other path in the file, half a line is no entry yet, a line
    /// that is not JSON says nothing, and a transcript written whole again under the same
    /// name — pi migrating it — is read again from its start.
    #[test]
    fn a_tree_is_read_on_as_it_grows_and_walked_from_its_leaf() {
        let trees = Trees::default();
        let path = file(&format!(
            "{{\"type\":\"title\",\"title\":\"x\"}}\n{V3}{}{}",
            entry("a", None, 1),
            entry("b", Some("a"), 2)
        ));
        assert_eq!(walked(&trees, &path).unwrap(), [2, 1]);
        append(&path, &entry("c", Some("a"), 3));
        let half = entry("d", Some("c"), 4);
        append(&path, &half[..half.len() / 2]);
        assert_eq!(
            walked(&trees, &path).unwrap(),
            [3, 1],
            "the other branch, and half a line is none"
        );
        append(&path, &half[half.len() / 2..]);
        append(&path, "not json\n");
        assert_eq!(walked(&trees, &path).unwrap(), [4, 3, 1]);
        std::fs::write(
            &path,
            format!(
                "{}{}{}",
                V3.replace("\"s\"", "\"t\""),
                entry("x", None, 8),
                entry("y", Some("x"), 9)
            ),
        )
        .unwrap();
        assert_eq!(
            walked(&trees, &path).unwrap(),
            [9, 8],
            "written whole again"
        );
    }

    /// Only a first-version header, which could leave its version out, reads entries as a
    /// line. Every later tree that cannot be walked to a beginning is refused — never read
    /// in file order, never cut short — and a hole off the active path is not the leaf's.
    #[test]
    fn only_a_first_version_is_a_line_and_a_broken_tree_is_refused() {
        let trees = Trees::default();
        let v1 = file(
            "{\"type\":\"session\",\"id\":\"s\"}\n{\"type\":\"message\",\"n\":1}\n{\"type\":\"message\",\"n\":2}\n",
        );
        assert_eq!(walked(&trees, &v1).unwrap(), [2, 1]);
        let aside = format!(
            "{V3}{}{}{}",
            entry("a", None, 1),
            entry("b", Some("z"), 2),
            entry("c", Some("a"), 3)
        );
        assert_eq!(walked(&trees, &file(&aside)).unwrap(), [3, 1]);
        for (why, body) in [
            ("no header", entry("a", None, 1)),
            (
                "no parent named",
                format!("{V3}{{\"type\":\"message\",\"id\":\"a\",\"n\":1}}\n"),
            ),
            (
                "no id",
                format!("{V3}{{\"type\":\"message\",\"parentId\":null,\"n\":1}}\n"),
            ),
            (
                "an id that is no string",
                format!("{V3}{{\"type\":\"message\",\"id\":7,\"parentId\":null}}\n"),
            ),
            (
                "one id twice",
                format!("{V3}{}{}", entry("a", None, 1), entry("a", Some("a"), 2)),
            ),
            (
                "a parent not held",
                format!("{V3}{}{}", entry("a", None, 1), entry("b", Some("z"), 2)),
            ),
            (
                "a parent written after its child",
                format!(
                    "{V3}{}{}{}",
                    entry("a", Some("b"), 1),
                    entry("b", None, 2),
                    entry("c", Some("a"), 3)
                ),
            ),
        ] {
            assert!(walked(&trees, &file(&body)).is_err(), "{why}");
        }
    }
}
