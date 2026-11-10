//! Kilo's conversations, as its database keeps them: one SQLite file, `kilo.db` in its data
//! folder, whose `session` rows are the conversations and their sub-agents (a sub-agent is a
//! session whose `parent_id` names the session that started it), and whose `message` and
//! `part` rows hold what each one said, as JSON in `data`. Kilo writes a message's row when
//! the message starts and rewrites it, and its parts, until the message completes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;
use steery_driver_common::kernel;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use super::config;
use super::events::{self, Cursor, Message};

/// How many messages one read gives at most.
const BATCH: usize = 200;

/// How long a read waits on Kilo's own write before it gives up.
const BUSY: std::time::Duration = std::time::Duration::from_secs(2);

/// Kilo's database, and the folders it keeps its data, its configuration and its state in.
pub struct Store {
    db: PathBuf,
    data: PathBuf,
    config: PathBuf,
    state: PathBuf,
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
            state: xdg(home, "XDG_STATE_HOME", ".local/state").join("kilo"),
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

    /// Kilo's state folder: its marks of the turns that run (`session-turn/`) and the
    /// models picked last (`model.json`).
    pub fn state(&self) -> &Path {
        &self.state
    }

    /// The database where it is not the one Kilo opens with no `KILO_DB`.
    pub fn moved(&self) -> Option<&Path> {
        (self.db != self.data.join("kilo.db")).then_some(self.db.as_path())
    }

    /// The folder a conversation ran in; `not-found` where Kilo has no such conversation.
    pub fn directory(&self, id: &str) -> Result<Option<PathBuf>, Error> {
        let db = self.open()?.ok_or_else(|| not_found(id))?;
        db.query_row(
            "select directory from session where id = ?1",
            params![id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|error| self.refused(error))?
        .map(|directory| directory.map(PathBuf::from))
        .ok_or_else(|| not_found(id))
    }

    /// The tool and input of the call `call` of a conversation, as its `tool` part keeps
    /// them; none where Kilo keeps no such part.
    pub fn tool(&self, id: &str, call: &str) -> Result<Option<(String, Value)>, Error> {
        let Some(db) = self.open()? else {
            return Ok(None);
        };
        let data = db
            .query_row(
                "select data from part where session_id = ?1 \
                 and json_extract(data, '$.type') = 'tool' \
                 and json_extract(data, '$.callID') = ?2 limit 1",
                params![id, call],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| self.refused(error))?;
        Ok(data
            .and_then(|data| serde_json::from_str::<Value>(&data).ok())
            .and_then(|part| {
                let tool = part.get("tool")?.as_str()?.to_owned();
                let input = part.pointer("/state/input").cloned().unwrap_or(Value::Null);
                Some((tool, input))
            }))
    }

    /// The id of a conversation's newest message, `""` where it has none. Kilo's message
    /// ids grow with time, so every message written after it sorts after it.
    pub fn newest(&self, id: &str) -> Result<String, Error> {
        let db = self.open()?.ok_or_else(|| not_found(id))?;
        db.query_row(
            "select (select max(m.id) from message m where m.session_id = s.id) \
             from session s where s.id = ?1",
            params![id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|error| self.refused(error))?
        .map(Option::unwrap_or_default)
        .ok_or_else(|| not_found(id))
    }

    /// The person's messages after the message `after`, each as the text of its text parts.
    pub fn said_after(&self, id: &str, after: &str) -> Result<Vec<String>, Error> {
        let db = self.open()?.ok_or_else(|| not_found(id))?;
        let mut statement = db
            .prepare(
                "select m.id, p.data from message m join part p on p.message_id = m.id \
                 where m.session_id = ?1 and m.id > ?2 \
                 and json_extract(m.data, '$.role') = 'user' order by m.id, p.id",
            )
            .map_err(|error| self.refused(error))?;
        let rows = statement
            .query_map(params![id, after], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| self.refused(error))?;
        let mut said: Vec<(String, String)> = Vec::new();
        for row in rows {
            let (message, part) = row.map_err(|error| self.refused(error))?;
            let part: Value = serde_json::from_str(&part).unwrap_or(Value::Null);
            if part.get("type").and_then(Value::as_str) != Some("text") {
                continue;
            }
            let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
            match said.last_mut() {
                Some((last, words)) if *last == message => words.push_str(text),
                _ => said.push((message, text.to_owned())),
            }
        }
        Ok(said.into_iter().map(|(_, words)| words).collect())
    }

    /// A conversation's newest person message, when it was written, and its newest answer,
    /// as Kilo keeps it in `data`.
    pub fn latest(&self, id: &str) -> Result<Latest, Error> {
        let db = self.open()?.ok_or_else(|| not_found(id))?;
        let newest = |role: &str| {
            db.query_row(
                "select time_created, data from message where session_id = ?1 \
                 and json_extract(data, '$.role') = ?2 order by id desc limit 1",
                params![id, role],
                |row| Ok((millis(row.get(0)?), row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| self.refused(error))
        };
        let asked = newest("user")?;
        let answer = newest("assistant")?;
        Ok(Latest {
            asked: asked.as_ref().map(|(at, _)| *at),
            model: asked.and_then(|(_, data)| {
                let data: Value = serde_json::from_str(&data).ok()?;
                data.get("model").cloned()
            }),
            answer: answer.and_then(|(_, data)| serde_json::from_str(&data).ok()),
        })
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
        let not_found = || not_found(&asked.external_id);
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

/// Kilo's refusal of a conversation it does not have.
fn not_found(id: &str) -> Error {
    Error::refused(refusal::NOT_FOUND, format!("Kilo has no conversation {id}"))
}

/// The newest of a conversation, as [`Store::latest`] reads it.
pub struct Latest {
    /// When the person's newest message was written.
    pub asked: Option<u64>,
    /// The model that message asked for: `{providerID, modelID}`.
    pub model: Option<Value>,
    /// The newest answer's `data`.
    pub answer: Option<Value>,
}

/// A process of Kilo's that runs a turn of a conversation, as its own mark says.
#[derive(Debug, PartialEq, Eq)]
pub enum Turn {
    /// A process on this computer.
    Here(u32),
    /// A process on the computer of that name, which shares Kilo's state folder.
    Elsewhere(String),
}

impl Store {
    /// The process that runs a turn of `id` now. Kilo marks each turn with the file
    /// `session-turn/<id>.<pid>`, which holds the computer's name, from before the turn
    /// starts until it ends, and refreshes the file's time every 20 s. Kilo itself reads a
    /// mark of this computer as live while its process lives, up to a day, and a mark of
    /// another computer as live for a minute after its last refresh.
    pub fn turn(&self, id: &str) -> Option<Turn> {
        const ELSEWHERE: std::time::Duration = std::time::Duration::from_secs(60);
        const HERE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
        let here = hostname();
        let marks = std::fs::read_dir(self.state.join("session-turn")).ok()?;
        marks.flatten().find_map(|mark| {
            let name = mark.file_name();
            let (session, pid) = name.to_str()?.rsplit_once('.')?;
            let pid: u32 = pid.parse().ok().filter(|_| session == id)?;
            let age = mark
                .metadata()
                .ok()?
                .modified()
                .ok()?
                .elapsed()
                .unwrap_or_default();
            let host = std::fs::read_to_string(mark.path()).ok()?;
            if here.as_deref() == Some(host.as_str()) {
                let alive = i32::try_from(pid).is_ok_and(|pid| kernel::exists(pid).unwrap_or(true));
                (alive && age <= HERE).then_some(Turn::Here(pid))
            } else {
                (age <= ELSEWHERE).then_some(Turn::Elsewhere(host))
            }
        })
    }
}

/// This computer's name, as Kilo writes it in a mark (Node's `os.hostname()`).
fn hostname() -> Option<String> {
    let mut name = [0u8; 256];
    // SAFETY: the buffer is `name.len()` bytes long.
    if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) } != 0 {
        return None;
    }
    let end = name.iter().position(|byte| *byte == 0)?;
    String::from_utf8(name[..end].to_vec()).ok()
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
