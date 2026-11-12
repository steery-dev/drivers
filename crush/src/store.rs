//! Crush's sessions: one SQLite store per project, `<project>/.crush/crush.db`, which
//! `projects.json` lists. Table `sessions` has a row of each session; a sub-agent names its
//! lead in `parent_session_id`. A session whose id begins `title-` is Crush's own: the one
//! that names its lead. Table `messages` has a row of each message, its `parts` a JSON array
//! of parts tagged `type`. Crush writes an answer's row while the model streams it and
//! gives it a `finish` part at its end, so a read stops before an answer not yet finished.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use crate::events::{Fold, Message, State};
use crate::home::{self, Project};

/// The most messages that one read answers.
const BATCH: usize = 200;

/// The title Crush gives a session before it names it.
const UNTITLED: &str = "Untitled Session";

/// The columns of a row, in [`Row::of`]'s order.
const COLUMNS: &str = "id, parent_session_id, title, created_at, updated_at, prompt_tokens, \
    completion_tokens";

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

/// One row of `sessions`, with the project it is in.
pub(crate) struct Row {
    pub id: String,
    pub parent: Option<String>,
    title: String,
    created: u64,
    updated: u64,
    /// The newest response's tokens, as Crush keeps them: what it read, what it wrote.
    pub tokens: (u64, u64),
    pub cwd: PathBuf,
}

/// Milliseconds of Crush's seconds since 1970.
fn millis(seconds: i64) -> u64 {
    u64::try_from(seconds).unwrap_or(0) * 1000
}

impl Row {
    fn of(cwd: &Path) -> impl Fn(&rusqlite::Row) -> rusqlite::Result<Self> + '_ {
        move |row| {
            let count = |index| {
                row.get::<_, i64>(index)
                    .map(|n| u64::try_from(n).unwrap_or(0))
            };
            Ok(Self {
                id: row.get(0)?,
                parent: row.get::<_, Option<String>>(1)?.filter(|p| !p.is_empty()),
                title: row.get(2)?,
                created: millis(row.get(3)?),
                updated: millis(row.get(4)?),
                tokens: (count(5)?, count(6)?),
                cwd: cwd.to_owned(),
            })
        }
    }

    /// Crush's own, never the person's.
    fn own(&self) -> bool {
        self.id.starts_with("title-")
    }

    fn title(&self) -> Option<String> {
        let title = self.title.trim();
        (!title.is_empty() && title != UNTITLED).then(|| title.to_owned())
    }

    fn session(&self, sub_agents: Vec<SubAgent>) -> Session {
        Session {
            external_id: self.id.clone(),
            cwd: Some(self.cwd.clone()),
            title: self.title(),
            created_at: Some(self.created),
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
            started_at: Some(self.created),
            started_by: None,
            transcripts: Vec::new(),
        }
    }
}

/// One project's store.
struct Db<'a> {
    project: &'a Project,
}

impl Db<'_> {
    /// The store, read only; none where Crush has not made it.
    fn open(&self) -> Result<Option<Connection>, Error> {
        let db = self.project.db();
        if !db.exists() {
            return Ok(None);
        }
        let connection = Connection::open_with_flags(
            &db,
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
            format!("cannot read {}: {e}", self.project.db().display()),
        )
    }

    /// Every row but Crush's own.
    fn rows(&self) -> Result<Vec<Row>, Error> {
        let Some(db) = self.open()? else {
            return Ok(Vec::new());
        };
        let mut statement = db
            .prepare(&format!(
                "select {COLUMNS} from sessions order by created_at, id"
            ))
            .map_err(|e| self.unreadable(e))?;
        let rows = statement
            .query_map([], Row::of(&self.project.path))
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(|e| self.unreadable(e))?;
        Ok(rows.into_iter().filter(|row| !row.own()).collect())
    }

    /// One row, where this store has it and it is not Crush's own.
    fn row(&self, id: &str) -> Result<Option<Row>, Error> {
        let Some(db) = self.open()? else {
            return Ok(None);
        };
        let row = db
            .query_row(
                &format!("select {COLUMNS} from sessions where id = ?1"),
                params![id],
                Row::of(&self.project.path),
            )
            .optional()
            .map_err(|e| self.unreadable(e))?;
        Ok(row.filter(|row| !row.own()))
    }

    /// A session's messages, in the order Crush wrote them.
    fn messages(&self, id: &str) -> Result<Vec<Message>, Error> {
        let Some(db) = self.open()? else {
            return Ok(Vec::new());
        };
        let mut statement = db
            .prepare(
                "select id, role, parts, model, provider, created_at, is_summary_message \
                 from messages where session_id = ?1 order by created_at, rowid",
            )
            .map_err(|e| self.unreadable(e))?;
        statement
            .query_map(params![id], |row| {
                let parts: String = row.get(2)?;
                Ok(Message {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    parts: serde_json::from_str::<Value>(&parts).unwrap_or(Value::Null),
                    model: row.get::<_, Option<String>>(3)?.filter(|m| !m.is_empty()),
                    provider: row.get::<_, Option<String>>(4)?.filter(|p| !p.is_empty()),
                    at: millis(row.get(5)?),
                    summary: row.get::<_, i64>(6)? != 0,
                })
            })
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(|e| self.unreadable(e))
    }
}

/// The sub-agents of `lead` within `rows`, theirs among them, and the newest time of any.
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

/// Every project's store, as `projects.json` lists them.
pub(crate) struct Store {
    projects: Vec<Project>,
}

impl Store {
    pub fn new(home: &Path) -> Self {
        Self {
            projects: home::projects(home),
        }
    }

    fn dbs(&self) -> impl Iterator<Item = Db<'_>> {
        self.projects.iter().map(|project| Db { project })
    }

    /// The row of `id` and the store that has it.
    fn find(&self, id: &str) -> Result<(Db<'_>, Row), Error> {
        for db in self.dbs() {
            if let Some(row) = db.row(id)? {
                return Ok((db, row));
            }
        }
        Err(not_found(id))
    }

    /// `sessions/list`: each session of each project, its sub-agents within it.
    pub fn list(&self, asked: List) -> Result<Listed, Error> {
        let mut sessions = Vec::new();
        for db in self.dbs() {
            let rows = db.rows()?;
            for lead in rows.iter().filter(|row| row.parent.is_none()) {
                let (sub_agents, updated) = sub_agents(&rows, &lead.id);
                let updated = updated.max(lead.updated);
                if asked.seen_since.is_some_and(|since| updated < since) {
                    continue;
                }
                let mut session = lead.session(sub_agents);
                session.updated_at = updated;
                sessions.push(session);
            }
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

    /// `sessions/locate`: the session, or the lead of the sub-agent named.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let (db, mut row) = self.find(&locate.external_id)?;
        while let Some(parent) = row.parent.clone() {
            row = db.row(&parent)?.ok_or_else(|| not_found(&parent))?;
        }
        let (sub_agents, _) = sub_agents(&db.rows()?, &row.id);
        Ok(Located {
            session: row.session(sub_agents),
        })
    }

    /// The store that keeps `id`, a lead's or a sub-agent's: the file a Crush that has the
    /// session's project open holds.
    pub fn transcript(&self, id: &str) -> Result<PathBuf, Error> {
        let (db, _) = self.find(id)?;
        Ok(db.project.db())
    }

    /// The folder of the project that keeps `id`.
    pub fn place(&self, id: &str) -> Result<PathBuf, Error> {
        Ok(self.find(id)?.1.cwd)
    }

    /// The words of each of the person's messages in `id`, in order.
    pub fn said(&self, id: &str) -> Result<Vec<String>, Error> {
        let (db, row) = self.find(id)?;
        Ok(db
            .messages(&row.id)?
            .iter()
            .filter(|message| message.begins())
            .map(Message::said)
            .collect())
    }

    /// `sessions/read`: the session's events after the cursor, or a sub-agent's.
    /// `servers` names the MCP servers Crush loads in the session's folder.
    pub fn read(
        &self,
        read: Read,
        servers: impl FnOnce(&Path) -> Vec<String>,
    ) -> Result<Events, Error> {
        let (db, lead) = self.find(&read.external_id)?;
        let row = match &read.sub_agent {
            Some(sub_agent) => {
                let row = db.row(sub_agent)?.ok_or_else(|| not_found(sub_agent))?;
                let mut parent = row.parent.clone();
                while let Some(id) = parent.as_deref() {
                    if id == lead.id {
                        break;
                    }
                    parent = db.row(id)?.and_then(|row| row.parent);
                }
                if parent.is_none() {
                    return Err(not_found(sub_agent));
                }
                row
            }
            None => lead,
        };
        let messages = db.messages(&row.id)?;
        // An answer the model still streams, and every message after it, wait for a later
        // read.
        let ready = messages
            .iter()
            .position(|message| !message.finished())
            .unwrap_or(messages.len());
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
                if cursor.v == 1 && cursor.n <= ready && id_at(cursor.n) == cursor.id =>
            {
                (cursor.n, cursor.st, false)
            }
            Some(_) => (0, State::default(), true),
            None => (
                tail_start(&messages[..ready], read.tail),
                State::default(),
                false,
            ),
        };
        let tailed = read.cursor.is_none() && from > 0;
        let end = ready.min(from + BATCH);
        let more = end < ready;
        let servers = servers(&row.cwd);
        let children: Vec<String> = db
            .rows()?
            .into_iter()
            .filter(|child| child.parent.as_deref() == Some(row.id.as_str()))
            .map(|child| child.id)
            .collect();
        let mut fold = Fold::new(state, &servers, &children);
        for message in &messages[from..end] {
            fold.message(message);
        }
        if end == messages.len() {
            fold.tokens(row.tokens);
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
        bytes += message.parts.to_string().len() as u64;
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
        format!("Crush has no session `{id}` here"),
    )
}
