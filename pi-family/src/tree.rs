//! A pi or omp transcript is a tree: after its `session` header every entry names its
//! parent by id (`parentId`, `null` at a root), and the conversation is the path from the
//! last entry back to a root. A branch moves the leaf and leaves the other path in the
//! file, read by nobody. Only a first-version header, which could leave its version out,
//! makes the entries a line, each the parent of the next.
//!
//! The tree, kept between reads and walked in batches, is the drivers' own
//! ([`steery_driver_common::tree`]); this is how pi's entries make it.

use serde::{Deserialize, Deserializer};
use steery_driver_common::tree::{Parent, Shape, Tree};

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

/// How a pi or omp transcript's entries make its tree.
#[derive(Default)]
pub struct Entries {
    /// The header's `version` once the header is read; 1 where it names none.
    version: Option<u64>,
}

impl Shape for Entries {
    /// omp's `title` slot, rewritten in place above the header, is no entry. Every version
    /// past the first names each entry's parent, and an entry naming none, or one id given
    /// twice, is a transcript that cannot be read as a tree. A line that is not JSON says
    /// nothing, as pi reads it; one that is JSON and no entry cannot be read as a tree.
    fn read(&mut self, tree: &mut Tree, line: &[u8], start: u64, end: u64) -> Result<(), String> {
        let head = match serde_json::from_slice::<Head>(line) {
            Ok(head) => head,
            Err(error) if !error.is_data() => return Ok(()),
            Err(_) => return Err("that transcript has a record that is no entry".into()),
        };
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
        let full = || "that transcript holds more entries than can be read".to_owned();
        if version == 1 {
            tree.add(start, end, tree.last()).ok_or_else(full)?;
            return Ok(());
        }
        let id = head
            .id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or("that transcript has an entry with no id")?;
        let parent = match &head.parent {
            Some(None) => Parent::Root,
            Some(Some(parent)) if !parent.is_empty() => Parent::Named(parent),
            _ => return Err("that transcript has an entry that names no parent".into()),
        };
        let at = tree.add(start, end, parent).ok_or_else(full)?;
        if !tree.name(id, at) {
            return Err("that transcript gives two entries one id".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use steery_driver_common::tree::Trees;

    use super::*;

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
    fn walked(trees: &Trees<Entries>, path: &Path) -> Result<Vec<i64>, String> {
        trees.walk(path, Entries::default(), |walk| {
            let mut seen = Vec::new();
            for at in walk.tree.path()?.into_iter().rev() {
                let record = walk.record(walk.tree.node(at))?.value;
                seen.push(record.and_then(|record| record["n"].as_i64()).unwrap_or(-1));
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
        ] {
            assert!(walked(&trees, &file(&body)).is_err(), "{why}");
        }
    }
}
