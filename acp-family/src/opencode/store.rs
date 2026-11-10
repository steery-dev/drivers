//! OpenCode 2's conversations, read from its one SQLite file. A conversation is a row of
//! `session_v2`; a sub-agent is a conversation whose `parent_id` names its lead; a fork
//! names its source in `fork_session_id`. Its messages are rows of `session_message`, in
//! `seq` order, an assistant's longer content in `session_part`, attached bytes in `blob`.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use super::events::{Fold, Row, State};

/// The most rows that one read answers.
const BATCH: i64 = 200;

/// The `seq` before every row: rows OpenCode 2 imported from OpenCode 1 start at 0.
const BEFORE: i64 = -1;

/// Where a read stands: the `seq` and id of the last row read, and what the rows so far
/// carry on.
#[derive(Serialize, Deserialize)]
struct Cursor {
    v: u8,
    seq: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default)]
    st: State,
}

impl Cursor {
    fn parse(cursor: &str) -> Result<Self, Error> {
        serde_json::from_str::<Self>(cursor)
            .ok()
            .filter(|cursor| cursor.v == 1)
            .ok_or_else(|| {
                Error::invalid(format!("this driver did not give the cursor `{cursor}`"))
            })
    }

    fn said(&self) -> String {
        serde_json::to_string(self).expect("a cursor is JSON")
    }
}

/// One row of `session_v2`.
struct Conversation {
    id: String,
    parent: Option<String>,
    fork: Option<String>,
    directory: String,
    title: Option<String>,
    agent: Option<String>,
    created: u64,
    updated: u64,
    /// The message where a revert stands: it and every row after it are off the history.
    revert: Option<String>,
}

pub(crate) struct Store<'a> {
    db: &'a Path,
}

impl<'a> Store<'a> {
    pub fn new(db: &'a Path) -> Self {
        Self { db }
    }

    /// The database, read only; none where OpenCode 2 has not made it yet.
    fn open(&self) -> Result<Option<Connection>, Error> {
        if !self.db.exists() {
            return Ok(None);
        }
        let connection = Connection::open_with_flags(
            self.db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| self.unreadable(e))?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(|e| self.unreadable(e))?;
        // OpenCode 1 keeps a database at the same path; only OpenCode 2 has `session_v2`.
        let second: bool = connection
            .query_row(
                "select count(*) from sqlite_master where type = 'table' and name = 'session_v2'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|e| self.unreadable(e))?
            > 0;
        Ok(second.then_some(connection))
    }

    fn unreadable(&self, e: rusqlite::Error) -> Error {
        Error::refused(
            refusal::REFUSED,
            format!("cannot read {}: {e}", self.db.display()),
        )
    }

    /// Every conversation, with when it or any of its messages last changed.
    fn conversations(&self, db: &Connection) -> Result<Vec<Conversation>, Error> {
        let mut statement = db
            .prepare(
                "select s.id, s.parent_id, s.fork_session_id, s.directory, s.title, s.agent,
                        s.time_created,
                        max(s.time_updated,
                            coalesce((select max(m.time_updated) from session_message m
                                      where m.session_id = s.id), 0)),
                        s.revert
                 from session_v2 s",
            )
            .map_err(|e| self.unreadable(e))?;
        let rows = statement
            .query_map([], |row| {
                let revert: Option<String> = row.get(8)?;
                Ok(Conversation {
                    id: row.get(0)?,
                    parent: row.get(1)?,
                    fork: row.get(2)?,
                    directory: row.get(3)?,
                    title: row.get(4)?,
                    agent: row.get(5)?,
                    created: row.get::<_, i64>(6)?.max(0) as u64,
                    updated: row.get::<_, i64>(7)?.max(0) as u64,
                    revert: revert
                        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                        .and_then(|revert| revert["messageID"].as_str().map(str::to_owned)),
                })
            })
            .map_err(|e| self.unreadable(e))?;
        rows.collect::<Result<_, _>>()
            .map_err(|e| self.unreadable(e))
    }

    /// `sessions/list`: each lead conversation; its sub-agents are listed under it.
    pub fn list(&self, list: List) -> Result<Listed, Error> {
        let Some(db) = self.open()? else {
            return Ok(Listed {
                sessions: Vec::new(),
                excluded: Vec::new(),
                complete: true,
                cursor: None,
                after: None,
                gone: Vec::new(),
            });
        };
        let all = self.conversations(&db)?;
        let tree = Tree::new(&all);
        let mut excluded = Vec::new();
        let mut sessions = Vec::new();
        for conversation in &all {
            if conversation.parent.is_some() {
                excluded.push(conversation.id.clone());
                continue;
            }
            let session = tree.session(conversation);
            if list
                .seen_since
                .is_none_or(|since| session.updated_at >= since)
            {
                sessions.push(session);
            }
        }
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        Ok(Listed {
            sessions,
            excluded,
            complete: true,
            cursor: None,
            after: None,
            gone: Vec::new(),
        })
    }

    /// `sessions/locate`.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let db = self.open()?.ok_or_else(|| not_found(&locate.external_id))?;
        let all = self.conversations(&db)?;
        let tree = Tree::new(&all);
        let conversation = tree
            .by_id
            .get(locate.external_id.as_str())
            .filter(|conversation| conversation.parent.is_none())
            .ok_or_else(|| not_found(&locate.external_id))?;
        Ok(Located {
            session: tree.session(conversation),
        })
    }

    /// `sessions/read`: the events of a conversation, or of one of its sub-agents. `servers`
    /// names the MCP servers that OpenCode loads in a folder, to tell their tools apart.
    pub fn read(
        &self,
        read: Read,
        servers: impl FnOnce(&Path) -> Vec<String>,
    ) -> Result<Events, Error> {
        let db = self.open()?.ok_or_else(|| not_found(&read.external_id))?;
        let all = self.conversations(&db)?;
        let tree = Tree::new(&all);
        let lead = tree
            .by_id
            .get(read.external_id.as_str())
            .filter(|conversation| conversation.parent.is_none())
            .ok_or_else(|| not_found(&read.external_id))?;
        let conversation = match read.sub_agent.as_deref() {
            None => *lead,
            Some(child) => tree
                .by_id
                .get(child)
                .filter(|found| tree.lead_of(found) == Some(lead.id.as_str()))
                .copied()
                .ok_or_else(|| not_found(child))?,
        };
        let id = conversation.id.as_str();
        // The first row off the history, where a revert stands.
        let end: i64 = match &conversation.revert {
            Some(message) => db
                .query_row(
                    "select seq from session_message where session_id = ?1 and id = ?2",
                    params![id, message],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| self.unreadable(e))?
                .unwrap_or(i64::MAX),
            None => i64::MAX,
        };
        let mut reset = false;
        let (from, state, tailed) = match (read.tail, read.cursor.as_deref()) {
            (Some(tail), _) => (self.tail_start(&db, id, end, tail)?, State::default(), true),
            (None, Some(cursor)) => {
                let cursor = Cursor::parse(cursor)?;
                let held = match &cursor.id {
                    None => cursor.seq == BEFORE,
                    Some(row) => {
                        db.query_row(
                            "select count(*) from session_message
                             where session_id = ?1 and seq = ?2 and id = ?3",
                            params![id, cursor.seq, row],
                            |found| found.get::<_, i64>(0),
                        )
                        .map_err(|e| self.unreadable(e))?
                            > 0
                    }
                };
                if held && cursor.seq < end {
                    (cursor.seq, cursor.st, false)
                } else {
                    reset = true;
                    (BEFORE, State::default(), false)
                }
            }
            (None, None) => (BEFORE, State::default(), false),
        };
        // Rows at or before the newest `idle` row are settled, whatever they say.
        let settled: i64 = db
            .query_row(
                "select coalesce(max(seq), -1) from session_message
                 where session_id = ?1 and type = 'idle' and seq < ?2",
                params![id, end],
                |row| row.get(0),
            )
            .map_err(|e| self.unreadable(e))?;
        let rows = self.rows(&db, id, from, end)?;
        let servers = servers(Path::new(&lead.directory));
        let mut fold = Fold {
            fork: conversation
                .fork
                .as_deref()
                .map(|fork| (fork, conversation.created)),
            state,
            events: Vec::new(),
            servers: &servers,
        };
        let more = rows.len() as i64 == BATCH;
        let mut last: Option<(i64, String)> = None;
        let mut stopped = false;
        for row in &rows {
            if row.seq > settled && row.open() {
                stopped = true;
                break;
            }
            fold.row(row);
            last = Some((row.seq, row.id.clone()));
        }
        if !more && !stopped {
            fold.end_imported();
            if let Some((parent, at)) = fold.fork {
                fold.forked(parent, at);
            }
        }
        let (seq, row) = match last {
            Some((seq, row)) => (seq, Some(row)),
            None if from == BEFORE => (BEFORE, None),
            None => (from, self.row_id(&db, id, from)?),
        };
        Ok(Events {
            cursor: Cursor {
                v: 1,
                seq,
                id: row.clone(),
                st: fold.state,
            }
            .said(),
            events: fold.events,
            more: more && !stopped,
            reset,
            whole: from == BEFORE,
            after: if tailed { None } else { row },
        })
    }

    fn row_id(&self, db: &Connection, session: &str, seq: i64) -> Result<Option<String>, Error> {
        db.query_row(
            "select id from session_message where session_id = ?1 and seq = ?2",
            params![session, seq],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| self.unreadable(e))
    }

    /// The `seq` to read after, so that the rows read hold about the newest `tail` bytes.
    fn tail_start(
        &self,
        db: &Connection,
        session: &str,
        end: i64,
        tail: u64,
    ) -> Result<i64, Error> {
        let mut statement = db
            .prepare(
                "select seq, length(data) from session_message
                 where session_id = ?1 and seq < ?2 order by seq desc",
            )
            .map_err(|e| self.unreadable(e))?;
        let mut rows = statement
            .query(params![session, end])
            .map_err(|e| self.unreadable(e))?;
        let mut total = 0u64;
        let mut start = BEFORE;
        let mut count = 0;
        while let Some(row) = rows.next().map_err(|e| self.unreadable(e))? {
            let seq: i64 = row.get(0).map_err(|e| self.unreadable(e))?;
            let length: i64 = row.get(1).map_err(|e| self.unreadable(e))?;
            total += length.max(0) as u64;
            count += 1;
            if total > tail || count >= BATCH {
                return Ok(seq);
            }
            start = seq - 1;
        }
        Ok(start)
    }

    /// The rows after `from` and before `end`, their parts and blobs joined.
    fn rows(&self, db: &Connection, session: &str, from: i64, end: i64) -> Result<Vec<Row>, Error> {
        // OpenCode 2's first run copies OpenCode 1's messages, ids kept, and leaves `message`.
        let v1: bool = db
            .query_row(
                "select count(*) > 0 from sqlite_master where type = 'table' and name = 'message'",
                [],
                |found| found.get(0),
            )
            .map_err(|e| self.unreadable(e))?;
        let imported = if v1 {
            "exists(select 1 from message where message.id = session_message.id)"
        } else {
            "0"
        };
        let mut statement = db
            .prepare(&format!(
                "select seq, id, type, time_created, data, {imported} from session_message
                 where session_id = ?1 and seq > ?2 and seq < ?3 order by seq limit ?4"
            ))
            .map_err(|e| self.unreadable(e))?;
        let rows = statement
            .query_map(params![session, from, end, BATCH], |row| {
                let data: String = row.get(4)?;
                Ok(Row {
                    seq: row.get(0)?,
                    id: row.get(1)?,
                    kind: row.get(2)?,
                    created: row.get::<_, i64>(3)?.max(0) as u64,
                    data: serde_json::from_str(&data).unwrap_or(Value::Null),
                    blobs: Vec::new(),
                    imported: row.get(5)?,
                })
            })
            .map_err(|e| self.unreadable(e))?;
        let mut rows: Vec<Row> = rows
            .collect::<Result<_, _>>()
            .map_err(|e| self.unreadable(e))?;
        let mut parts = db
            .prepare("select data from session_part where message_id = ?1 order by ordinal")
            .map_err(|e| self.unreadable(e))?;
        let mut blob = db
            .prepare("select data from blob where hash = ?1")
            .map_err(|e| self.unreadable(e))?;
        for row in &mut rows {
            match row.kind.as_str() {
                // As OpenCode reads it: the parts first, then any content in the row.
                "assistant" => {
                    let stored: Vec<Value> = parts
                        .query_map(params![row.id], |part| part.get::<_, String>(0))
                        .map_err(|e| self.unreadable(e))?
                        .filter_map(Result::ok)
                        .filter_map(|text| serde_json::from_str(&text).ok())
                        .collect();
                    if !stored.is_empty() {
                        let inline = row.data["content"].as_array().cloned().unwrap_or_default();
                        row.data["content"] =
                            Value::Array(stored.into_iter().chain(inline).collect());
                    }
                }
                "user" => {
                    let hashes: Vec<String> = row.data["files"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|file| file["uri"].as_str()?.strip_prefix("media:"))
                        .map(str::to_owned)
                        .collect();
                    for hash in hashes {
                        let bytes: Option<Vec<u8>> = blob
                            .query_row(params![hash], |found| found.get(0))
                            .optional()
                            .map_err(|e| self.unreadable(e))?;
                        if let Some(bytes) = bytes {
                            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                            row.blobs.push((hash, data));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(rows)
    }
}

fn not_found(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("OpenCode 2 has no conversation `{id}` here"),
    )
}

/// The conversations by id, and each one's sub-agents.
struct Tree<'a> {
    by_id: HashMap<&'a str, &'a Conversation>,
    children: HashMap<&'a str, Vec<&'a Conversation>>,
}

impl<'a> Tree<'a> {
    fn new(all: &'a [Conversation]) -> Self {
        let by_id = all.iter().map(|c| (c.id.as_str(), c)).collect();
        let mut children: HashMap<&str, Vec<&Conversation>> = HashMap::new();
        for conversation in all {
            if let Some(parent) = &conversation.parent {
                children
                    .entry(parent.as_str())
                    .or_default()
                    .push(conversation);
            }
        }
        Self { by_id, children }
    }

    /// The lead conversation above a sub-agent; none for a lead itself.
    fn lead_of(&self, conversation: &Conversation) -> Option<&'a str> {
        let mut parent = conversation.parent.as_deref()?;
        // A cycle cannot come from OpenCode; the bound keeps a broken file from hanging.
        for _ in 0..64 {
            let above = self.by_id.get(parent)?;
            match above.parent.as_deref() {
                Some(next) => parent = next,
                None => return Some(above.id.as_str()),
            }
        }
        None
    }

    fn session(&self, lead: &Conversation) -> Session {
        let mut sub_agents = Vec::new();
        let mut updated = lead.updated;
        let mut stack: Vec<(&Conversation, Option<&str>)> = self
            .children
            .get(lead.id.as_str())
            .into_iter()
            .flatten()
            .map(|child| (*child, None))
            .collect();
        while let Some((child, above)) = stack.pop() {
            updated = updated.max(child.updated);
            sub_agents.push(SubAgent {
                external_id: child.id.clone(),
                lead: above.map(str::to_owned),
                name: child.agent.clone(),
                task: child
                    .title
                    .as_deref()
                    .and_then(|title| title.lines().next())
                    .map(str::to_owned),
                started_at: Some(child.created),
                started_by: None,
                transcripts: Vec::new(),
            });
            for grandchild in self.children.get(child.id.as_str()).into_iter().flatten() {
                stack.push((*grandchild, Some(child.id.as_str())));
            }
        }
        sub_agents.sort_by_key(|agent| agent.started_at);
        Session {
            external_id: lead.id.clone(),
            cwd: Some(lead.directory.clone().into()),
            title: lead.title.clone(),
            created_at: Some(lead.created),
            updated_at: updated,
            transcripts: Vec::new(),
            parent: lead.fork.clone(),
            app: false,
            sub_agents,
        }
    }
}
