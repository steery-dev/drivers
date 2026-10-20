//! A Claude Code transcript is a tree: every message, attachment and system record names
//! the record before it by its `uuid` (`parentUuid`, `null` at a root), and the
//! conversation is the path from the last such record no record names as its parent back
//! to a root. Claude Code does not always append a turn's records in the order it made
//! them, so a parent can be written after its child: an attachment before the reminder it
//! follows, a prompt before the answer it replies to. A rewind writes the next record
//! under an earlier one and leaves the other path in the file, read by nobody.
//! A compaction starts the chain again at its boundary record, which names the record it
//! follows as `logicalParentUuid`: the conversation goes on through it. Records that name
//! no `uuid` of their own — the mode, the queue, the title, the costs — are no part of it.
//! In a lead's transcript a sub-agent's records (`isSidechain`), which older releases
//! inlined there, are its own tree and none of the lead's.
//!
//! The tree, kept between reads and walked in batches, is the drivers' own
//! ([`steery_driver_common::tree`]); this is how Claude Code's records make it. A uuid
//! written twice names the later record, and no line breaks the tree: a line that is no
//! JSON object says nothing.

use serde::{Deserialize, Deserializer};
use steery_driver_common::tree::{Parent, Shape, Tree};

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

/// How a Claude Code transcript's records make its tree.
#[derive(Default)]
pub struct Records {
    /// The transcript is a sub-agent's, whose sidechain records are its own.
    pub own: bool,
}

impl Shape for Records {
    /// A message or system record that names no uuid follows the record before it.
    fn read(&mut self, tree: &mut Tree, line: &[u8], start: u64, end: u64) -> Result<(), String> {
        let Ok(head) = serde_json::from_slice::<Head>(line) else {
            return Ok(());
        };
        if !self.own && head.sidechain == Some(true) {
            return Ok(());
        }
        let id = head.uuid.as_deref().filter(|id| !id.is_empty());
        let parent = match id {
            Some(_) => match (&head.parent, head.logical.as_deref()) {
                (Some(Some(parent)), _) => Parent::Named(parent),
                (_, Some(logical)) => tree.find(logical).map_or(Parent::Root, Parent::Node),
                _ => Parent::Root,
            },
            None if matches!(head.kind.as_deref(), Some("user" | "assistant" | "system")) => {
                tree.last()
            }
            None => return Ok(()),
        };
        if let (Some(at), Some(id)) = (tree.add(start, end, parent), id) {
            tree.name(id, at);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::json;
    use steery_driver_common::tree::Trees;

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
    fn walked(trees: &Trees<Records>, path: &Path) -> Result<Vec<i64>, String> {
        trees.walk(path, Records::default(), |walk| {
            let mut seen = Vec::new();
            for at in walk.tree.path()?.into_iter().rev() {
                let record = walk.record(walk.tree.node(at))?.value;
                seen.push(record.and_then(|record| record["n"].as_i64()).unwrap_or(-1));
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

    /// A chain goes back through a parent written after its child, as Claude Code writes a
    /// turn's attachments before the record they follow; a walk that cannot reach a
    /// beginning says so: a parent the file does not hold, or records naming each other in
    /// a circle.
    #[test]
    fn a_chain_is_walked_to_a_beginning_or_refused() {
        let trees = Trees::default();
        let reordered = file(&format!(
            "{}{}{}{}{}",
            record("a", None, 1),
            record("b", Some("a"), 2),
            record("d", Some("c"), 4),
            record("e", Some("d"), 5),
            record("c", Some("b"), 3)
        ));
        assert_eq!(walked(&trees, &reordered).unwrap(), [5, 4, 3, 2, 1]);
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
