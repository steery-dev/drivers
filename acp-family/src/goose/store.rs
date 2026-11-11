//! Goose's conversations: one SQLite file, `sessions` a row each and `messages` a row of
//! each message. A sub-agent names its lead in `parent_session_id`; a `hidden` conversation
//! is Goose's own, such as `/doctor`'s. Goose writes a conversation's messages anew whole
//! where it changes one (`replace_conversation`), so a message keeps its `message_id` but
//! not its row: a read stands at a count of messages and the id of the last.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use super::events::{Fold, Message, State};

/// The most messages that one read answers.
const BATCH: usize = 200;

/// The columns of a row, in [`Row::of`]'s order.
const COLUMNS: &str = "id, name, session_type, working_dir, created_at, updated_at, \
    parent_session_id";

/// Where a read stands: how many messages it read, the id of the last, and what they carry
/// on.
#[derive(Serialize, Deserialize)]
struct Cursor {
    v: u8,
    n: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    st: State,
}

/// One row of `sessions`.
pub(crate) struct Row {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub cwd: PathBuf,
    pub created: Option<u64>,
    pub updated: u64,
    pub parent: Option<String>,
}

impl Row {
    fn of(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        let created: Option<String> = row.get(4)?;
        let updated: Option<String> = row.get(5)?;
        let created = created.as_deref().and_then(millis);
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
            cwd: PathBuf::from(row.get::<_, String>(3)?),
            created,
            updated: updated.as_deref().and_then(millis).or(created).unwrap_or(0),
            parent: row.get::<_, Option<String>>(6)?.filter(|p| !p.is_empty()),
        })
    }

    /// Goose's own, never the person's.
    fn hidden(&self) -> bool {
        self.kind == "hidden"
    }

    /// A sub-agent, not a conversation of its own.
    fn is_sub_agent(&self) -> bool {
        self.parent.is_some()
    }

    fn title(&self) -> Option<String> {
        let name = self.name.trim();
        (!name.is_empty()).then(|| name.to_owned())
    }

    fn session(&self, sub_agents: Vec<SubAgent>) -> Session {
        Session {
            external_id: self.id.clone(),
            cwd: Some(self.cwd.clone()),
            title: self.title(),
            created_at: self.created,
            updated_at: self.updated,
            transcripts: Vec::new(),
            parent: None,
            app: false,
            sub_agents,
        }
    }

    fn sub_agent(&self) -> SubAgent {
        SubAgent {
            external_id: self.id.clone(),
            lead: self.parent.clone(),
            name: None,
            task: self.title(),
            started_at: self.created,
            started_by: None,
            transcripts: Vec::new(),
        }
    }
}

/// Milliseconds since 1970 of Goose's `YYYY-MM-DD HH:MM:SS`, in UTC.
fn millis(text: &str) -> Option<u64> {
    chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .and_then(|at| u64::try_from(at.and_utc().timestamp_millis()).ok())
}

pub(crate) struct Store<'a> {
    db: &'a Path,
}

impl<'a> Store<'a> {
    pub fn new(db: &'a Path) -> Self {
        Self { db }
    }

    /// The database, read only; none where Goose has not made it yet.
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
        Ok(Some(connection))
    }

    fn unreadable(&self, e: rusqlite::Error) -> Error {
        Error::refused(
            refusal::REFUSED,
            format!("cannot read {}: {e}", self.db.display()),
        )
    }

    /// Every row but Goose's own.
    fn rows(&self) -> Result<Vec<Row>, Error> {
        let Some(db) = self.open()? else {
            return Ok(Vec::new());
        };
        let mut statement = db
            .prepare(&format!(
                "select {COLUMNS} from sessions where session_type != 'hidden' \
                 order by created_at, id"
            ))
            .map_err(|e| self.unreadable(e))?;
        statement
            .query_map([], Row::of)
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(|e| self.unreadable(e))
    }

    /// One row, where Goose has it and it is not Goose's own.
    pub fn row(&self, id: &str) -> Result<Option<Row>, Error> {
        let Some(db) = self.open()? else {
            return Ok(None);
        };
        let row = db
            .query_row(
                &format!("select {COLUMNS} from sessions where id = ?1"),
                params![id],
                Row::of,
            )
            .optional()
            .map_err(|e| self.unreadable(e))?;
        Ok(row.filter(|row| !row.hidden()))
    }

    /// One row, refused where Goose has none.
    pub fn found(&self, id: &str) -> Result<Row, Error> {
        self.row(id)?.ok_or_else(|| not_found(id))
    }

    /// The folder a conversation runs in.
    #[expect(dead_code, reason = "Drive's `terminal` reads it")]
    pub fn directory(&self, id: &str) -> Result<Option<PathBuf>, Error> {
        Ok(self.row(id)?.map(|row| row.cwd))
    }

    /// The sub-agents of `lead`, theirs among them.
    fn sub_agents(rows: &[Row], lead: &str) -> (Vec<SubAgent>, u64) {
        let mut sub_agents = Vec::new();
        let mut updated = 0;
        let mut stack = vec![lead];
        while let Some(parent) = stack.pop() {
            for child in rows
                .iter()
                .filter(|row| row.parent.as_deref() == Some(parent))
            {
                updated = updated.max(child.updated);
                sub_agents.push(child.sub_agent());
                stack.push(&child.id);
            }
        }
        (sub_agents, updated)
    }

    /// `sessions/list`: each conversation, its sub-agents within it. Goose keeps no
    /// transcript of a conversation apart from the database.
    pub fn list(&self, asked: List) -> Result<Listed, Error> {
        let rows = self.rows()?;
        let mut sessions = Vec::new();
        for lead in rows.iter().filter(|row| !row.is_sub_agent()) {
            let (sub_agents, updated) = Self::sub_agents(&rows, &lead.id);
            let updated = updated.max(lead.updated);
            if asked.seen_since.is_some_and(|since| updated < since) {
                continue;
            }
            let mut session = lead.session(sub_agents);
            session.updated_at = updated;
            sessions.push(session);
        }
        Ok(Listed {
            sessions,
            excluded: Vec::new(),
            complete: true,
            cursor: None,
            after: None,
            gone: Vec::new(),
        })
    }

    /// `sessions/locate`: the conversation, or the lead of the sub-agent named.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let mut row = self.found(&locate.external_id)?;
        while let Some(parent) = row.parent.clone() {
            row = self.found(&parent)?;
        }
        let (sub_agents, _) = Self::sub_agents(&self.rows()?, &row.id);
        Ok(Located {
            session: row.session(sub_agents),
        })
    }

    /// A conversation's messages, in Goose's order.
    pub fn messages(&self, id: &str) -> Result<Vec<Message>, Error> {
        let Some(db) = self.open()? else {
            return Ok(Vec::new());
        };
        let mut statement = db
            .prepare(
                "select id, message_id, role, created_timestamp, content_json, metadata_json \
                 from messages where session_id = ?1 order by id",
            )
            .map_err(|e| self.unreadable(e))?;
        let json = |text: Option<String>| {
            text.and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .unwrap_or(Value::Null)
        };
        statement
            .query_map(params![id], |row| {
                let message_id: Option<String> = row.get(1)?;
                let at: i64 = row.get(3)?;
                Ok(Message {
                    id: message_id
                        .filter(|id| !id.is_empty())
                        .unwrap_or_else(|| format!("row-{}", row.get::<_, i64>(0).unwrap_or(0))),
                    role: row.get(2)?,
                    at: u64::try_from(at).unwrap_or(0) * 1000,
                    content: json(row.get(4)?),
                    metadata: json(row.get(5)?),
                })
            })
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(|e| self.unreadable(e))
    }

    /// `sessions/read`: the conversation's events after the cursor, or a sub-agent's.
    /// `servers` names the MCP servers Goose loads.
    pub fn read(&self, read: Read, servers: impl FnOnce() -> Vec<String>) -> Result<Events, Error> {
        let row = match &read.sub_agent {
            Some(sub_agent) => {
                let row = self.found(sub_agent)?;
                let mut lead = row.parent.clone();
                while let Some(parent) = lead.as_deref() {
                    if parent == read.external_id {
                        break;
                    }
                    lead = self.found(parent)?.parent;
                }
                if lead.is_none() {
                    return Err(not_found(sub_agent));
                }
                row
            }
            None => self.found(&read.external_id)?,
        };
        let messages = self.messages(&row.id)?;
        let id_at = |n: usize| -> Option<String> {
            n.checked_sub(1)
                .and_then(|last| messages.get(last))
                .map(|message| message.id.clone())
        };
        let cursor = read
            .cursor
            .as_deref()
            .map(|cursor| serde_json::from_str::<Cursor>(cursor).ok());
        let (from, state, reset) = match cursor {
            Some(Some(cursor))
                if cursor.v == 1 && cursor.n <= messages.len() && id_at(cursor.n) == cursor.id =>
            {
                (cursor.n, cursor.st, false)
            }
            Some(_) => (0, State::default(), true),
            None => (tail_start(&messages, read.tail), State::default(), false),
        };
        let tailed = read.cursor.is_none() && from > 0;
        let end = messages.len().min(from + BATCH);
        let more = end < messages.len();
        let servers = servers();
        let mut fold = Fold::new(state, &servers);
        for message in &messages[from..end] {
            fold.message(message);
        }
        if !more {
            fold.settle();
        }
        let after = (!tailed).then(|| id_at(end)).flatten();
        let cursor = Cursor {
            v: 1,
            n: end,
            id: id_at(end),
            st: fold.state,
        };
        Ok(Events {
            cursor: serde_json::to_string(&cursor).unwrap_or_default(),
            events: fold.events,
            more,
            reset,
            whole: from == 0,
            after,
        })
    }

    /// The person's words in a conversation's messages from the `mark`th on.
    #[cfg_attr(not(test), expect(dead_code, reason = "Drive's receipts read it"))]
    pub fn said_after(&self, id: &str, mark: &str) -> Result<Vec<String>, Error> {
        let from: usize = mark
            .parse()
            .map_err(|_| Error::invalid(format!("`{mark}` is no mark of this driver's")))?;
        self.found(id)?;
        Ok(self
            .messages(id)?
            .iter()
            .skip(from)
            .filter_map(Message::said)
            .collect())
    }

    /// How many messages the conversation has: the mark that the next words come after.
    #[cfg_attr(not(test), expect(dead_code, reason = "Drive's receipts read it"))]
    pub fn count(&self, id: &str) -> Result<usize, Error> {
        self.found(id)?;
        Ok(self.messages(id)?.len())
    }
}

/// The first message within `tail` bytes of the end that a person's words begin, else the
/// first after the bound.
fn tail_start(messages: &[Message], tail: Option<u64>) -> usize {
    let Some(tail) = tail else {
        return 0;
    };
    let mut bytes = 0u64;
    let mut start = messages.len();
    for (index, message) in messages.iter().enumerate().rev() {
        bytes += message.content.to_string().len() as u64;
        if bytes > tail {
            break;
        }
        start = index;
    }
    (start..messages.len())
        .find(|&index| messages[index].begins())
        .unwrap_or(start)
}

fn not_found(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("Goose has no conversation `{id}` here"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database of Goose's shape, with the rows given.
    fn db(name: &str, sql: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("goose-{name}-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let db = Connection::open(&path).unwrap();
        db.execute_batch(&format!(
            "create table sessions (id text primary key, name text not null default '', \
             session_type text not null default 'user', working_dir text not null, \
             created_at timestamp, updated_at timestamp, parent_session_id text);
             create table messages (id integer primary key autoincrement, message_id text, \
             session_id text not null, role text not null, content_json text not null, \
             created_timestamp integer not null, metadata_json text);
             {sql}"
        ))
        .unwrap();
        path
    }

    const ROWS: &str = "insert into sessions values
        ('a', 'Lead', 'user', '/w', '2026-11-11 09:00:00', '2026-11-11 09:01:00', null),
        ('b', 'Child', 'sub_agent', '/w', '2026-11-11 09:00:10', '2026-11-11 09:05:00', 'a'),
        ('d', '/doctor', 'hidden', '/w', '2026-11-11 09:00:20', '2026-11-11 09:00:30', null);
        insert into messages (message_id, session_id, role, content_json, created_timestamp, metadata_json) values
        ('m1', 'a', 'user', '[{\"type\":\"text\",\"text\":\"hi\"}]', 1, '{\"userVisible\":true}'),
        ('m2', 'a', 'user', '[{\"type\":\"text\",\"text\":\"<turn-context>x</turn-context>\"}]', 1, '{\"userVisible\":false}'),
        ('m3', 'a', 'assistant', '[{\"type\":\"toolRequest\",\"id\":\"t1\",\"toolCall\":{\"status\":\"success\",\"value\":{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}}}]', 2, '{\"userVisible\":true}'),
        ('m4', 'a', 'user', '[{\"type\":\"toolResponse\",\"id\":\"t1\",\"toolResult\":{\"status\":\"success\",\"value\":{\"content\":[{\"type\":\"text\",\"text\":\"file\"}],\"isError\":false}}}]', 3, '{\"userVisible\":true}'),
        ('m5', 'a', 'assistant', '[{\"type\":\"text\",\"text\":\"done\"}]', 4, '{\"userVisible\":true}'),
        ('s1', 'a', 'user', '[{\"type\":\"text\",\"text\":\"also\"}]', 4, '{\"userVisible\":true,\"steer\":true}'),
        ('m6', 'a', 'assistant', '[{\"type\":\"text\",\"text\":\"done also\"}]', 5, '{\"userVisible\":true}');";

    #[test]
    fn lists_leads_with_sub_agents_and_hides_goose_own() {
        let path = db("list", ROWS);
        let listed = Store::new(&path)
            .list(List {
                seen_since: None,
                transcripts: Vec::new(),
                after: None,
            })
            .unwrap();
        assert_eq!(listed.sessions.len(), 1);
        let lead = &listed.sessions[0];
        assert_eq!(lead.external_id, "a");
        assert_eq!(lead.title.as_deref(), Some("Lead"));
        assert_eq!(lead.sub_agents[0].external_id, "b");
        assert_eq!(lead.updated_at, millis("2026-11-11 09:05:00").unwrap());
        assert!(Store::new(&path).row("d").unwrap().is_none());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reads_one_turn_with_its_steered_words() {
        let path = db("read", ROWS);
        let store = Store::new(&path);
        let read = |cursor: Option<String>| {
            store
                .read(
                    Read {
                        external_id: "a".into(),
                        sub_agent: None,
                        cursor,
                        tail: None,
                    },
                    Vec::new,
                )
                .unwrap()
        };
        let first = read(None);
        let events = serde_json::to_value(&first.events).unwrap();
        let types: Vec<&str> = events
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            [
                "turn_began",
                "words",
                "call_began",
                "call_ended",
                "words",
                "words",
                "words",
                "words",
                "turn_ended"
            ]
        );
        assert!(!events.to_string().contains("turn-context"));
        assert_eq!(events[4]["voice"], "tool");
        assert_eq!(events[6]["text"], "also");
        assert_eq!(events[8]["state"], "finished");
        assert_eq!(store.count("a").unwrap(), 7);
        assert_eq!(store.said_after("a", "1").unwrap(), ["also"]);
        let again = read(Some(first.cursor.clone()));
        assert!(again.events.is_empty() && !again.reset);
        // Goose writes the conversation anew without its last message: the cursor's id no
        // longer matches, and the read starts again.
        Connection::open(&path)
            .unwrap()
            .execute("delete from messages where message_id = 'm6'", [])
            .unwrap();
        assert!(read(Some(first.cursor)).reset);
        std::fs::remove_file(&path).unwrap();
    }
}
