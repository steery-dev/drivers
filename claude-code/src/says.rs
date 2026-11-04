//! Which turn of a live session took Steery's words (protocol 7). The runtime names each
//! `say` it sends (`Say::id`), and its words are given here before they go into the
//! inbox, with where the session's transcript ended then. Claude Code writes words it
//! takes as a person's input ([`crate::drive::input_carries`]): a `user` record that
//! begins a turn, or a queued prompt a running turn reads between its calls. While any
//! say waits to be seen, the state follower ([`crate::state`]) reads the transcript on
//! from that end, and the turn running when the words went in, or the next one to begin,
//! is the one that took them.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use steery_driver_common::receipt::{self, DELIVERED};

use crate::events::{millis, text};

/// How long words given are looked for before the say is let go: the runtime has long
/// given up its own look for them (`receipt/find`) by then.
const GIVEN_KEPT: Duration = Duration::from_secs(15 * 60);

/// The most of a transcript one look reads: a file read again from its start may be
/// hundreds of megabytes, and the rest is read by the looks after.
const LOOK_MOST: u64 = 64 << 20;

/// Every live session's says, shared by `say`, `stop` and the state follower.
#[derive(Default)]
pub struct Says {
    sessions: Mutex<HashMap<String, Session>>,
}

/// One session's says.
#[derive(Default)]
struct Session {
    /// Where its transcript is read on from while words wait to be seen.
    read: Option<Read>,
    /// Says whose words were not yet seen go in.
    given: Vec<Given>,
    /// Says whose words went in, each with when, that no turn ended since: the turn
    /// running took them, or the next one to begin will.
    took: Vec<(String, u64)>,
}

/// How far a transcript has been read: which file, once opened, and the end of its last
/// whole record read.
struct Read {
    path: PathBuf,
    file: Option<(u64, u64)>,
    through: u64,
}

/// A say whose words were handed to the inbox: its id, what its words are known by in a
/// record, and when it was given.
struct Given {
    id: String,
    marker: String,
    at: Instant,
}

impl Read {
    /// Read on from where `path` ends now, or from its start where its end cannot be read.
    fn from_end(path: &Path) -> Self {
        let end = receipt::end(path).ok();
        Self {
            path: path.to_owned(),
            file: end.map(|end| (end.dev, end.ino)),
            through: end.map_or(0, |end| end.bytes),
        }
    }
}

impl Says {
    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Session>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The say `id` of `words`, about to go into the session `external_id`, whose
    /// transcript is `transcript` where it is known. Its words are known by the delivery
    /// marker of `id` where they carry it, else by themselves. Words of nothing are none.
    pub fn give(&self, external_id: &str, id: &str, words: &str, transcript: Option<&Path>) {
        let delivered = format!("{DELIVERED}{id}");
        let marker = match receipt::marked(words, &delivered) {
            true => delivered,
            false => words.trim().to_owned(),
        };
        if marker.is_empty() {
            return;
        }
        let mut sessions = self.sessions();
        let session = sessions.entry(external_id.to_owned()).or_default();
        if session.read.is_none() {
            session.read = transcript.map(Read::from_end);
        }
        session.given.retain(|given| given.id != id);
        session.given.push(Given {
            id: id.to_owned(),
            marker,
            at: Instant::now(),
        });
    }

    /// The say `id` let go: its words never reached the session.
    pub fn ungive(&self, external_id: &str, id: &str) {
        if let Some(session) = self.sessions().get_mut(external_id) {
            session.given.retain(|given| given.id != id);
        }
    }

    /// Read on the transcript of `external_id`, `transcript` where it is known, as far as
    /// its last whole record, while words given wait to be seen: each say whose words a
    /// person's input carries went in when that record was written.
    pub fn read(&self, external_id: &str, transcript: Option<&Path>) {
        let mut sessions = self.sessions();
        let Some(Session { read, given, took }) = sessions.get_mut(external_id) else {
            return;
        };
        given.retain(|given| given.at.elapsed() < GIVEN_KEPT);
        if given.is_empty() {
            *read = None;
            return;
        }
        // Words given before the transcript was found are looked for from its start.
        if read.is_none()
            && let Some(path) = transcript
        {
            *read = Some(Read {
                path: path.to_owned(),
                file: None,
                through: 0,
            });
        }
        let Some(read) = read.as_mut() else {
            return;
        };
        let Ok(file) = File::open(&read.path) else {
            return;
        };
        let Ok(meta) = file.metadata() else {
            return;
        };
        // Another file under the name, or a shorter one, is read again from its start, as
        // `receipt/find` reads it.
        let identity = (meta.dev(), meta.ino());
        if read.file != Some(identity) || meta.len() < read.through {
            read.file = Some(identity);
            read.through = 0;
        }
        if meta.len() == read.through {
            return;
        }
        let mut file = file;
        if file.seek(SeekFrom::Start(read.through)).is_err() {
            return;
        }
        let most = (meta.len() - read.through).min(LOOK_MOST);
        let mut lines = BufReader::with_capacity(1 << 20, file.take(most));
        let mut line = Vec::new();
        while !given.is_empty() {
            line.clear();
            if !matches!(lines.read_until(b'\n', &mut line), Ok(1..)) || line.last() != Some(&b'\n')
            {
                break;
            }
            read.through += line.len() as u64;
            let Ok(record) = serde_json::from_slice::<Value>(&line[..line.len() - 1]) else {
                continue;
            };
            given.retain(|one| {
                let carried = crate::drive::input_carries(&record, &one.marker);
                if carried {
                    let at = text(&record, "timestamp")
                        .and_then(millis)
                        .unwrap_or_else(crate::state::millis_now);
                    took.push((one.id.clone(), at));
                }
                !carried
            });
        }
    }

    /// The says whose words went in that no turn ended since, oldest first, each with
    /// when: the turn running took them, or the next one to begin will.
    pub fn took(&self, external_id: &str) -> Vec<(String, u64)> {
        self.sessions()
            .get(external_id)
            .map(|session| session.took.clone())
            .unwrap_or_default()
    }

    /// Whether the words of say `id` were given and not yet seen go in.
    pub fn waits(&self, external_id: &str, id: &str) -> bool {
        self.sessions()
            .get(external_id)
            .is_some_and(|session| session.given.iter().any(|given| given.id == id))
    }

    /// The says a turn that ended at `at` took, each with when its words went in: each
    /// whose words went in by then, which no later turn took.
    pub fn settle(&self, external_id: &str, at: u64) -> Vec<(String, u64)> {
        let mut sessions = self.sessions();
        let Some(session) = sessions.get_mut(external_id) else {
            return Vec::new();
        };
        let (settled, later) = std::mem::take(&mut session.took)
            .into_iter()
            .partition(|(_, went)| *went <= at);
        session.took = later;
        settled
    }

    /// A session gone: its says with it.
    pub fn forget(&self, external_id: &str) {
        self.sessions().remove(external_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(record: serde_json::Value) -> String {
        format!("{record}\n")
    }

    /// Words given are taken by the record that carries their delivery marker whole,
    /// whether a turn began with them or a running one read them from its queue, at that
    /// record's time; a turn that ended settles only the says that went in by its end.
    #[test]
    fn a_say_is_taken_by_the_record_carrying_it() {
        let dir = std::env::temp_dir().join(format!("says-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("t.jsonl");
        std::fs::write(
            &transcript,
            line(
                serde_json::json!({"type": "user", "message": {"role": "user", "content": "before"},
                "timestamp": "2026-11-04T10:00:00Z"}),
            ),
        )
        .unwrap();
        let says = Says::default();
        says.give(
            "s",
            "aim.1",
            "Do it.\nsteery-delivery:aim.1",
            Some(&transcript),
        );
        says.give(
            "s",
            "aim.10",
            "Then this.\nsteery-delivery:aim.10",
            Some(&transcript),
        );
        says.read("s", Some(&transcript));
        assert!(says.took("s").is_empty());
        assert!(says.waits("s", "aim.1"));

        let mut more = line(serde_json::json!({"type": "user", "isMeta": true,
            "origin": {"kind": "peer"}, "timestamp": "2026-11-04T10:00:05Z",
            "message": {"role": "user",
                "content": "Another Claude session sent a message:\nDo it.\nsteery-delivery:aim.1"}}));
        more.push_str(&line(
            serde_json::json!({"type": "attachment", "timestamp": "2026-11-04T10:00:09Z",
            "attachment": {"type": "queued_command", "commandMode": "prompt",
                "origin": {"kind": "peer"}, "prompt": "Then this.\nsteery-delivery:aim.10"}}),
        ));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        std::io::Write::write_all(&mut file, more.as_bytes()).unwrap();
        says.read("s", Some(&transcript));
        let took: Vec<String> = says.took("s").into_iter().map(|(id, _)| id).collect();
        assert_eq!(took, ["aim.1", "aim.10"]);
        assert!(!says.waits("s", "aim.1"));

        let first = millis("2026-11-04T10:00:07Z").unwrap();
        let ids = |took: Vec<(String, u64)>| took.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        assert_eq!(ids(says.settle("s", first)), ["aim.1"]);
        assert_eq!(ids(says.settle("s", first + 10_000)), ["aim.10"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
