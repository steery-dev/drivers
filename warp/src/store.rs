//! Warp's conversations: each `warp.sqlite` (see [`crate::home`]) has a row of each in
//! `agent_conversations`, its `summary` a JSON of its title and first folder, its
//! `conversation_data` a JSON that names the conversation that started it, if one did. Its
//! tasks are rows of `agent_tasks`, each a Protocol Buffers `Task` ([`crate::task`]). The
//! conversation's own agent works in its root tasks; a task with a parent is a sub-agent
//! that a call started. Warp writes an answer's text while the model streams it, so a read
//! stops before the streamed words of a request whose end Warp has not recorded.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use crate::events::{Fold, State};
use crate::home;
use crate::task::{self, Body, Message, Task};

/// The most messages that one read answers.
const BATCH: usize = 200;

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

/// Milliseconds since 1970 of a column that SQLite keeps as UTC text.
const MILLIS: &str = "cast(unixepoch({}, 'subsec') * 1000 as integer)";

fn millis(column: &str) -> String {
    MILLIS.replace("{}", column)
}

/// One conversation, with the store it is in.
#[derive(Clone)]
struct Conversation {
    id: String,
    title: Option<String>,
    cwd: Option<PathBuf>,
    /// The conversation that started it.
    parent: Option<String>,
    name: Option<String>,
    /// Warp hides it: Warp asked it alone to sum up a diff.
    hidden: bool,
    updated: u64,
}

/// One store, read only.
struct Db {
    path: PathBuf,
    connection: Connection,
}

impl Db {
    fn open(path: &Path) -> Result<Self, Error> {
        let unreadable = |e: rusqlite::Error| unreadable(path, e);
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(unreadable)?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(unreadable)?;
        Ok(Self {
            path: path.to_owned(),
            connection,
        })
    }

    fn unreadable(&self, e: rusqlite::Error) -> Error {
        unreadable(&self.path, e)
    }

    /// Every conversation, the newest write to it or its tasks as its time.
    fn conversations(&self, id: Option<&str>) -> Result<Vec<Conversation>, Error> {
        let sql = format!(
            "select c.conversation_id, c.conversation_data, c.summary, \
             max({c}, coalesce((select max({t}) from agent_tasks t \
             where t.conversation_id = c.conversation_id), 0)) \
             from agent_conversations c where ?1 is null or c.conversation_id = ?1 order by c.id",
            c = millis("c.last_modified_at"),
            t = millis("t.last_modified_at"),
        );
        let mut statement = self
            .connection
            .prepare(&sql)
            .map_err(|e| self.unreadable(e))?;
        statement
            .query_map(params![id], |row| {
                let data: Value = row
                    .get::<_, Option<String>>(1)?
                    .and_then(|data| serde_json::from_str(&data).ok())
                    .unwrap_or(Value::Null);
                let summary: Value = row
                    .get::<_, Option<String>>(2)?
                    .and_then(|summary| serde_json::from_str(&summary).ok())
                    .unwrap_or(Value::Null);
                let text = |value: &Value| {
                    value
                        .as_str()
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned)
                };
                Ok(Conversation {
                    id: row.get(0)?,
                    title: text(&summary["title"]).or_else(|| text(&summary["initial_query"])),
                    cwd: text(&summary["initial_working_directory"]).map(PathBuf::from),
                    parent: text(&data["parent_conversation_id"]),
                    name: text(&data["agent_name"]),
                    hidden: summary["is_unlisted_auto_code_diff"] == true,
                    updated: u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0),
                })
            })
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(|e| self.unreadable(e))
    }

    fn conversation(&self, id: &str) -> Result<Option<Conversation>, Error> {
        Ok(self.conversations(Some(id))?.into_iter().next())
    }

    /// A conversation's tasks, in the order Warp first wrote them.
    fn tasks(&self, id: &str) -> Result<Vec<Task>, Error> {
        let mut statement = self
            .connection
            .prepare("select task from agent_tasks where conversation_id = ?1 order by id")
            .map_err(|e| self.unreadable(e))?;
        let blobs = statement
            .query_map(params![id], |row| row.get::<_, Vec<u8>>(0))
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(|e| self.unreadable(e))?;
        Ok(blobs.iter().filter_map(|blob| task::task(blob)).collect())
    }

    /// Whether a conversation of this store has the task `id`.
    fn task_owner(&self, id: &str) -> Result<Option<String>, Error> {
        self.connection
            .query_row(
                "select conversation_id from agent_tasks where task_id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| self.unreadable(e))
    }
}

fn unreadable(path: &Path, e: rusqlite::Error) -> Error {
    Error::refused(
        refusal::REFUSED,
        format!("cannot read {}: {e}", path.display()),
    )
}

impl Conversation {
    /// The newest write to it or to a conversation it started.
    fn updated(&self, children: &[Conversation]) -> u64 {
        children
            .iter()
            .map(|child| child.updated)
            .fold(self.updated, u64::max)
    }

    fn session(&self, tasks: &[Task], children: &[Conversation]) -> Session {
        let roots = || tasks.iter().filter(|task| task.parent.is_none());
        let created = roots()
            .flat_map(|task| &task.messages)
            .find_map(|message| message.at);
        let cwd = self.cwd.clone().or_else(|| {
            roots()
                .flat_map(|task| &task.messages)
                .find_map(|message| match &message.body {
                    Body::Query { cwd, .. } | Body::System { cwd } => {
                        cwd.as_ref().map(PathBuf::from)
                    }
                    _ => None,
                })
        });
        let mut sub_agents: Vec<SubAgent> = tasks
            .iter()
            .filter(|task| task.parent.is_some())
            .map(|task| sub_agent(task, tasks))
            .collect();
        sub_agents.extend(children.iter().map(|child| SubAgent {
            external_id: child.id.clone(),
            lead: None,
            name: child.name.clone(),
            task: child.title.clone(),
            started_at: None,
            started_by: None,
            transcripts: Vec::new(),
        }));
        Session {
            external_id: self.id.clone(),
            cwd,
            // Rows from before Warp kept a `summary` are titled by their root task.
            title: self.title.clone().or_else(|| {
                roots()
                    .map(|task| task.description.trim())
                    .find(|description| !description.is_empty())
                    .map(str::to_owned)
            }),
            created_at: created,
            updated_at: self.updated(children),
            transcripts: Vec::new(),
            parent: None,
            app: false,
            sub_agents,
        }
    }
}

/// A task that a call started, as a sub-agent: its lead is the task of that call, unless
/// that is a root task, the conversation's own agent.
fn sub_agent(task: &Task, tasks: &[Task]) -> SubAgent {
    let parent = task.parent.as_deref();
    let lead = tasks
        .iter()
        .find(|other| Some(other.id.as_str()) == parent)
        .filter(|lead| lead.parent.is_some())
        .map(|lead| lead.id.clone());
    let started_by = tasks
        .iter()
        .flat_map(|other| &other.messages)
        .find_map(|message| match &message.body {
            Body::Call(call) if call.task.as_deref() == Some(task.id.as_str()) => {
                Some(call.id.clone())
            }
            _ => None,
        });
    SubAgent {
        external_id: task.id.clone(),
        lead,
        name: None,
        task: task
            .description
            .lines()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned),
        started_at: task.messages.iter().find_map(|message| message.at),
        started_by,
        transcripts: Vec::new(),
    }
}

/// Every store of Warp's here.
pub(crate) struct Store {
    dbs: Vec<PathBuf>,
}

impl Store {
    pub fn new(home: &Path) -> Self {
        Self {
            dbs: home::stores(home),
        }
    }

    fn dbs(&self) -> impl Iterator<Item = Result<Db, Error>> + '_ {
        self.dbs.iter().map(|path| Db::open(path))
    }

    /// The store with the conversation `id`, and the conversation.
    fn find(&self, id: &str) -> Result<(Db, Conversation), Error> {
        for db in self.dbs() {
            let db = db?;
            if let Some(conversation) = db.conversation(id)? {
                return Ok((db, conversation));
            }
        }
        Err(not_found(id))
    }

    /// `sessions/list`: each conversation that no other started, with its sub-agents.
    pub fn list(&self, asked: List) -> Result<Listed, Error> {
        let mut sessions = Vec::new();
        for db in self.dbs() {
            let db = db?;
            let conversations = db.conversations(None)?;
            for lead in conversations
                .iter()
                .filter(|conversation| conversation.parent.is_none() && !conversation.hidden)
            {
                let children: Vec<Conversation> = children(&conversations, &lead.id);
                if asked
                    .seen_since
                    .is_some_and(|since| lead.updated(&children) < since)
                {
                    continue;
                }
                sessions.push(lead.session(&db.tasks(&lead.id)?, &children));
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

    /// `sessions/locate`: the conversation, or the lead of the conversation or task named.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let id = &locate.external_id;
        for db in self.dbs() {
            let db = db?;
            let owner = match db.conversation(id)? {
                Some(_) => id.clone(),
                None => match db.task_owner(id)? {
                    Some(owner) => owner,
                    None => continue,
                },
            };
            let mut lead = db.conversation(&owner)?.ok_or_else(|| not_found(&owner))?;
            while let Some(parent) = lead.parent.clone() {
                lead = db
                    .conversation(&parent)?
                    .ok_or_else(|| not_found(&parent))?;
            }
            let conversations = db.conversations(None)?;
            let children = children(&conversations, &lead.id);
            return Ok(Located {
                session: lead.session(&db.tasks(&lead.id)?, &children),
            });
        }
        Err(not_found(id))
    }

    /// `sessions/read`: the conversation's events after the cursor, or a sub-agent's: a
    /// task of it, or a conversation it started.
    pub fn read(&self, read: Read) -> Result<Events, Error> {
        let (db, lead) = self.find(&read.external_id)?;
        let messages: Vec<Message> = match &read.sub_agent {
            None => roots(db.tasks(&lead.id)?),
            Some(sub_agent) => {
                let tasks = db.tasks(&lead.id)?;
                match tasks
                    .into_iter()
                    .find(|task| task.id == *sub_agent && task.parent.is_some())
                {
                    Some(task) => task.messages,
                    None => {
                        let child = db
                            .conversation(sub_agent)?
                            .filter(|child| child.parent.as_deref() == Some(lead.id.as_str()))
                            .ok_or_else(|| not_found(sub_agent))?;
                        roots(db.tasks(&child.id)?)
                    }
                }
            }
        };
        let ready = ready(&messages);
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
        let mut fold = Fold::new(state);
        for message in &messages[from..end] {
            fold.message(message);
        }
        let cursor = Cursor {
            v: 1,
            n: end,
            id: id_at(end),
            st: fold.state,
        };
        Ok(Events {
            cursor: serde_json::to_string(&cursor).unwrap_or_default(),
            events: fold.events,
            more: end < ready,
            reset,
            whole: from == 0,
            after: (!tailed).then(|| id_at(end)).flatten(),
        })
    }
}

/// The conversations that `lead` started.
fn children(conversations: &[Conversation], lead: &str) -> Vec<Conversation> {
    conversations
        .iter()
        .filter(|child| child.parent.as_deref() == Some(lead))
        .cloned()
        .collect()
}

/// The messages of the conversation's own agent: those of its root tasks.
fn roots(tasks: Vec<Task>) -> Vec<Message> {
    tasks
        .into_iter()
        .filter(|task| task.parent.is_none())
        .flat_map(|task| task.messages)
        .collect()
}

/// How many messages a read may fold: all but the streamed words of the last request, and
/// what follows them, while Warp has recorded no end of that request.
fn ready(messages: &[Message]) -> usize {
    let Some(last) = messages.last() else {
        return 0;
    };
    let start = messages
        .iter()
        .rposition(|message| message.request != last.request)
        .map_or(0, |index| index + 1);
    let request = &messages[start..];
    if request
        .iter()
        .any(|message| matches!(message.body, Body::Request(_)))
    {
        return messages.len();
    }
    request
        .iter()
        .position(|message| matches!(message.body, Body::Output(_) | Body::Reasoning(_)))
        .map_or(messages.len(), |index| start + index)
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
        bytes += message.bytes as u64;
        if bytes > tail {
            break;
        }
        start = index;
    }
    (start..messages.len())
        .find(|&index| matches!(messages[index].body, Body::Query { .. }))
        .unwrap_or(start)
}

fn not_found(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("Warp has no conversation `{id}` here"),
    )
}
