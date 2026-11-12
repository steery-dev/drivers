//! OpenClaw's conversations, from the SQLite store of each agent,
//! `<root>/agents/<agent>/agent/openclaw-agent.sqlite`, opened read-only.
//!
//! `session_nodes` holds one row per conversation key (`agent:main:main`,
//! `agent:main:explicit:<name>`), naming the window it writes now. `session_windows` holds
//! each window, a `session_id` whose records are the `transcript_events` rows. A window that
//! continues another names it in `previous_session_id` with the reason `compaction`,
//! `rollover` or `recovery`: the same conversation, which the driver reads as one under its
//! first window's id. A `reset`, `fork`, `rewind` or `switch` starts a new conversation; a
//! `fork` names the one it came from.
//!
//! `openclaw agent` runs (`created_via` `run`) are listed: a person typed each at the CLI.
//! A key that a sub-agent's spawn made (`spawned_by` or `parent_session_key`) is listed under
//! the conversation that spawned it; no live store has one, so that link is read off
//! `src/config/sessions/session-accessor.sqlite-canonical-inventory.ts:122-124` alone. Keys
//! that cron or OpenClaw itself made (`cron`, `internal`) have no person and are excluded.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_driver_common::lines::BATCH;
use steery_sdk::wire::harness::{
    Events, Execution, List, Listed, Locate, Located, Read, Session, SubAgent,
};
use steery_sdk::{Error, refusal};

use super::events::{self, Child, Entry, Fold};

const STORE: &str = "openclaw-agent.sqlite";

/// The reasons of a window that goes on with the conversation of the window it names.
const CONTINUES: [&str; 3] = ["compaction", "rollover", "recovery"];

/// The `created_via` of keys that no person made.
const MACHINE: [&str; 2] = ["cron", "internal"];

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(2))?;
    Ok(connection)
}

fn failed(why: rusqlite::Error) -> Error {
    Error::refused(refusal::REFUSED, format!("OpenClaw's store: {why}"))
}

fn not_found(id: &str) -> Error {
    Error::refused(refusal::NOT_FOUND, format!("no OpenClaw conversation {id}"))
}

/// One row of `session_windows`.
struct Window {
    id: String,
    key: String,
    previous: Option<String>,
    reason: Option<String>,
    created_at: u64,
    updated_at: u64,
    spawned: bool,
}

/// One row of `session_nodes`.
struct Node {
    current: Option<String>,
    status: Option<String>,
    created_via: Option<String>,
    parent: Option<String>,
    title: Option<String>,
    fork_source: Option<String>,
    /// The model's context window, `entry_json.contextTokens`.
    context: Option<u64>,
}

/// One agent's store, its windows chained into conversations.
struct Agent {
    path: PathBuf,
    connection: Connection,
    windows: HashMap<String, Window>,
    nodes: HashMap<String, Node>,
    /// Each conversation's windows, oldest first, by its first window's id.
    chains: HashMap<String, Vec<String>>,
    /// The conversation each window is part of.
    head: HashMap<String, String>,
}

fn millis(value: Option<i64>) -> u64 {
    value.unwrap_or(0).max(0) as u64
}

impl Agent {
    fn load(path: &Path) -> rusqlite::Result<Self> {
        let connection = open(path)?;
        let mut windows = HashMap::new();
        let mut order = Vec::new();
        {
            let mut statement = connection.prepare(
                "SELECT session_id, session_key, previous_session_id, reason, created_at,
                        updated_at, parent_session_key, spawned_by
                 FROM session_windows ORDER BY created_at, session_id",
            )?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let window = Window {
                    id: row.get(0)?,
                    key: row.get(1)?,
                    previous: row.get(2)?,
                    reason: row.get(3)?,
                    created_at: millis(row.get(4)?),
                    updated_at: millis(row.get(5)?),
                    spawned: row
                        .get::<_, Option<String>>(6)?
                        .is_some_and(|s| !s.is_empty())
                        || row
                            .get::<_, Option<String>>(7)?
                            .is_some_and(|s| !s.is_empty()),
                };
                order.push(window.id.clone());
                windows.insert(window.id.clone(), window);
            }
        }
        let mut nodes = HashMap::new();
        {
            let mut statement = connection.prepare(
                "SELECT session_key, current_session_id, status, created_via,
                        coalesce(nullif(spawned_by, ''), nullif(parent_session_key, '')),
                        coalesce(nullif(label, ''), nullif(display_name, '')),
                        fork_source_session_id, json_extract(entry_json, '$.contextTokens')
                 FROM session_nodes",
            )?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let key: String = row.get(0)?;
                let context: Option<i64> = row.get(7).ok().flatten();
                nodes.insert(
                    key,
                    Node {
                        current: row.get(1)?,
                        status: row.get(2)?,
                        created_via: row.get(3)?,
                        parent: row.get(4)?,
                        title: row.get(5)?,
                        fork_source: row.get(6)?,
                        context: context.map(|tokens| tokens.max(0) as u64),
                    },
                );
            }
        }
        // A window that continues one: the newest such is the conversation's next.
        let mut next: HashMap<String, String> = HashMap::new();
        for id in &order {
            let window = &windows[id];
            if let Some(previous) = window.previous.as_ref().filter(|previous| {
                windows.contains_key(*previous)
                    && window
                        .reason
                        .as_deref()
                        .is_some_and(|reason| CONTINUES.contains(&reason))
            }) {
                next.insert(previous.clone(), id.clone());
            }
        }
        let continued: HashSet<&String> = next.values().collect();
        let mut chains = HashMap::new();
        let mut head = HashMap::new();
        for id in order.iter().filter(|id| !continued.contains(id)) {
            let mut chain = vec![id.clone()];
            let mut seen = HashSet::from([id.clone()]);
            while let Some(following) = next.get(chain.last().unwrap()) {
                if !seen.insert(following.clone()) {
                    break;
                }
                chain.push(following.clone());
            }
            for window in &chain {
                head.insert(window.clone(), id.clone());
            }
            chains.insert(id.clone(), chain);
        }
        Ok(Self {
            path: path.to_owned(),
            connection,
            windows,
            nodes,
            chains,
            head,
        })
    }

    fn node(&self, first: &str) -> Option<&Node> {
        self.nodes.get(&self.windows.get(first)?.key)
    }

    /// Whether the conversation is a sub-agent's run.
    fn spawned(&self, first: &str) -> bool {
        self.windows[first].spawned
            || self.node(first).is_some_and(|node| {
                node.parent.is_some() || node.created_via.as_deref() == Some("spawn")
            })
    }

    /// Whether no person is at the conversation.
    fn headless(&self, first: &str) -> bool {
        self.node(first)
            .and_then(|node| node.created_via.as_deref())
            .is_some_and(|via| MACHINE.contains(&via))
    }

    /// The conversation whose key is `key`: the one of its current window.
    fn of_key(&self, key: &str) -> Option<&String> {
        let current = self.nodes.get(key)?.current.as_ref()?;
        self.head.get(current)
    }

    /// The sub-agents' conversations and the conversation each works in.
    fn leads(&self) -> HashMap<&String, &String> {
        self.chains
            .keys()
            .filter(|first| self.spawned(first))
            .filter_map(|first| {
                let parent = self.node(first)?.parent.as_ref()?;
                Some((first, self.of_key(parent)?))
            })
            .collect()
    }

    fn cwd(&self, first: &str) -> Option<PathBuf> {
        let json: String = self
            .connection
            .query_row(
                "SELECT event_json FROM transcript_events
                 WHERE session_id = ?1 AND event_json IS NOT NULL ORDER BY seq LIMIT 1",
                [first],
                |row| row.get(0),
            )
            .ok()?;
        let header: Value = serde_json::from_str(&json).ok()?;
        (header.get("type").and_then(Value::as_str) == Some("session"))
            .then(|| header.get("cwd")?.as_str().map(PathBuf::from))
            .flatten()
    }

    fn session(&self, first: &str, leads: &HashMap<&String, &String>) -> Session {
        let chain = &self.chains[first];
        let window = &self.windows[first];
        let node = self.node(first);
        let parent = match (window.reason.as_deref(), &window.previous) {
            (Some("fork"), Some(previous)) => self.head.get(previous).cloned(),
            _ => None,
        }
        .or_else(|| {
            let source = node?.fork_source.as_ref()?;
            self.head.get(source).filter(|head| *head != first).cloned()
        });
        let sub_agents = leads
            .iter()
            .filter(|(_, lead)| lead.as_str() == first)
            .map(|(child, _)| SubAgent {
                external_id: (*child).clone(),
                lead: None,
                name: self.node(child).and_then(|node| node.title.clone()),
                task: None,
                started_at: Some(self.windows[*child].created_at),
                started_by: None,
                transcripts: vec![self.path.clone()],
            })
            .collect();
        Session {
            external_id: first.to_owned(),
            cwd: self.cwd(first),
            title: node.and_then(|node| node.title.clone()),
            created_at: Some(window.created_at),
            updated_at: chain
                .iter()
                .map(|id| self.windows[id].updated_at)
                .max()
                .unwrap_or(window.updated_at),
            transcripts: vec![self.path.clone()],
            parent,
            app: false,
            sub_agents,
        }
    }

    /// The records of a conversation's windows, in order, the session headers left out.
    fn records(&self, first: &str) -> rusqlite::Result<Vec<Entry>> {
        let mut statement = self.connection.prepare(
            "SELECT seq, event_json, event_zstd FROM transcript_events
             WHERE session_id = ?1 ORDER BY seq",
        )?;
        let mut entries = Vec::new();
        for window in &self.chains[first] {
            let mut rows = statement.query(params![window])?;
            while let Some(row) = rows.next()? {
                let seq: i64 = row.get(0)?;
                let json = match row.get::<_, Option<String>>(1)? {
                    Some(json) => json,
                    None => match row.get::<_, Option<Vec<u8>>>(2)? {
                        Some(packed) => match unpacked(&packed) {
                            Some(json) => json,
                            None => continue,
                        },
                        None => continue,
                    },
                };
                let Ok(record) = serde_json::from_str::<Value>(&json) else {
                    continue;
                };
                if record.get("type").and_then(Value::as_str) == Some("session") {
                    continue;
                }
                entries.push(Entry {
                    window: window.clone(),
                    seq,
                    record,
                    bytes: json.len() as u64,
                });
            }
        }
        Ok(entries)
    }

    /// The sub-agents of every key, for the `sub_agent` events of `sessions_spawn` results.
    fn children(&self) -> HashMap<String, Child> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.parent.is_some())
            .filter_map(|(key, node)| {
                let first = self.head.get(node.current.as_ref()?)?;
                Some((
                    key.clone(),
                    Child {
                        external_id: first.clone(),
                        state: node.status.as_deref().and_then(execution),
                    },
                ))
            })
            .collect()
    }
}

fn execution(status: &str) -> Option<Execution> {
    Some(match status {
        "running" => Execution::Running,
        "done" => Execution::Finished,
        "failed" | "timeout" => Execution::Failed,
        "killed" => Execution::Interrupted,
        _ => return None,
    })
}

/// A zstd frame's text.
fn unpacked(packed: &[u8]) -> Option<String> {
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(packed).ok()?;
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes).ok()?;
    String::from_utf8(bytes).ok()
}

/// The records on the active path. A transcript with no `leaf` record is one line, in
/// order. A `leaf` record moves the active leaf to its `targetId`, as a rewind or a branch
/// does, and the path is then the leaf's ancestors by `parentId`, with the leaf records
/// themselves left out and side appends (`appendMode` `side`) kept where their parent is on
/// it (`src/config/sessions/transcript-tree.ts:75-116,327-354`). OpenClaw's repair of parents
/// across a `reset` record is not followed.
fn active(entries: Vec<Entry>) -> Vec<Entry> {
    let is_leaf = |entry: &Entry| entry.record.get("type").and_then(Value::as_str) == Some("leaf");
    if !entries.iter().any(is_leaf) {
        return entries;
    }
    let id = |entry: &Entry| {
        entry
            .record
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let side =
        |entry: &Entry| entry.record.get("appendMode").and_then(Value::as_str) == Some("side");
    let mut parent: HashMap<String, Option<String>> = HashMap::new();
    let mut leaf: Option<String> = None;
    for entry in &entries {
        let Some(own) = id(entry) else {
            continue;
        };
        if is_leaf(entry) {
            let target = entry
                .record
                .get("targetId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if target
                .as_ref()
                .is_none_or(|target| parent.contains_key(target))
            {
                parent.insert(own, target.clone());
                leaf = target;
            }
            continue;
        }
        // A parentless record goes on from the active leaf.
        let named = match entry.record.get("parentId") {
            Some(Value::String(parent)) => Some(parent.clone()),
            Some(Value::Null) => None,
            _ => leaf.clone(),
        };
        parent.insert(own.clone(), named);
        if !side(entry) {
            leaf = Some(own);
        }
    }
    let mut path: HashSet<String> = HashSet::new();
    let mut at = leaf;
    while let Some(id) = at {
        if !path.insert(id.clone()) {
            break;
        }
        at = parent.get(&id).cloned().flatten();
    }
    entries
        .into_iter()
        .filter(|entry| !is_leaf(entry))
        .filter(|entry| match id(entry) {
            Some(own) if side(entry) => parent
                .get(&own)
                .and_then(Option::as_ref)
                .is_some_and(|parent| path.contains(parent)),
            Some(own) => path.contains(&own),
            None => true,
        })
        .collect()
}

/// Where a read ended: how many records of the active path it read, and the id of the last.
/// Another last record there means the path moved, a rewind or a branch: read again whole.
#[derive(Serialize, Deserialize)]
struct Cursor {
    v: u32,
    n: usize,
    last: Option<String>,
}

fn key_of(entry: &Entry) -> String {
    entry
        .record
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{}#{}", entry.window, entry.seq))
}

/// The configured MCP servers' names as OpenClaw writes them in its tools' names.
fn servers(root: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(root.join("openclaw.json")) else {
        return Vec::new();
    };
    let Ok(config) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    config
        .pointer("/mcp/servers")
        .and_then(Value::as_object)
        .map(|servers| {
            servers
                .keys()
                .map(|name| events::safe_server(name))
                .collect()
        })
        .unwrap_or_default()
}

/// OpenClaw's conversations under its state folder.
pub struct Store {
    root: PathBuf,
    stores: Vec<PathBuf>,
}

impl Store {
    pub fn new(root: &Path) -> Self {
        let mut stores: Vec<PathBuf> = std::fs::read_dir(root.join("agents"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|agent| agent.path().join("agent").join(STORE))
            .filter(|store| store.is_file())
            .collect();
        stores.sort();
        Self {
            root: root.to_owned(),
            stores,
        }
    }

    /// The store and the first window of the conversation `id` names.
    fn find(&self, id: &str) -> Result<(Agent, String), Error> {
        for path in &self.stores {
            let Ok(agent) = Agent::load(path) else {
                continue;
            };
            if let Some(first) = agent.head.get(id).cloned() {
                return Ok((agent, first));
            }
        }
        Err(not_found(id))
    }

    pub fn list(&self, list: List) -> Listed {
        let mut sessions = Vec::new();
        let mut excluded = Vec::new();
        let mut complete = true;
        for path in &self.stores {
            let Ok(agent) = Agent::load(path) else {
                complete = false;
                continue;
            };
            let leads = agent.leads();
            let mut firsts: Vec<&String> = agent.chains.keys().collect();
            firsts.sort();
            for first in firsts {
                if agent.spawned(first) || agent.headless(first) {
                    excluded.push(first.clone());
                    continue;
                }
                let session = agent.session(first, &leads);
                if list
                    .seen_since
                    .is_none_or(|since| session.updated_at >= since)
                {
                    sessions.push(session);
                }
            }
        }
        Listed {
            sessions,
            excluded,
            complete,
            cursor: None,
            after: None,
            gone: Vec::new(),
        }
    }

    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let (agent, first) = self.find(&locate.external_id)?;
        if first != locate.external_id || agent.spawned(&first) || agent.headless(&first) {
            return Err(not_found(&locate.external_id));
        }
        Ok(Located {
            session: agent.session(&first, &agent.leads()),
        })
    }

    pub fn read(&self, read: Read) -> Result<Events, Error> {
        let (agent, first) = self.find(&read.external_id)?;
        if first != read.external_id || agent.spawned(&first) {
            return Err(not_found(&read.external_id));
        }
        let target = match &read.sub_agent {
            Some(sub) => {
                let leads = agent.leads();
                match agent.head.get(sub) {
                    Some(child) if leads.get(child) == Some(&&first) => child.clone(),
                    _ => return Err(not_found(sub)),
                }
            }
            None => first,
        };
        let cursor = match &read.cursor {
            Some(text) => Some(
                serde_json::from_str::<Cursor>(text)
                    .map_err(|why| Error::invalid(format!("cursor: {why}")))?,
            ),
            None => None,
        };
        let path = active(agent.records(&target).map_err(failed)?);
        let reset = cursor.as_ref().is_some_and(|cursor| {
            cursor.n > path.len()
                || (cursor.n > 0 && Some(key_of(&path[cursor.n - 1])) != cursor.last)
        });
        let start = match (&cursor, reset, read.tail) {
            (Some(cursor), false, _) => cursor.n,
            (None, _, Some(tail)) => {
                let mut total = 0;
                let mut start = path.len();
                while start > 0 && total + path[start - 1].bytes <= tail {
                    start -= 1;
                    total += path[start].bytes;
                }
                start
            }
            _ => 0,
        };
        let servers = servers(&self.root);
        let children = agent.children();
        let mut fold = Fold {
            events: Vec::new(),
            path: &path,
            servers: &servers,
            children: &children,
            context: agent.node(&target).and_then(|node| node.context),
        };
        let mut end = start;
        let mut weight = 0;
        while end < path.len() && weight < BATCH {
            weight += path[end].bytes;
            fold.entry(end);
            end += 1;
        }
        let cursor = Cursor {
            v: 1,
            n: end,
            last: end.checked_sub(1).map(|last| key_of(&path[last])),
        };
        Ok(Events {
            cursor: serde_json::to_string(&cursor).unwrap_or_default(),
            events: fold.events,
            more: end < path.len(),
            reset,
            whole: start == 0,
            after: None,
        })
    }
}
