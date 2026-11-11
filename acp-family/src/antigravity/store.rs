//! Antigravity's conversations: a row each in `<root>/conversation_summaries.db` (its id,
//! title, workspaces, when it last changed, and, for a sub-agent's run, the conversation
//! that started it), and a folder each in `<root>/brain/<id>/` that holds its transcript.
//! Antigravity holds the database open in WAL mode as it runs; the driver opens it read
//! only.
//!
//! The transcript does not say whether a person was at it: a headless run's prompt is a
//! `USER_INPUT` step like a typed one. Antigravity's terminal writes each prompt a person
//! typed to `<root>/history.jsonl`, and a run with `-p` writes none. A prompt there names
//! its conversation, except the first prompt of a new one, which names only its folder and
//! time; that conversation is the one in the folder whose first step came within
//! [`FIRST_STEP`] of it. A conversation a person typed into is listed, one that is not is
//! excluded, and a sub-agent's run is listed under its lead, never on its own.
//!
//! While Antigravity runs a conversation, it holds an exclusive `flock` on
//! `<root>/presence/<id>.lock`, and releases it when it ends; the file stays. A sub-agent's
//! run has no lock of its own: it runs while its lead does.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::Value;
use steery_sdk::wire::harness::{
    Event, Events, List, Listed, Locate, Located, Read, Session, SubAgent,
};
use steery_sdk::{Error, refusal};

use super::events;

const DATABASE: &str = "conversation_summaries.db";
const HISTORY: &str = "history.jsonl";
const PRESENCE: &str = "presence";
pub(super) const BRAIN: &str = "brain";
pub(super) const TRANSCRIPT: &str = ".system_generated/logs/transcript_full.jsonl";

/// How far after a typed prompt its new conversation's first step may come. The terminal
/// writes the prompt to its history before it opens the conversation: 0.7 s before, as
/// recorded.
const FIRST_STEP: u64 = 30_000;

/// How deep sub-agents that start sub-agents are followed.
pub(super) const DEPTH: usize = 4;

const COLUMNS: &str = "conversation_id, title, last_modified_time, workspace_uris, \
     parent_conversation_id, agent_name";

/// Antigravity's conversations under its root.
pub struct Store {
    root: PathBuf,
}

/// One row of the database.
struct Row {
    id: String,
    title: String,
    modified: Option<u64>,
    cwd: Option<PathBuf>,
    parent: String,
    agent: String,
}

/// What was read of the root at one call.
struct Index {
    rows: BTreeMap<String, Row>,
    /// The conversations a person typed into.
    typed: HashSet<String>,
    complete: bool,
}

/// A prompt a person typed, as the terminal kept it.
#[derive(Deserialize)]
struct Typed {
    timestamp: u64,
    workspace: Option<PathBuf>,
    #[serde(rename = "conversationId")]
    conversation: Option<String>,
}

/// What the first step of a transcript says.
#[derive(Default)]
struct Head {
    started: Option<u64>,
    prompt: Option<String>,
    /// A sub-agent's task: the message its lead sent it.
    task: Option<String>,
}

impl Store {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
        }
    }

    /// `sessions/list`.
    pub fn list(&self, list: List) -> Listed {
        let index = self.index();
        let mut sessions = Vec::new();
        let mut excluded = Vec::new();
        for row in index.rows.values().filter(|row| row.parent.is_empty()) {
            if index.typed.contains(&row.id) {
                sessions.push(self.session(&index, row));
            } else {
                excluded.push(row.id.clone());
            }
        }
        sessions.retain(|session| {
            list.seen_since
                .is_none_or(|since| session.updated_at >= since)
        });
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        Listed {
            sessions,
            excluded,
            complete: index.complete,
            cursor: None,
            after: None,
            gone: Vec::new(),
        }
    }

    /// `sessions/locate`.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let index = self.index();
        let row = index.listed(&locate.external_id)?;
        Ok(Located {
            session: self.session(&index, row),
        })
    }

    /// `sessions/read`.
    pub fn read(&self, read: Read) -> Result<Events, Error> {
        let index = self.index();
        let lead = index.listed(&read.external_id)?;
        let id = match &read.sub_agent {
            None => &lead.id,
            Some(child) => index
                .descendants(&lead.id)
                .into_iter()
                .find(|row| &row.id == child)
                .map(|row| &row.id)
                .ok_or_else(|| not_found(child))?,
        };
        events::read(
            &self.transcript(id),
            read.cursor.as_deref(),
            read.tail,
            self.running(&lead.id),
        )
    }

    fn transcript(&self, id: &str) -> PathBuf {
        self.root.join(BRAIN).join(id).join(TRANSCRIPT)
    }

    /// Whether Antigravity runs the conversation `id`: whether a process holds its
    /// presence lock. The probe takes a shared lock and drops it at once.
    fn running(&self, id: &str) -> bool {
        let Ok(file) = File::open(self.root.join(PRESENCE).join(format!("{id}.lock"))) else {
            return false;
        };
        held(&file)
    }

    fn session(&self, index: &Index, row: &Row) -> Session {
        let transcript = self.transcript(&row.id);
        let head = head(&transcript);
        let updated_at = [row.modified, events::changed(&transcript), head.started]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or_default();
        Session {
            external_id: row.id.clone(),
            cwd: row.cwd.clone(),
            title: Some(row.title.clone())
                .filter(|title| !title.trim().is_empty())
                .or(head.prompt),
            created_at: head.started,
            updated_at,
            transcripts: vec![transcript],
            parent: None,
            app: false,
            sub_agents: self.sub_agents(index, row),
        }
    }

    /// The sub-agents of a conversation, and in turn theirs, each named by the call of its
    /// lead that started it.
    fn sub_agents(&self, index: &Index, row: &Row) -> Vec<SubAgent> {
        let mut found = Vec::new();
        let mut leads = vec![(None, row)];
        for _ in 0..DEPTH {
            let mut next = Vec::new();
            for (lead, lead_row) in leads {
                let children: Vec<&Row> = index
                    .rows
                    .values()
                    .filter(|child| child.parent == lead_row.id)
                    .collect();
                if children.is_empty() {
                    continue;
                }
                let started = self.started(&lead_row.id);
                for child in children {
                    let transcript = self.transcript(&child.id);
                    let head = head(&transcript);
                    let call = started.get(&child.id);
                    found.push(SubAgent {
                        external_id: child.id.clone(),
                        lead: lead.clone(),
                        name: Some(child.agent.clone()).filter(|name| !name.is_empty()),
                        task: head.task,
                        started_at: call.map(|(at, _)| *at).or(head.started),
                        started_by: call.map(|(_, id)| id.clone()),
                        transcripts: vec![transcript],
                    });
                    next.push((Some(child.id.clone()), child));
                }
            }
            leads = next;
        }
        found
    }

    /// The sub-agents that a conversation's calls started: when, and by which call.
    fn started(&self, id: &str) -> BTreeMap<String, (u64, String)> {
        let Ok(events) = events::read(&self.transcript(id), None, None, true) else {
            return BTreeMap::new();
        };
        events
            .events
            .into_iter()
            .filter_map(|event| match event {
                Event::SubAgent {
                    at,
                    external_id,
                    call: Some(call),
                    ..
                } => Some((external_id, (at, call))),
                _ => None,
            })
            .collect()
    }

    fn index(&self) -> Index {
        let (rows, complete) = match self.rows() {
            Ok(rows) => (rows, true),
            Err(_) => (Vec::new(), false),
        };
        let rows: BTreeMap<String, Row> =
            rows.into_iter().map(|row| (row.id.clone(), row)).collect();
        let typed = self.typed(&rows);
        Index {
            rows,
            typed,
            complete,
        }
    }

    /// Every row; none where Antigravity has not made the database yet.
    fn rows(&self) -> rusqlite::Result<Vec<Row>> {
        let path = self.root.join(DATABASE);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let db = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(Duration::from_secs(2))?;
        let mut statement = db.prepare(&format!("select {COLUMNS} from conversation_summaries"))?;
        statement
            .query_map([], |row| {
                let text = |i| {
                    row.get::<_, Option<String>>(i)
                        .map(Option::unwrap_or_default)
                };
                Ok(Row {
                    id: text(0)?,
                    title: text(1)?,
                    modified: sqlite_millis(&text(2)?),
                    cwd: serde_json::from_str::<Vec<String>>(&text(3)?)
                        .ok()
                        .and_then(|uris| uris.first().and_then(|uri| path_of(uri))),
                    parent: text(4)?,
                    agent: text(5)?,
                })
            })?
            .collect()
    }

    /// The conversations that `history.jsonl` says a person typed into.
    fn typed(&self, rows: &BTreeMap<String, Row>) -> HashSet<String> {
        let mut typed = HashSet::new();
        let Ok(file) = File::open(self.root.join(HISTORY)) else {
            return typed;
        };
        let mut firsts = Vec::new();
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(prompt) = serde_json::from_str::<Typed>(&line) else {
                continue;
            };
            match prompt.conversation {
                Some(id) => {
                    typed.insert(id);
                }
                None => firsts.push(prompt),
            }
        }
        if firsts.is_empty() {
            return typed;
        }
        let started: Vec<(&Row, u64)> = rows
            .values()
            .filter(|row| row.parent.is_empty())
            .filter_map(|row| Some((row, head(&self.transcript(&row.id)).started?)))
            .collect();
        for prompt in firsts {
            // The transcript's times are whole seconds; the prompt's are milliseconds.
            let from = prompt.timestamp - prompt.timestamp % 1000;
            let nearest = started
                .iter()
                .filter(|(row, at)| {
                    row.cwd == prompt.workspace && (from..=from + FIRST_STEP).contains(at)
                })
                .min_by_key(|(_, at)| *at);
            if let Some((row, _)) = nearest {
                typed.insert(row.id.clone());
            }
        }
        typed
    }
}

impl Index {
    /// A conversation a person typed into, by its id.
    fn listed(&self, id: &str) -> Result<&Row, Error> {
        self.rows
            .get(id)
            .filter(|row| row.parent.is_empty() && self.typed.contains(id))
            .ok_or_else(|| not_found(id))
    }

    /// The sub-agents' runs under a conversation, to [`DEPTH`].
    fn descendants(&self, id: &str) -> Vec<&Row> {
        let mut found = Vec::new();
        let mut leads = vec![id.to_owned()];
        for _ in 0..DEPTH {
            let children: Vec<&Row> = self
                .rows
                .values()
                .filter(|row| leads.contains(&row.parent))
                .collect();
            leads = children.iter().map(|row| row.id.clone()).collect();
            found.extend(children);
        }
        found
    }
}

/// What the first step of a transcript says: when it came, the person's prompt, or the
/// message a lead sent its sub-agent (`... content=<task>` inside `<SYSTEM_MESSAGE>`).
fn head(transcript: &Path) -> Head {
    let Some(line) = File::open(transcript)
        .ok()
        .and_then(|file| BufReader::new(file).lines().next())
        .and_then(Result::ok)
    else {
        return Head::default();
    };
    let Ok(step) = serde_json::from_str::<Value>(&line) else {
        return Head::default();
    };
    let content = step["content"].as_str().unwrap_or_default();
    let (prompt, task) = match step["type"].as_str() {
        Some("USER_INPUT") => (events::title(content), None),
        Some("SYSTEM_MESSAGE") => (
            None,
            content
                .split_once(" content=")
                .map(|(_, task)| task.split("\n</SYSTEM_MESSAGE>").next().unwrap_or(task))
                .map(|task| task.trim().to_owned())
                .filter(|task| !task.is_empty()),
        ),
        _ => (None, None),
    };
    Head {
        started: step["created_at"].as_str().and_then(events::millis),
        prompt,
        task,
    }
}

/// Whether a process holds a `flock` on `file`.
#[cfg(unix)]
fn held(file: &File) -> bool {
    use std::os::fd::AsRawFd as _;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is open for the call; a lock taken is released before it returns.
    unsafe {
        if libc::flock(fd, libc::LOCK_SH | libc::LOCK_NB) == 0 {
            libc::flock(fd, libc::LOCK_UN);
            false
        } else {
            std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
        }
    }
}

#[cfg(not(unix))]
fn held(_: &File) -> bool {
    false
}

/// The path of a `file://` URI, its `%XX` escapes decoded.
fn path_of(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?.as_bytes();
    let mut bytes = Vec::with_capacity(path.len());
    let mut i = 0;
    while i < path.len() {
        let escaped = (path[i] == b'%')
            .then(|| path.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok());
        match escaped {
            Some(byte) => {
                bytes.push(byte);
                i += 3;
            }
            None => {
                bytes.push(path[i]);
                i += 1;
            }
        }
    }
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
}

/// Milliseconds since 1970 of the database's `2026-11-11 12:34:40.610326+00:00`.
fn sqlite_millis(at: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S%.f%:z").ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}

fn not_found(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("Antigravity keeps no conversation `{id}` that a person typed into here"),
    )
}

#[cfg(test)]
mod tests {
    use super::events::INVOKE_SUBAGENT;
    use super::*;
    use steery_sdk::wire::harness::{Event, TurnState, Voice};

    fn root(scene: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/antigravity")
            .join(scene)
            .join(".gemini/antigravity-cli")
    }

    fn read(store: &Store, id: &str, sub_agent: Option<&str>) -> Events {
        store
            .read(Read {
                external_id: id.into(),
                sub_agent: sub_agent.map(str::to_owned),
                cursor: None,
                tail: None,
            })
            .unwrap()
    }

    fn ends(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::TurnEnded { state, failure, .. } => Some(match failure {
                    Some(failure) => format!("{state:?} {:?}", failure.kind),
                    None => format!("{state:?}"),
                }),
                _ => None,
            })
            .collect()
    }

    fn said(events: &[Event], voice: Voice) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Words { voice: v, text, .. } if *v == voice => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn typed_conversations_list_and_read_as_antigravity_wrote_them() {
        // The terminal's two prompts: the first names only its folder and time.
        let store = Store::new(&root("tui"));
        let listed = store.list(List::default());
        assert!(listed.complete);
        let [session] = listed.sessions.as_slice() else {
            panic!("one conversation: {:?}", listed.sessions);
        };
        assert_eq!(session.external_id, "bafd8265-1a0b-400f-8bc2-857ce6aacee8");
        assert!(
            session
                .cwd
                .as_deref()
                .is_some_and(|cwd| cwd.ends_with("tui/work"))
        );
        let events = read(&store, &session.external_id, None);
        assert_eq!(
            said(&events.events, Voice::Person),
            ["read hello", "second"]
        );
        assert_eq!(ends(&events.events), ["Finished", "Finished"]);

        // A cursor at the end reads nothing again.
        let again = store
            .read(Read {
                external_id: session.external_id.clone(),
                sub_agent: None,
                cursor: Some(events.cursor),
                tail: None,
            })
            .unwrap();
        assert!(again.events.is_empty() && !again.reset);

        // A run with `-p` is no person's: excluded, and not read.
        let store = Store::new(&root("subagent"));
        let listed = store.list(List::default());
        assert!(listed.sessions.is_empty());
        assert_eq!(listed.excluded, ["5fb7d18d-2f7f-4da6-9251-8d7a07ccc995"]);
        assert!(
            store
                .locate(Locate {
                    external_id: listed.excluded[0].clone()
                })
                .is_err()
        );
    }

    #[test]
    fn a_typed_lead_lists_its_sub_agent_and_errors_end_turns() {
        let store = Store::new(&root("tuisub2"));
        let listed = store.list(List::default());
        let [lead] = listed.sessions.as_slice() else {
            panic!("the lead alone: {:?}", listed.sessions);
        };
        let [child] = lead.sub_agents.as_slice() else {
            panic!("one sub-agent: {:?}", lead.sub_agents);
        };
        assert_eq!(child.external_id, "f4c750f6-d77f-4716-a9c9-90f8d6af6d50");
        assert_eq!(child.name.as_deref(), Some("research"));
        assert_eq!(
            child.task.as_deref(),
            Some("Read hello.txt in the current folder and report its text.")
        );
        let lead_events = read(&store, &lead.external_id, None);
        let started = lead_events.events.iter().find_map(|event| match event {
            Event::CallBegan { call, .. } if call.tool == INVOKE_SUBAGENT => Some(call.id.clone()),
            _ => None,
        });
        assert_eq!(child.started_by, started);
        let events = read(&store, &lead.external_id, Some(&child.external_id));
        assert_eq!(ends(&events.events), ["Finished"]);

        // Five attempts at a rate limit, then Antigravity gave up.
        let events = events::read(
            &root("e429")
                .join("brain/e2677a08-c577-4df5-a216-d009b5e6fd55")
                .join(TRANSCRIPT),
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(ends(&events.events), ["Failed RateLimit"]);
        assert!(matches!(
            events.events.last(),
            Some(Event::TurnEnded {
                state: TurnState::Failed,
                ..
            })
        ));
    }
}
