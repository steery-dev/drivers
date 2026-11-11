//! Cline's conversations. Its SQLite file lists them, a row of `sessions` each: a sub-agent
//! names its lead in `parent_session_id`, `status` says whether a turn runs, and `pid` the
//! process that runs it. Each one's messages are one JSON file, `messages_path`, which Cline
//! writes whole.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_sdk::wire::harness::{
    Events, List, Listed, Locate, Located, Read, Session, SubAgent, TurnState,
};
use steery_sdk::{Error, refusal};

use super::events::{Fold, State, person};

/// The most messages that one read answers.
const BATCH: usize = 200;

/// The columns of a row, in [`Row::of`]'s order.
const COLUMNS: &str = "session_id, pid, started_at, status, model, cwd, parent_session_id, \
    is_subagent, prompt, messages_path, updated_at, agent_id";

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
    pub pid: Option<u32>,
    pub started: Option<u64>,
    pub status: String,
    pub model: String,
    pub cwd: PathBuf,
    pub parent: Option<String>,
    pub subagent: bool,
    pub prompt: Option<String>,
    pub messages: PathBuf,
    pub updated: u64,
    pub agent: Option<String>,
}

impl Row {
    fn of(row: &rusqlite::Row) -> rusqlite::Result<Self> {
        let pid: Option<i64> = row.get(1)?;
        let started: Option<String> = row.get(2)?;
        let updated: String = row.get(10)?;
        Ok(Self {
            id: row.get(0)?,
            pid: pid
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid > 0),
            started: started.as_deref().and_then(millis),
            status: row.get(3)?,
            model: row.get(4)?,
            cwd: PathBuf::from(row.get::<_, String>(5)?),
            parent: row.get::<_, Option<String>>(6)?.filter(|p| !p.is_empty()),
            subagent: row.get::<_, i64>(7)? != 0,
            prompt: row.get(8)?,
            messages: PathBuf::from(row.get::<_, String>(9)?),
            updated: millis(&updated).unwrap_or(0),
            agent: row.get::<_, Option<String>>(11)?.filter(|a| !a.is_empty()),
        })
    }

    /// Cline says a turn of it runs or waits, in a live process: the one its mark names, or
    /// else one that names the conversation among its arguments. Cline keeps the pid of the
    /// process that first ran a conversation when another resumes it (`cline --id <id>`, or
    /// this driver's turn of it), so the mark names a process gone.
    pub fn live(&self) -> Option<(u32, u64)> {
        if !matches!(self.status.as_str(), "running" | "pending") {
            return None;
        }
        let alive = |pid: u32| Some((pid, steery_driver_common::kernel::started(pid)?));
        self.pid
            .and_then(alive)
            .or_else(|| resumer(&self.id).and_then(alive))
    }

    /// How the last turn ended, where no turn runs.
    pub fn ended(&self) -> TurnState {
        match self.status.as_str() {
            "failed" => TurnState::Failed,
            "idle" | "completed" => TurnState::Finished,
            _ => TurnState::Interrupted,
        }
    }

    /// A sub-agent, not a conversation of its own.
    fn is_sub_agent(&self) -> bool {
        self.subagent || self.parent.is_some()
    }

    /// The first line of the person's first words.
    fn title(&self) -> Option<String> {
        let prompt = person(self.prompt.as_deref()?);
        let line = prompt.lines().find(|line| !line.trim().is_empty())?.trim();
        Some(line.to_owned())
    }

    fn session(&self, sub_agents: Vec<SubAgent>) -> Session {
        Session {
            external_id: self.id.clone(),
            cwd: Some(self.cwd.clone()),
            title: self.title(),
            created_at: self.started,
            updated_at: self.updated,
            transcripts: vec![self.messages.clone()],
            parent: None,
            app: false,
            sub_agents,
        }
    }

    fn sub_agent(&self) -> SubAgent {
        SubAgent {
            external_id: self.id.clone(),
            lead: self.parent.clone(),
            name: self.agent.clone(),
            task: self.title(),
            started_at: self.started,
            started_by: None,
            transcripts: vec![self.messages.clone()],
        }
    }
}

/// The first live process but this one with `id` as a whole argument.
fn resumer(id: &str) -> Option<u32> {
    let listed = std::process::Command::new("ps")
        .args(["-axww", "-o", "pid=,args="])
        .output()
        .ok()?;
    let me = std::process::id();
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .find_map(|line| {
            let mut words = line.split_whitespace();
            let pid: u32 = words.next()?.parse().ok()?;
            (pid != me && words.any(|word| word == id)).then_some(pid)
        })
}

/// Milliseconds since 1970 of an RFC 3339 time.
fn millis(text: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .and_then(|at| u64::try_from(at.timestamp_millis()).ok())
}

pub(crate) struct Store<'a> {
    db: &'a Path,
}

impl<'a> Store<'a> {
    pub fn new(db: &'a Path) -> Self {
        Self { db }
    }

    /// The database, read only; none where Cline has not made it yet.
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

    /// Every row.
    fn rows(&self) -> Result<Vec<Row>, Error> {
        let Some(db) = self.open()? else {
            return Ok(Vec::new());
        };
        let mut statement = db
            .prepare(&format!(
                "select {COLUMNS} from sessions order by started_at"
            ))
            .map_err(|e| self.unreadable(e))?;
        statement
            .query_map([], Row::of)
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(|e| self.unreadable(e))
    }

    /// One row, where Cline has it.
    pub fn row(&self, id: &str) -> Result<Option<Row>, Error> {
        let Some(db) = self.open()? else {
            return Ok(None);
        };
        db.query_row(
            &format!("select {COLUMNS} from sessions where session_id = ?1"),
            params![id],
            Row::of,
        )
        .optional()
        .map_err(|e| self.unreadable(e))
    }

    /// One row, refused where Cline has none.
    pub fn found(&self, id: &str) -> Result<Row, Error> {
        self.row(id)?.ok_or_else(|| not_found(id))
    }

    /// The folder a conversation runs in.
    pub fn directory(&self, id: &str) -> Result<Option<PathBuf>, Error> {
        Ok(self.row(id)?.map(|row| row.cwd))
    }

    /// `sessions/list`: each conversation, its sub-agents within it.
    pub fn list(&self, asked: List) -> Result<Listed, Error> {
        let rows = self.rows()?;
        let mut sessions = Vec::new();
        let mut excluded = Vec::new();
        for lead in rows.iter().filter(|row| !row.is_sub_agent()) {
            let mut sub_agents = Vec::new();
            let mut updated = lead.updated;
            let mut stack = vec![lead.id.as_str()];
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
            if asked.seen_since.is_some_and(|since| updated < since) {
                continue;
            }
            let mut session = lead.session(sub_agents);
            session.updated_at = updated;
            if !asked.transcripts.is_empty() {
                let named = |path: &PathBuf| asked.transcripts.contains(path);
                for sub_agent in &session.sub_agents {
                    if sub_agent.transcripts.iter().any(named) {
                        excluded.push(sub_agent.external_id.clone());
                    }
                }
                if !session.transcripts.iter().any(named) {
                    continue;
                }
            }
            sessions.push(session);
        }
        Ok(Listed {
            sessions,
            excluded,
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
        let rows = self.rows()?;
        let mut sub_agents = Vec::new();
        let mut stack = vec![row.id.clone()];
        while let Some(parent) = stack.pop() {
            for child in rows.iter().filter(|r| r.parent.as_deref() == Some(&parent)) {
                sub_agents.push(child.sub_agent());
                stack.push(child.id.clone());
            }
        }
        Ok(Located {
            session: row.session(sub_agents),
        })
    }

    /// The messages of a conversation's file; none where Cline wrote none yet.
    pub fn messages(row: &Row) -> Result<Vec<Value>, Error> {
        let text = match std::fs::read_to_string(&row.messages) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(Error::refused(
                    refusal::REFUSED,
                    format!("cannot read {}: {e}", row.messages.display()),
                ));
            }
        };
        let mut document: Value = serde_json::from_str(&text).map_err(|e| {
            Error::refused(
                refusal::REFUSED,
                format!("{} is not JSON: {e}", row.messages.display()),
            )
        })?;
        Ok(match document["messages"].take() {
            Value::Array(messages) => messages,
            _ => Vec::new(),
        })
    }

    /// `sessions/read`: the conversation's events after the cursor, or a sub-agent's.
    /// `servers` names the MCP servers Cline loads in a folder.
    pub fn read(
        &self,
        read: Read,
        servers: impl FnOnce(&Path) -> Vec<String>,
    ) -> Result<Events, Error> {
        let row = match &read.sub_agent {
            Some(sub_agent) => {
                let row = self.found(sub_agent)?;
                if row.parent.as_deref() != Some(read.external_id.as_str()) {
                    return Err(not_found(sub_agent));
                }
                row
            }
            None => self.found(&read.external_id)?,
        };
        let messages = Self::messages(&row)?;
        let id_at = |n: usize| -> Option<String> {
            n.checked_sub(1)
                .and_then(|last| messages.get(last))
                .and_then(|message| message["id"].as_str())
                .map(str::to_owned)
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
        let servers = servers(&row.cwd);
        let mut fold = Fold::new(state, &servers);
        for message in &messages[from..end] {
            fold.message(message);
        }
        if !more && row.live().is_none() {
            fold.end(row.ended());
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
    pub fn said_after(&self, id: &str, mark: &str) -> Result<Vec<String>, Error> {
        let from: usize = mark
            .parse()
            .map_err(|_| Error::invalid(format!("`{mark}` is no mark of this driver's")))?;
        let messages = Self::messages(&self.found(id)?)?;
        Ok(messages
            .iter()
            .skip(from)
            .filter(|message| message["role"] == "user")
            .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str().map(|text| person(text).to_owned()))
            .collect())
    }

    /// How many messages the conversation has: the mark that the next words come after.
    pub fn count(&self, id: &str) -> Result<usize, Error> {
        Ok(Self::messages(&self.found(id)?)?.len())
    }
}

/// The first message within `tail` bytes of the end that a person's words begin, else the
/// first after the bound.
fn tail_start(messages: &[Value], tail: Option<u64>) -> usize {
    let Some(tail) = tail else {
        return 0;
    };
    let mut bytes = 0u64;
    let mut start = messages.len();
    for (index, message) in messages.iter().enumerate().rev() {
        bytes += message.to_string().len() as u64;
        if bytes > tail {
            break;
        }
        start = index;
    }
    (start..messages.len())
        .find(|&index| {
            messages[index]["role"] == "user"
                && messages[index]["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b["type"] != "tool_result"))
        })
        .unwrap_or(start)
}

fn not_found(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("Cline has no conversation `{id}` here"),
    )
}
