//! A transcript that is a tree: every record names the one before it, and the
//! conversation is the path from the last record back to a root. Going back to an earlier
//! record and on from there — a branch, a rewind — moves the leaf and leaves the other path
//! in the file, read by nobody but for what it spent.
//!
//! What is kept of a transcript between reads is its tree — where each record lies and
//! which record is its parent, never a word of a record — read on from where the last read
//! left the file. How a record names its parent is the harness's own, said by a [`Shape`].
//! A transcript its shape cannot read as a tree, or whose leaf's ancestry does not reach a
//! beginning, is an error and never a shorter conversation.
//!
//! A read goes along the path from where its cursor stands and says at most [`BATCH`]
//! bytes of records; once the path is read to its end, the records off it written since
//! are read too, within what is left of the same bound, so that no read of a long,
//! branch-heavy transcript runs past its own (PROTOCOL.md §9).

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;

use crate::follow::Followed;
use crate::lines::{BATCH, RECORD, bytes};

/// Why a walk stopped short of a beginning. Said, rather than handing back what had been
/// walked, which would read exactly like a whole short conversation.
const CARRIED_ON: &str = "that conversation carries on from one this machine does not hold, so only part of it can be read";

/// How a harness's records make a tree: one per transcript, kept with its tree.
pub trait Shape: Default + Send + 'static {
    /// One more line of the file, without its newline, at `start..end`: added to `tree`
    /// where it is a record of it. An error is a transcript that cannot be read as a tree,
    /// left unread until it is replaced or written whole again.
    fn read(&mut self, tree: &mut Tree, line: &[u8], start: u64, end: u64) -> Result<(), String>;
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

/// A record's parent, as a [`Shape`] names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parent {
    /// None: the record begins a conversation.
    Root,
    /// The record at this place in the tree.
    Node(u32),
    /// A record the file does not hold before it.
    Unheld,
}

/// The tree of one transcript. A parent is looked up among the records before it when a
/// record is added, so every parent comes before its child and no walk can go round in a
/// circle.
#[derive(Default)]
pub struct Tree {
    ids: HashMap<Box<str>, u32>,
    nodes: Vec<Node>,
}

impl Tree {
    /// The record named `id`, where one before is.
    pub fn find(&self, id: &str) -> Parent {
        self.ids
            .get(id)
            .map_or(Parent::Unheld, |at| Parent::Node(*at))
    }

    /// The last record added, or none: the parent of a record that follows the one
    /// before it.
    pub fn last(&self) -> Parent {
        self.nodes
            .len()
            .checked_sub(1)
            .map_or(Parent::Root, |at| Parent::Node(at as u32))
    }

    /// Add the record at `start..end` (its newline not included) under `parent`; its
    /// place, or none where the tree holds as many records as can be.
    pub fn add(&mut self, start: u64, end: u64, parent: Parent) -> Option<u32> {
        let at = u32::try_from(self.nodes.len())
            .ok()
            .filter(|at| *at < UNHELD)?;
        let parent = match parent {
            Parent::Root => ROOT,
            Parent::Node(node) => node,
            Parent::Unheld => UNHELD,
        };
        self.nodes.push(Node { start, end, parent });
        Some(at)
    }

    /// Name the record at `at` `id`; whether no record before had that name. A later
    /// record given the name takes it.
    pub fn name(&mut self, id: &str, at: u32) -> bool {
        self.ids.insert(id.into(), at).is_none()
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
struct Index<S> {
    followed: Followed,
    shape: S,
    tree: Tree,
    /// Why the file cannot be read as a tree, once it cannot: kept until it is replaced.
    broken: Option<String>,
    /// When this tree was last walked, by the clock of [`Trees`].
    used: u64,
}

impl<S: Shape> Index<S> {
    /// Read what was appended since the last read, up to its last newline. A file
    /// replaced, shortened or written whole again is read again from its start, its shape
    /// `fresh`.
    fn update(&mut self, open: &File, fresh: S) -> std::io::Result<()> {
        if self.followed.restart(open)? {
            self.shape = fresh;
            self.tree = Tree::default();
            self.broken = None;
        }
        if self.broken.is_some() {
            return Ok(());
        }
        let Self {
            followed,
            shape,
            tree,
            broken,
            ..
        } = self;
        followed.read_on(open, |line, start, end| {
            match shape.read(tree, line, start, end) {
                Ok(()) => true,
                Err(why) => {
                    *broken = Some(why);
                    false
                }
            }
        })?;
        if self.broken.is_some() {
            self.tree = Tree::default();
        }
        Ok(())
    }

    fn weight(&self) -> usize {
        self.tree.nodes.len() + 1
    }
}

/// How many records the trees kept may hold between them; past it, the tree walked
/// longest ago is dropped first. A record is under a hundred bytes: about 45 MiB at the
/// bound. Counted in records rather than transcripts because a listing walks every
/// transcript written to in its window, however many that is.
const ENTRIES: usize = 1 << 19;

/// Every tree kept, by transcript, with a clock that says which was walked last. Each
/// transcript is walked under its own lock, so a long read of one holds no other.
pub struct Trees<S> {
    kept: Mutex<(u64, Kept<S>)>,
}

type Kept<S> = HashMap<PathBuf, Arc<Mutex<Index<S>>>>;

impl<S> Default for Trees<S> {
    fn default() -> Self {
        Self {
            kept: Mutex::new((0, HashMap::new())),
        }
    }
}

/// A transcript's tree as it stands now, and the file open to read its records from.
pub struct Walk<'a> {
    pub tree: &'a Tree,
    pub open: &'a File,
    /// The file's device and inode.
    pub file: (u64, u64),
    /// How long the file is, as far as the tree was read.
    pub len: u64,
    path: &'a Path,
}

impl<S: Shape> Trees<S> {
    /// Read `file`'s tree on to its end, and hand it to `read`; an error where the file
    /// cannot be read as a tree. `fresh` is its shape where it is read from its start.
    pub fn walk<T>(
        &self,
        file: &Path,
        fresh: S,
        read: impl FnOnce(Walk<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        let failed = |e: std::io::Error| format!("{}: {e}", file.display());
        let open = File::open(file).map_err(failed)?;
        let index = {
            let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
            let (clock, trees) = &mut *kept;
            *clock += 1;
            let index = trees.entry(file.to_owned()).or_default().clone();
            index.lock().unwrap_or_else(PoisonError::into_inner).used = *clock;
            index
        };
        let mut index = index.lock().unwrap_or_else(PoisonError::into_inner);
        index.update(&open, fresh).map_err(failed)?;
        let weight = index.weight();
        if let Some(broken) = &index.broken {
            return Err(broken.clone());
        }
        let result = read(Walk {
            tree: &index.tree,
            open: &open,
            file: index.followed.file(),
            len: index.followed.length(),
            path: file,
        });
        drop(index);
        self.shed(file, weight);
        result
    }

    /// Drop the trees walked longest ago until those left hold at most [`ENTRIES`]
    /// records; the one just walked stays whatever it holds.
    fn shed(&self, walked: &Path, weight: usize) {
        let mut kept = self.kept.lock().unwrap_or_else(PoisonError::into_inner);
        let trees = &mut kept.1;
        let weigh = |index: &Arc<Mutex<Index<S>>>| {
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

/// One record of the file: where it starts, and what it holds, none where it is longer
/// than [`RECORD`] or no JSON.
pub struct Record {
    pub start: u64,
    pub value: Option<Value>,
}

/// Where a read of a transcript's path stands, as its cursor says it: the file it was, by
/// its device and inode; the last record of its path whose events were said (`tip`,
/// counted from 1, 0 for none) and where that record starts; how far into the file the
/// records off the path were read (`aside`); and `mark`, what the driver keeps of the
/// conversation between reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub file: (u64, u64),
    pub tip: u32,
    pub at: u64,
    pub aside: u64,
    pub mark: u64,
}

impl Cursor {
    /// `cursor` as a driver whose cursors are `version` gave it, or none.
    pub fn parse(version: &str, cursor: &str) -> Option<Self> {
        let mut parts = cursor.split('.');
        if parts.next() != Some(version) {
            return None;
        }
        let mut numbers = [0u64; 6];
        for number in &mut numbers {
            *number = parts.next()?.parse().ok()?;
        }
        if parts.next().is_some() {
            return None;
        }
        let [device, inode, tip, at, aside, mark] = numbers;
        Some(Self {
            file: (device, inode),
            tip: u32::try_from(tip).ok()?,
            at,
            aside,
            mark,
        })
    }

    /// The cursor as a driver whose cursors are `version` gives it.
    pub fn said(&self, version: &str) -> String {
        format!(
            "{version}.{}.{}.{}.{}.{}.{}",
            self.file.0, self.file.1, self.tip, self.at, self.aside, self.mark
        )
    }
}

/// One read of a transcript's path, from where a cursor or a tail puts it.
pub struct Read {
    /// How many records of the path were read before this one: 0 where it reads from the
    /// beginning.
    pub from: usize,
    /// The cursor's `mark`, where the read goes on from it.
    pub mark: Option<u64>,
    /// Whether a cursor was given and the read cannot go on from it: the conversation went
    /// back to an earlier record and on from there, or the file was replaced or written
    /// whole again. The read then starts again from the beginning.
    pub reset: bool,
    /// The path's records from `from`, at most about [`BATCH`] bytes of them, at least one
    /// where any remain.
    pub records: Vec<Record>,
    /// Whether the path goes on past them.
    pub rest: bool,
    /// Once the path is read to its end: the records off it from where the last read left
    /// them, within what is left of [`BATCH`]: what a branch left behind still spent.
    pub aside: Vec<Record>,
    /// How far into the file the records off the path are read now.
    pub through: u64,
    /// Whether anything is left to read: the path, or records off it.
    pub more: bool,
}

impl Walk<'_> {
    /// The record of `node`.
    pub fn record(&self, node: Node) -> Result<Record, String> {
        let value = if node.end - node.start > RECORD as u64 {
            None
        } else {
            let bytes = bytes(self.open, node.start, node.end)
                .map_err(|e| format!("{}: {e}", self.path.display()))?;
            serde_json::from_slice(&bytes).ok()
        };
        Ok(Record {
            start: node.start,
            value,
        })
    }

    /// Read `path` on from `given`, the cursor a read before said, or the records within
    /// `tail` bytes of the end. A cursor goes on from its record where that record is
    /// still on the path, in the same file; else the read starts again from the beginning.
    pub fn read(
        &self,
        path: &[u32],
        given: Option<&Cursor>,
        tail: Option<u64>,
    ) -> Result<Read, String> {
        // Where on the path the cursor's record is: the records before `from` are read.
        let on = given.and_then(|cursor| {
            (cursor.file == self.file).then_some(())?;
            if cursor.tip == 0 {
                return Some(0);
            }
            let tip = cursor.tip - 1;
            let at = path.iter().position(|node| *node == tip)?;
            (self.tree.node(tip).start == cursor.at).then_some(at + 1)
        });
        let reset = given.is_some() && on.is_none();
        let (from, aside) = match (on, given, tail) {
            (Some(from), Some(cursor), _) => (from, cursor.aside),
            (None, _, Some(tail)) if !reset => {
                let edge = self.len.saturating_sub(tail);
                let from = path
                    .iter()
                    .position(|node| self.tree.node(*node).start >= edge)
                    .unwrap_or(path.len());
                (from, edge)
            }
            _ => (0, 0),
        };
        let mut taken = 0;
        let mut records = Vec::new();
        for node in &path[from..] {
            if taken >= BATCH {
                break;
            }
            let node = self.tree.node(*node);
            taken += node.end - node.start;
            records.push(self.record(node)?);
        }
        let rest = from + records.len() < path.len();
        let (off, through, more) = if rest {
            (Vec::new(), aside, true)
        } else {
            self.aside(path, aside, taken)?
        };
        Ok(Read {
            from,
            mark: on.and(given).map(|cursor| cursor.mark),
            reset,
            records,
            rest,
            aside: off,
            through,
            more,
        })
    }

    /// The records off `path` from `aside` on, while what was `taken` of the batch and
    /// they come to less than [`BATCH`]; how far into the file they were read, and whether
    /// any are left past it.
    fn aside(
        &self,
        path: &[u32],
        aside: u64,
        mut taken: u64,
    ) -> Result<(Vec<Record>, u64, bool), String> {
        let mut on_path = vec![false; self.tree.nodes.len()];
        for node in path {
            on_path[*node as usize] = true;
        }
        let mut records = Vec::new();
        let mut through = aside;
        for (node, on) in self.tree.nodes.iter().zip(on_path) {
            if on || node.start < aside {
                through = through.max(node.end + 1);
                continue;
            }
            if taken >= BATCH {
                return Ok((records, node.start, true));
            }
            taken += node.end - node.start;
            records.push(self.record(*node)?);
            through = through.max(node.end + 1);
        }
        Ok((records, through, false))
    }

    /// The cursor after the first `read` records of `path`, the records off it read
    /// `through`, with `mark`.
    pub fn cursor(&self, path: &[u32], read: usize, through: u64, mark: u64) -> Cursor {
        let tip = read.checked_sub(1).map(|at| path[at]);
        Cursor {
            file: self.file,
            tip: tip.map_or(0, |tip| tip + 1),
            at: tip.map_or(0, |tip| self.tree.node(tip).start),
            aside: through,
            mark,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records naming their parent by `id` and `parent`, as both shapes of the drivers'
    /// harnesses do; a line that is no such record is no record.
    #[derive(Default)]
    struct Linked;

    impl Shape for Linked {
        fn read(
            &mut self,
            tree: &mut Tree,
            line: &[u8],
            start: u64,
            end: u64,
        ) -> Result<(), String> {
            let Ok(record) = serde_json::from_slice::<Value>(line) else {
                return Ok(());
            };
            if record["broken"] == true {
                return Err("broken".into());
            }
            let Some(id) = record["id"].as_str() else {
                return Ok(());
            };
            let parent = record["parent"]
                .as_str()
                .map_or(Parent::Root, |parent| tree.find(parent));
            let at = tree.add(start, end, parent).ok_or("full")?;
            tree.name(id, at);
            Ok(())
        }
    }

    fn file(text: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "steery-common-tree-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, text).unwrap();
        path
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

    /// A record carrying its place in the test as `n`, `pad` bytes long or more.
    fn record(id: &str, parent: Option<&str>, n: i64, pad: usize) -> String {
        format!(
            "{}\n",
            serde_json::json!({"id": id, "parent": parent, "n": n, "pad": "x".repeat(pad)})
        )
    }

    fn n(record: &Record) -> i64 {
        record
            .value
            .as_ref()
            .and_then(|value| value["n"].as_i64())
            .unwrap_or(-1)
    }

    /// The active path, leaf first, as each record's `n`.
    fn walked(trees: &Trees<Linked>, path: &Path) -> Result<Vec<i64>, String> {
        trees.walk(path, Linked, |walk| {
            let mut seen = Vec::new();
            for at in walk.tree.path()?.into_iter().rev() {
                seen.push(n(&walk.record(walk.tree.node(at))?));
            }
            Ok(seen)
        })
    }

    /// A tree is read on as it grows, from whichever record was written last: a branch
    /// moves the leaf and leaves the other path in the file, half a line is no record yet,
    /// a line that is not one says nothing, and a transcript written whole again under the
    /// same name is read again from its start.
    #[test]
    fn a_tree_is_read_on_as_it_grows_and_walked_from_its_leaf() {
        let trees = Trees::default();
        let path = file(&format!(
            "{}{}",
            record("a", None, 1, 0),
            record("b", Some("a"), 2, 0)
        ));
        assert_eq!(walked(&trees, &path).unwrap(), [2, 1]);
        append(&path, &record("c", Some("a"), 3, 0));
        let half = record("d", Some("c"), 4, 0);
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
                "{}{}",
                record("x", None, 8, 0),
                record("y", Some("x"), 9, 0)
            ),
        )
        .unwrap();
        assert_eq!(
            walked(&trees, &path).unwrap(),
            [9, 8],
            "written whole again"
        );
    }

    /// A walk that cannot reach a beginning says so, as does a transcript its shape cannot
    /// read, until it is written whole again; a hole off the path is not the leaf's.
    #[test]
    fn a_tree_that_cannot_be_walked_is_refused() {
        let trees = Trees::default();
        assert!(walked(&trees, &file(&record("b", Some("elsewhere"), 1, 0))).is_err());
        let circle = file(&format!(
            "{}{}",
            record("a", Some("b"), 1, 0),
            record("b", Some("a"), 2, 0)
        ));
        assert!(walked(&trees, &circle).is_err());
        let aside = file(&format!(
            "{}{}{}",
            record("a", None, 1, 0),
            record("b", Some("z"), 2, 0),
            record("c", Some("a"), 3, 0)
        ));
        assert_eq!(walked(&trees, &aside).unwrap(), [3, 1]);
        let broken = file(&format!("{}{{\"broken\":true}}\n", record("a", None, 1, 0)));
        assert_eq!(walked(&trees, &broken).unwrap_err(), "broken");
        append(&broken, &record("b", Some("a"), 2, 0));
        assert_eq!(
            walked(&trees, &broken).unwrap_err(),
            "broken",
            "until written whole again"
        );
        std::fs::write(&broken, record("a", None, 1, 0)).unwrap();
        assert_eq!(walked(&trees, &broken).unwrap(), [1]);
    }

    /// What one read said: the path's records and the records off it, as their `n`.
    type Said = (Vec<i64>, Vec<i64>);

    /// Every read of `path` from `cursor` until nothing is left: for each, the path's
    /// records and the records off it, as their `n`; and the cursor the last one said.
    fn reads(
        trees: &Trees<Linked>,
        path: &Path,
        mut cursor: Option<Cursor>,
    ) -> (Vec<Said>, Option<Cursor>) {
        let mut out = Vec::new();
        loop {
            let (said, next, more) = trees
                .walk(path, Linked, |walk| {
                    let nodes = walk.tree.path()?;
                    let read = walk.read(&nodes, cursor.as_ref(), None)?;
                    assert!(!read.reset);
                    let next = walk.cursor(&nodes, read.from + read.records.len(), read.through, 7);
                    let said = (
                        read.records.iter().map(n).collect(),
                        read.aside.iter().map(n).collect(),
                    );
                    Ok((said, next, read.more))
                })
                .unwrap();
            assert_eq!(Cursor::parse("9", &next.said("9")).as_ref(), Some(&next));
            out.push(said);
            cursor = Some(next);
            if !more {
                return (out, cursor);
            }
            assert!(out.len() < 20, "a read that never ends");
        }
    }

    /// A long, branch-heavy transcript read for the first time is read in batches: the
    /// records off the path count against the same bound as the path's, a read stops among
    /// them and says `more`, the next goes on from where it stopped, and each is said once.
    #[test]
    fn records_off_the_path_are_read_in_batches_and_each_once() {
        let trees = Trees::default();
        let mb = 1 << 20;
        let mut text = record("root", None, 0, 0);
        // Six abandoned branches of 1 MiB each off the root, then the path.
        for n in 1..=6 {
            text.push_str(&record(&format!("gone{n}"), Some("root"), n, mb));
        }
        text.push_str(&record("kept", Some("root"), 100, 0));
        let path = file(&text);
        let (said, cursor) = reads(&trees, &path, None);
        assert_eq!(said[0].0, [0, 100], "the path first");
        let aside: Vec<i64> = said.iter().flat_map(|(_, aside)| aside.clone()).collect();
        assert_eq!(aside, [1, 2, 3, 4, 5, 6], "each once, in file order");
        assert!(said.len() >= 2, "more than one batch");
        for (_, aside) in &said {
            assert!(aside.len() <= 5, "within the bound: {aside:?}");
        }
        // What is written since is read on, past what was read.
        append(&path, &record("gone7", Some("root"), 7, 0));
        append(&path, &record("next", Some("kept"), 101, 0));
        let (said, _) = reads(&trees, &path, cursor);
        assert_eq!(said, [(vec![101], vec![7])]);
    }

    /// A cursor of another file, or whose record is no longer on the path, starts the
    /// read again from the beginning; a tail starts at the first record within it.
    #[test]
    fn a_cursor_off_the_path_starts_again_and_a_tail_reads_the_end() {
        let trees = Trees::default();
        let path = file(&format!(
            "{}{}{}",
            record("a", None, 1, 0),
            record("b", Some("a"), 2, 0),
            record("c", Some("a"), 3, 0)
        ));
        trees
            .walk(&path, Linked, |walk| {
                let nodes = walk.tree.path()?;
                let gone = Cursor {
                    file: walk.file,
                    tip: 2,
                    at: walk.tree.node(1).start,
                    aside: 0,
                    mark: 1,
                };
                let read = walk.read(&nodes, Some(&gone), None)?;
                assert!(read.reset && read.from == 0 && read.mark.is_none());
                assert_eq!(read.records.iter().map(n).collect::<Vec<_>>(), [1, 3]);
                assert_eq!(read.aside.iter().map(n).collect::<Vec<_>>(), [2]);
                let kept = walk.cursor(&nodes, 1, 0, 1);
                let read = walk.read(&nodes, Some(&kept), None)?;
                assert!(!read.reset && read.from == 1 && read.mark == Some(1));
                let elsewhere = Cursor {
                    file: (0, 0),
                    ..kept
                };
                assert!(walk.read(&nodes, Some(&elsewhere), None)?.reset);
                let tail = walk.read(&nodes, None, Some(walk.len - walk.tree.node(2).start))?;
                assert_eq!(tail.from, 1);
                assert_eq!(tail.records.iter().map(n).collect::<Vec<_>>(), [3]);
                assert!(tail.aside.is_empty(), "the branch is before the tail");
                Ok(())
            })
            .unwrap();
    }
}
