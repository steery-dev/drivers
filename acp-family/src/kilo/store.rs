//! Kilo's conversations, as its database keeps them: one SQLite file, `kilo.db` in its data
//! folder, whose `session` rows are the conversations and their sub-agents (a sub-agent is a
//! session whose `parent_id` names the session that started it), and whose `message` and
//! `part` rows hold what each one said, as JSON in `data`. Kilo writes a message's row when
//! the message starts and rewrites it, and its parts, until the message completes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use super::config;
use super::events::{self, Cursor, Message};

/// How many messages one read gives at most.
const BATCH: usize = 200;

/// How long a read waits on Kilo's own write before it gives up.
const BUSY: std::time::Duration = std::time::Duration::from_secs(2);

/// Kilo's database, and the folders it keeps its data and its configuration in.
pub struct Store {
    db: PathBuf,
    data: PathBuf,
    config: PathBuf,
}

/// One `session` row.
struct Row {
    id: String,
    parent: Option<String>,
    directory: Option<String>,
    title: Option<String>,
    created: u64,
    updated: u64,
    /// `kilo run` without a person: it denies every `question`.
    headless: bool,
}

impl Store {
    /// The database Kilo opens here: `KILO_DB` where it is set (a name in its data folder,
    /// or an absolute path), else `kilo.db` in its data folder, `$XDG_DATA_HOME/kilo`.
    pub fn new(home: &Path) -> Self {
        let data = xdg(home, "XDG_DATA_HOME", ".local/share").join("kilo");
        let db = match std::env::var_os("KILO_DB").filter(|db| !db.is_empty()) {
            Some(db) if Path::new(&db).is_absolute() => PathBuf::from(db),
            Some(db) => data.join(db),
            None => data.join("kilo.db"),
        };
        Self {
            db,
            data,
            config: xdg(home, "XDG_CONFIG_HOME", ".config").join("kilo"),
        }
    }

    /// Kilo's data folder, where its database is.
    pub fn data(&self) -> &Path {
        &self.data
    }

    /// Kilo's global configuration folder.
    pub fn config(&self) -> &Path {
        &self.config
    }

    fn open(&self) -> Result<Option<Connection>, Error> {
        if !self.db.is_file() {
            return Ok(None);
        }
        let db = Connection::open_with_flags(
            &self.db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| self.refused(error))?;
        db.busy_timeout(BUSY).map_err(|error| self.refused(error))?;
        Ok(Some(db))
    }

    fn refused(&self, error: rusqlite::Error) -> Error {
        Error::refused(
            refusal::REFUSED,
            format!(
                "Kilo's database {} did not read: {error}",
                self.db.display()
            ),
        )
    }

    fn rows(&self, db: &Connection) -> Result<Vec<Row>, Error> {
        let mut statement = db
            .prepare(
                "select id, parent_id, directory, title, time_created, time_updated, permission \
                 from session",
            )
            .map_err(|error| self.refused(error))?;
        let rows = statement
            .query_map([], |row| {
                let permission: Option<String> = row.get(6)?;
                Ok(Row {
                    id: row.get(0)?,
                    parent: row.get(1)?,
                    directory: row.get(2)?,
                    title: row.get(3)?,
                    created: millis(row.get(4)?),
                    updated: millis(row.get(5)?),
                    headless: permission.as_deref().is_some_and(headless),
                })
            })
            .map_err(|error| self.refused(error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| self.refused(error))?;
        Ok(rows)
    }

    /// The conversations, each with its sub-agents. A `kilo run` with no person at it is
    /// excluded.
    pub fn list(&self, asked: List) -> Result<Listed, Error> {
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
        let rows = self.rows(&db)?;
        let tree = Tree::new(&rows);
        let mut sessions = Vec::new();
        let mut excluded = Vec::new();
        for row in rows.iter().filter(|row| row.parent.is_none()) {
            let session = self.session(&db, &tree, row)?;
            if asked
                .seen_since
                .is_some_and(|since| session.updated_at < since)
            {
                continue;
            }
            match row.headless {
                true => excluded.push(session.external_id),
                false => sessions.push(session),
            }
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

    pub fn locate(&self, asked: Locate) -> Result<Located, Error> {
        let not_found = || {
            Error::refused(
                refusal::NOT_FOUND,
                format!("Kilo has no conversation {}", asked.external_id),
            )
        };
        let db = self.open()?.ok_or_else(not_found)?;
        let rows = self.rows(&db)?;
        let tree = Tree::new(&rows);
        let row = rows
            .iter()
            .find(|row| row.id == asked.external_id && row.parent.is_none())
            .ok_or_else(not_found)?;
        Ok(Located {
            session: self.session(&db, &tree, row)?,
        })
    }

    fn session(&self, db: &Connection, tree: &Tree, row: &Row) -> Result<Session, Error> {
        let below = tree.below(&row.id);
        let started = match below.is_empty() {
            true => HashMap::new(),
            false => self.tasks(db, &row.id, &below)?,
        };
        let sub_agents = below
            .iter()
            .map(|sub| {
                let task = started.get(&sub.id);
                SubAgent {
                    external_id: sub.id.clone(),
                    lead: sub.parent.clone().filter(|lead| *lead != row.id),
                    name: task.and_then(|task| task.name.clone()),
                    task: task
                        .and_then(|task| task.task.clone())
                        .or_else(|| sub.title.clone()),
                    started_at: Some(sub.created),
                    started_by: task.map(|task| task.call.clone()),
                    transcripts: Vec::new(),
                }
            })
            .collect();
        Ok(Session {
            external_id: row.id.clone(),
            cwd: row.directory.as_ref().map(PathBuf::from),
            title: row.title.clone().filter(|title| !title.is_empty()),
            created_at: Some(row.created),
            updated_at: below
                .iter()
                .map(|sub| sub.updated)
                .fold(row.updated, u64::max),
            transcripts: Vec::new(),
            parent: None,
            app: false,
            sub_agents,
        })
    }

    /// The `task` calls of a conversation and of its sub-agents, by the sub-agent each
    /// started (its `metadata.sessionId`).
    fn tasks(
        &self,
        db: &Connection,
        root: &str,
        below: &[&Row],
    ) -> Result<HashMap<String, Task>, Error> {
        let mut statement = db
            .prepare(
                "select data from part where session_id = ?1 \
                 and json_extract(data, '$.type') = 'tool' \
                 and json_extract(data, '$.tool') = 'task'",
            )
            .map_err(|error| self.refused(error))?;
        let mut tasks = HashMap::new();
        for session in std::iter::once(root).chain(below.iter().map(|sub| sub.id.as_str())) {
            let parts = statement
                .query_map(params![session], |row| row.get::<_, String>(0))
                .map_err(|error| self.refused(error))?;
            for part in parts {
                let part = part.map_err(|error| self.refused(error))?;
                let Ok(part) = serde_json::from_str::<Value>(&part) else {
                    continue;
                };
                let Some(sub) = part
                    .pointer("/state/metadata/sessionId")
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let Some(call) = part.get("callID").and_then(Value::as_str) else {
                    continue;
                };
                let input = part.pointer("/state/input");
                let said = |key: &str| {
                    input
                        .and_then(|input| input.get(key))
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                };
                tasks.insert(
                    sub.to_owned(),
                    Task {
                        call: call.to_owned(),
                        name: said("subagent_type").map(str::to_owned),
                        task: said("description")
                            .or_else(|| said("prompt"))
                            .and_then(|text| text.lines().next())
                            .map(str::to_owned),
                    },
                );
            }
        }
        Ok(tasks)
    }

    /// The events of a conversation, or of one of its sub-agents, after `cursor`.
    pub fn read(&self, asked: Read) -> Result<Events, Error> {
        let not_found =
            |id: &str| Error::refused(refusal::NOT_FOUND, format!("Kilo has no conversation {id}"));
        let db = self.open()?.ok_or_else(|| not_found(&asked.external_id))?;
        let rows = self.rows(&db)?;
        let root = rows
            .iter()
            .find(|row| row.id == asked.external_id && row.parent.is_none())
            .ok_or_else(|| not_found(&asked.external_id))?;
        let target = match &asked.sub_agent {
            None => root.id.as_str(),
            Some(sub) => Tree::new(&rows)
                .below(&root.id)
                .into_iter()
                .find(|row| row.id == *sub)
                .map(|row| row.id.as_str())
                .ok_or_else(|| not_found(sub))?,
        };
        let mut cursor = asked
            .cursor
            .as_deref()
            .map(Cursor::parse)
            .transpose()
            .map_err(Error::invalid)?
            .unwrap_or_default();
        // Kilo deletes the messages after a point it reverted to: a cursor on one of them
        // reads again from the start.
        let reset = match &cursor.message {
            Some(message) => !self.has(&db, message)?,
            None => false,
        };
        if reset {
            cursor = Cursor::default();
        }
        let mut whole = cursor.message.is_none();
        if whole
            && let Some(tail) = asked.tail
            && let Some(before) = self.tail(&db, target, tail)?
        {
            cursor.message = Some(before);
            whole = false;
        }
        let after = cursor.message.clone().unwrap_or_default();
        let mut messages = self.messages(&db, target, &after)?;
        let more = messages.len() > BATCH;
        messages.truncate(BATCH);
        let batch = messages.len();
        let servers = config::servers(&self.config, root.directory.as_deref().map(Path::new));
        let (events, read) = events::fold(&mut cursor, messages, &servers);
        Ok(Events {
            cursor: cursor.to_string(),
            events,
            // A message still running holds the rest back until it ends.
            more: more && read == batch,
            reset,
            whole,
            after: asked
                .tail
                .is_none()
                .then(|| cursor.message.clone())
                .flatten(),
        })
    }

    fn has(&self, db: &Connection, message: &str) -> Result<bool, Error> {
        db.query_row(
            "select 1 from message where id = ?1",
            params![message],
            |_| Ok(()),
        )
        .optional()
        .map(|found| found.is_some())
        .map_err(|error| self.refused(error))
    }

    /// The message before the newest messages whose rows and parts fit within `tail`
    /// bytes; none where every message fits.
    fn tail(&self, db: &Connection, session: &str, tail: u64) -> Result<Option<String>, Error> {
        let mut statement = db
            .prepare(
                "select m.id, length(m.data) + coalesce( \
                   (select sum(length(p.data)) from part p where p.message_id = m.id), 0) \
                 from message m where m.session_id = ?1 order by m.id desc",
            )
            .map_err(|error| self.refused(error))?;
        let mut sizes = statement
            .query_map(params![session], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(|error| self.refused(error))?;
        let mut kept = 0u64;
        for size in &mut sizes {
            let (id, bytes) = size.map_err(|error| self.refused(error))?;
            kept += u64::try_from(bytes).unwrap_or(0);
            if kept > tail {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    /// Up to one more than [`BATCH`] messages after `after`, oldest first, each with its
    /// parts in order.
    fn messages(&self, db: &Connection, session: &str, after: &str) -> Result<Vec<Message>, Error> {
        let mut statement = db
            .prepare(
                "select id, time_created, data from message \
                 where session_id = ?1 and id > ?2 order by id limit ?3",
            )
            .map_err(|error| self.refused(error))?;
        let rows = statement
            .query_map(params![session, after, (BATCH + 1) as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    millis(row.get(1)?),
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| self.refused(error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| self.refused(error))?;
        let mut parts = db
            .prepare("select id, data from part where message_id = ?1 order by id")
            .map_err(|error| self.refused(error))?;
        let mut messages = Vec::with_capacity(rows.len());
        for (id, created, data) in rows {
            let info = serde_json::from_str(&data).unwrap_or(Value::Null);
            let parts = parts
                .query_map(params![id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|error| self.refused(error))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| self.refused(error))?
                .into_iter()
                .map(|(id, data)| (id, serde_json::from_str(&data).unwrap_or(Value::Null)))
                .collect();
            messages.push(Message {
                id,
                created,
                info,
                parts,
            });
        }
        Ok(messages)
    }
}

/// A `task` call that started a sub-agent.
struct Task {
    call: String,
    name: Option<String>,
    task: Option<String>,
}

/// The sessions by the session that started each.
struct Tree<'a> {
    children: HashMap<&'a str, Vec<&'a Row>>,
}

impl<'a> Tree<'a> {
    fn new(rows: &'a [Row]) -> Self {
        let mut children: HashMap<&str, Vec<&Row>> = HashMap::new();
        for row in rows {
            if let Some(parent) = &row.parent {
                children.entry(parent.as_str()).or_default().push(row);
            }
        }
        for rows in children.values_mut() {
            rows.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
        }
        Self { children }
    }

    /// Every session below `id`, each before the sessions it started.
    fn below(&self, id: &str) -> Vec<&'a Row> {
        let mut found = Vec::new();
        let mut next = vec![id];
        while let Some(id) = next.pop() {
            for row in self.children.get(id).into_iter().flatten() {
                // A cycle a broken row could make ends here.
                if found.iter().any(|seen: &&Row| seen.id == row.id) {
                    continue;
                }
                found.push(*row);
                next.push(row.id.as_str());
            }
        }
        found
    }
}

/// Whether a session's `permission` rules are those of `kilo run` without a person: it
/// denies `question` everywhere, since nobody is there to answer one.
fn headless(permission: &str) -> bool {
    serde_json::from_str::<Value>(permission)
        .ok()
        .as_ref()
        .and_then(Value::as_array)
        .is_some_and(|rules| {
            rules.iter().any(|rule| {
                rule.get("permission").and_then(Value::as_str) == Some("question")
                    && rule.get("action").and_then(Value::as_str) == Some("deny")
                    && rule.get("pattern").and_then(Value::as_str) == Some("*")
            })
        })
}

fn millis(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// An XDG base folder: the variable where it is absolute, else its default under `home`.
fn xdg(home: &Path, variable: &str, default: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(|| home.join(default))
}
