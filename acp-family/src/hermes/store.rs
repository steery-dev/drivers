//! Hermes's conversations, from the SQLite store `state.db` of its home and of each profile
//! (`<home>/profiles/<name>/state.db`), opened read-only.
//!
//! A session row is one of four things, by `source` and `model_config`:
//! - a continuation: a compaction that rotates the conversation into a new session names the
//!   old one in `_compressed_from` (`hermes_state_mutation_compress.py:18-21`). It is the same
//!   conversation, so the driver reads the chain of them as one, under its first id.
//! - a sub-agent: `source` `subagent` (`tools/delegate_tool_child_run.py:45`), whose
//!   `parent_session_id` is the session that delegated to it.
//! - a run with no person: the sources Hermes paces by machine (`agent/prompt_caching.py:155`)
//!   and its API server (`gateway/session_api.py:25`).
//! - a conversation: any other source, a person's surface or a messaging platform. A
//!   branch names its parent in `_branched_from` (`tui_gateway/methods_session.py:249`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_driver_common::lines::BATCH;
use steery_sdk::wire::harness::{
    Count, Event, Events, List, Listed, Locate, Located, Read, Session, SubAgent,
};
use steery_sdk::{Error, refusal};

use super::events::{Child, Fold, Row};
use super::mcp;

/// Sources of runs that no person attends.
const HEADLESS: &[&str] = &[
    "oneshot",
    "cron",
    "batch",
    "webhook",
    "kanban",
    "api",
    "api_server",
    "tool",
];

/// Hermes's home: `HERMES_HOME`, else `~/.hermes` (`hermes_constants.py`).
pub fn root(home: &Path) -> PathBuf {
    std::env::var_os("HERMES_HOME")
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".hermes"))
}

fn ms(seconds: f64) -> u64 {
    (seconds * 1000.0).round().max(0.0) as u64
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_secs(2))?;
    Ok(connection)
}

fn failed(why: rusqlite::Error) -> Error {
    Error::refused(refusal::REFUSED, format!("Hermes's state.db: {why}"))
}

fn not_found(id: &str) -> Error {
    Error::refused(refusal::NOT_FOUND, format!("no Hermes conversation {id}"))
}

/// One row of `sessions`.
struct Info {
    id: String,
    source: String,
    created_source: Option<String>,
    parent: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    config: Value,
    started: u64,
    updated: u64,
    ended: bool,
    title: Option<String>,
    cwd: Option<PathBuf>,
    /// input (without cache), cache read, cache write, output, reasoning.
    tokens: [u64; 5],
}

impl Info {
    fn mark(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(Value::as_str)
    }
}

/// The sessions of one store, each with the first session of its chain.
struct Store {
    connection: Connection,
    infos: HashMap<String, Info>,
    /// The ids of each chain, oldest first, by its first id.
    chains: HashMap<String, Vec<String>>,
}

impl Store {
    fn load(path: &Path) -> rusqlite::Result<Self> {
        let connection = open(path)?;
        let mut infos = HashMap::new();
        {
            let mut statement = connection.prepare(
                "SELECT id, source, created_source, parent_session_id, model, billing_provider,
                        model_config, started_at, ended_at, last_activity_at, title, cwd,
                        input_tokens, cache_read_tokens, cache_write_tokens, output_tokens,
                        reasoning_tokens
                 FROM sessions",
            )?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let started: f64 = row.get(7)?;
                let ended: Option<f64> = row.get(8)?;
                let last: Option<f64> = row.get(9)?;
                let token = |at: usize| -> rusqlite::Result<u64> {
                    Ok(row.get::<_, Option<i64>>(at)?.unwrap_or(0).max(0) as u64)
                };
                let info = Info {
                    id: row.get(0)?,
                    source: row.get(1)?,
                    created_source: row.get(2)?,
                    parent: row.get(3)?,
                    model: row.get::<_, Option<String>>(4)?.filter(|m| !m.is_empty()),
                    provider: row.get::<_, Option<String>>(5)?.filter(|p| !p.is_empty()),
                    config: row
                        .get::<_, Option<String>>(6)?
                        .and_then(|text| serde_json::from_str(&text).ok())
                        .unwrap_or(Value::Null),
                    started: ms(started),
                    updated: ms([Some(started), ended, last]
                        .into_iter()
                        .flatten()
                        .fold(0.0, f64::max)),
                    ended: ended.is_some(),
                    title: row
                        .get::<_, Option<String>>(10)?
                        .filter(|t| !t.trim().is_empty()),
                    cwd: row
                        .get::<_, Option<String>>(11)?
                        .filter(|cwd| !cwd.is_empty())
                        .map(PathBuf::from),
                    tokens: [token(12)?, token(13)?, token(14)?, token(15)?, token(16)?],
                };
                infos.insert(info.id.clone(), info);
            }
        }
        let mut chains: HashMap<String, Vec<String>> = HashMap::new();
        for id in infos.keys() {
            chains
                .entry(first(&infos, id))
                .or_default()
                .push(id.clone());
        }
        for chain in chains.values_mut() {
            chain.sort_by_key(|id| (infos[id].started, id.clone()));
        }
        Ok(Self {
            connection,
            infos,
            chains,
        })
    }

    fn info(&self, id: &str) -> &Info {
        &self.infos[id]
    }

    fn subagent(&self, first: &str) -> bool {
        self.info(first).source == "subagent"
    }

    fn headless(&self, first: &str) -> bool {
        HEADLESS.contains(&self.info(first).source.as_str())
    }

    /// The conversation a chain belongs to: its own first id, or for a sub-agent the
    /// conversation of the session that delegated to it.
    fn conversation(&self, first: &str) -> String {
        let mut at = first.to_owned();
        let mut seen = HashSet::new();
        while self.subagent(&at) && seen.insert(at.clone()) {
            match self
                .info(&at)
                .parent
                .as_deref()
                .filter(|p| self.infos.contains_key(*p))
            {
                Some(parent) => at = self::first(&self.infos, parent),
                None => break,
            }
        }
        at
    }

    /// The sub-agent chains under the conversation `first`, each with its lead: the
    /// sub-agent that delegated to it, if one did.
    fn children(&self, first: &str) -> Vec<(String, Option<String>)> {
        let mut children: Vec<(String, Option<String>)> = self
            .chains
            .keys()
            .filter(|id| self.subagent(id) && *id != first && self.conversation(id) == first)
            .map(|id| {
                let lead = self
                    .info(id)
                    .parent
                    .as_deref()
                    .filter(|p| self.infos.contains_key(*p))
                    .map(|p| self::first(&self.infos, p))
                    .filter(|p| self.subagent(p));
                (id.clone(), lead)
            })
            .collect();
        children.sort_by_key(|(id, _)| (self.info(id).started, id.clone()));
        children
    }

    /// The ids of a chain as a JSON array, for `json_each`.
    fn ids(&self, first: &str) -> String {
        serde_json::to_string(&self.chains[first]).unwrap_or_default()
    }

    /// Each child's `_subagent_id`, and the call of the delegation that started it.
    fn delegations(&self, first: &str) -> rusqlite::Result<HashMap<String, String>> {
        let mut statement = self.connection.prepare(
            "SELECT tool_call_id, content FROM messages
             WHERE session_id IN (SELECT value FROM json_each(?1))
               AND tool_name = 'delegate_task' AND tool_call_id IS NOT NULL",
        )?;
        let mut rows = statement.query([self.ids(first)])?;
        let mut started = HashMap::new();
        while let Some(row) = rows.next()? {
            let call: String = row.get(0)?;
            let content: Option<String> = row.get(1)?;
            let Some(result) = content.and_then(|text| serde_json::from_str::<Value>(&text).ok())
            else {
                continue;
            };
            for id in result
                .get("subagent_ids")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                started.insert(id.to_owned(), call.clone());
            }
        }
        Ok(started)
    }

    fn task(&self, first: &str) -> rusqlite::Result<Option<String>> {
        let content: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT content FROM messages
                 WHERE session_id IN (SELECT value FROM json_each(?1)) AND role = 'user'
                 ORDER BY id LIMIT 1",
                [self.ids(first)],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|why| match why {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                why => Err(why),
            })?;
        Ok(content.flatten().and_then(|text| {
            text.lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_owned)
        }))
    }

    fn session(&self, first: &str) -> rusqlite::Result<Session> {
        let info = self.info(first);
        let chain = &self.chains[first];
        let children = self.children(first);
        let started = if children.is_empty() {
            HashMap::new()
        } else {
            self.delegations(first)?
        };
        let mut sub_agents = Vec::new();
        for (id, lead) in &children {
            let child = self.info(id);
            sub_agents.push(SubAgent {
                external_id: id.clone(),
                lead: lead.clone(),
                name: None,
                task: self.task(id)?,
                started_at: Some(child.started),
                started_by: child
                    .mark("_subagent_id")
                    .and_then(|sub| started.get(sub).cloned()),
                transcripts: Vec::new(),
            });
        }
        let updated = chain
            .iter()
            .chain(children.iter().flat_map(|(id, _)| self.chains[id].iter()))
            .map(|id| self.info(id).updated)
            .max()
            .unwrap_or(info.updated);
        Ok(Session {
            external_id: first.to_owned(),
            cwd: chain.iter().rev().find_map(|id| self.info(id).cwd.clone()),
            title: chain
                .iter()
                .rev()
                .find_map(|id| self.info(id).title.clone()),
            created_at: Some(info.started),
            updated_at: updated,
            transcripts: Vec::new(),
            parent: info
                .mark("_branched_from")
                .filter(|parent| self.infos.contains_key(*parent))
                .map(|parent| self::first(&self.infos, parent)),
            app: info.created_source.as_deref() == Some("desktop") || info.source == "gui",
            sub_agents,
        })
    }
}

/// The first session of the chain `id` continues.
fn first(infos: &HashMap<String, Info>, id: &str) -> String {
    let mut at = id;
    let mut seen = HashSet::new();
    while let Some(from) = infos[at]
        .mark("_compressed_from")
        .filter(|from| infos.contains_key(*from))
    {
        if !seen.insert(at) {
            break;
        }
        at = from;
    }
    at.to_owned()
}

/// Where a read ended: the last row read, and how many rows up to it had been taken back
/// (`active = 0` without a compaction), which a rewind changes.
#[derive(Serialize, Deserialize)]
struct Cursor {
    v: u32,
    id: i64,
    gone: i64,
}

pub struct Home {
    root: PathBuf,
    stores: Vec<PathBuf>,
}

impl Home {
    pub fn new(root: &Path) -> Self {
        let mut stores = vec![root.join("state.db")];
        if let Ok(profiles) = std::fs::read_dir(root.join("profiles")) {
            let mut more: Vec<PathBuf> = profiles
                .flatten()
                .map(|profile| profile.path().join("state.db"))
                .collect();
            more.sort();
            stores.extend(more);
        }
        stores.retain(|store| store.is_file());
        Self {
            root: root.to_owned(),
            stores,
        }
    }

    /// The store whose chain holds `id`, and that chain's first id.
    fn find(&self, id: &str) -> Result<(Store, String), Error> {
        for path in &self.stores {
            let store = Store::load(path).map_err(failed)?;
            if store.infos.contains_key(id) {
                let first = first(&store.infos, id);
                return Ok((store, first));
            }
        }
        Err(not_found(id))
    }

    pub fn list(&self, list: List) -> Result<Listed, Error> {
        let mut sessions = Vec::new();
        let mut excluded = Vec::new();
        let mut complete = true;
        for path in &self.stores {
            let Ok(store) = Store::load(path) else {
                complete = false;
                continue;
            };
            let mut firsts: Vec<&String> = store.chains.keys().collect();
            firsts.sort();
            for first in firsts {
                if store.subagent(first) {
                    continue;
                }
                if store.headless(first) {
                    excluded.push(first.clone());
                    continue;
                }
                match store.session(first) {
                    Ok(session)
                        if list
                            .seen_since
                            .is_none_or(|since| session.updated_at >= since) =>
                    {
                        sessions.push(session)
                    }
                    Ok(_) => {}
                    Err(_) => complete = false,
                }
            }
        }
        Ok(Listed {
            sessions,
            excluded,
            complete,
            cursor: None,
            after: None,
            gone: Vec::new(),
        })
    }

    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let (store, first) = self.find(&locate.external_id)?;
        if store.subagent(&first) || store.headless(&first) {
            return Err(not_found(&locate.external_id));
        }
        Ok(Located {
            session: store.session(&first).map_err(failed)?,
        })
    }

    pub fn read(&self, read: Read) -> Result<Events, Error> {
        let (store, conversation) = self.find(&read.external_id)?;
        if store.subagent(&conversation) {
            return Err(not_found(&read.external_id));
        }
        let target = match &read.sub_agent {
            None => conversation,
            Some(sub) => store
                .infos
                .contains_key(sub)
                .then(|| self::first(&store.infos, sub))
                .filter(|first| store.subagent(first) && store.conversation(first) == conversation)
                .ok_or_else(|| not_found(sub))?,
        };
        let cursor = match &read.cursor {
            Some(text) => Some(
                serde_json::from_str::<Cursor>(text)
                    .map_err(|why| Error::invalid(format!("cursor: {why}")))?,
            ),
            None => None,
        };
        self.events(&store, &target, cursor, read.tail)
            .map_err(failed)
    }

    fn events(
        &self,
        store: &Store,
        first: &str,
        cursor: Option<Cursor>,
        tail: Option<u64>,
    ) -> rusqlite::Result<Events> {
        let ids = store.ids(first);
        let connection = &store.connection;
        let gone = |up_to: i64| -> rusqlite::Result<i64> {
            connection.query_row(
                "SELECT count(*) FROM messages
                 WHERE session_id IN (SELECT value FROM json_each(?1))
                   AND active = 0 AND compacted = 0 AND id <= ?2",
                params![ids, up_to],
                |row| row.get(0),
            )
        };
        let reset = match &cursor {
            Some(cursor) => gone(cursor.id)? != cursor.gone,
            None => false,
        };
        let after = match (&cursor, reset, tail) {
            (Some(cursor), false, _) => cursor.id,
            (None, _, Some(tail)) => self.tail_start(connection, &ids, tail)?,
            _ => 0,
        };
        let whole = after == 0;
        let from_start = cursor.is_none() || reset || whole;

        // The message ids already read: the rows an in-place compaction writes again keep them.
        let mut seen: HashSet<String> = HashSet::new();
        if after > 0 {
            let mut statement = connection.prepare(
                "SELECT message_uid, id FROM messages
                 WHERE session_id IN (SELECT value FROM json_each(?1))
                   AND NOT (active = 0 AND compacted = 0) AND id <= ?2",
            )?;
            let mut rows = statement.query(params![ids, after])?;
            while let Some(row) = rows.next()? {
                seen.insert(uid(row.get_ref(0)?, row.get(1)?));
            }
        }

        let servers = mcp::names(&self.root);
        let children: HashMap<String, Child> = store
            .children(store.conversation(first).as_str())
            .into_iter()
            .filter_map(|(id, _)| {
                let info = store.info(&id);
                let sub = info.mark("_subagent_id")?.to_owned();
                Some((
                    sub,
                    Child {
                        ended: store.chains[&id].iter().all(|id| store.info(id).ended),
                        external_id: id,
                    },
                ))
            })
            .collect();
        let mut fold = Fold {
            events: Vec::new(),
            servers: &servers,
            children: &children,
            latest: 0,
        };
        let info = store.info(first);
        let chain: Vec<&Info> = store.chains[first]
            .iter()
            .map(|id| store.info(id))
            .collect();
        let model = chain.iter().rev().find_map(|info| info.model.clone());
        if from_start && let Some(model) = &model {
            fold.events.push(Event::Model {
                at: info.started,
                model: model.clone(),
                provider: chain.iter().rev().find_map(|info| info.provider.clone()),
                thinking: None,
            });
        }

        let mut statement = connection.prepare(
            "SELECT id, role, content, tool_call_id, tool_calls, tool_name, timestamp,
                    finish_reason, reasoning, reasoning_content, _compressed_summary,
                    display_kind, display_metadata, api_content, message_uid
             FROM messages
             WHERE session_id IN (SELECT value FROM json_each(?1))
               AND NOT (active = 0 AND compacted = 0) AND id > ?2
             ORDER BY id",
        )?;
        let mut rows = statement.query(params![ids, after])?;
        let mut last = after;
        let mut weight: u64 = 0;
        let mut more = false;
        let mut read_any = false;
        while let Some(row) = rows.next()? {
            if weight >= BATCH {
                more = true;
                break;
            }
            let id: i64 = row.get(0)?;
            last = id;
            read_any = true;
            let uid = uid(row.get_ref(14)?, id);
            let summary = row.get::<_, Option<i64>>(10)?.unwrap_or(0) != 0;
            if !seen.insert(uid.clone()) && !summary {
                continue;
            }
            let content: Option<String> = row.get(2)?;
            let tool_calls: Option<String> = row.get(4)?;
            let reasoning: Option<String> = row
                .get::<_, Option<String>>(8)?
                .filter(|r| !r.trim().is_empty())
                .or(row
                    .get::<_, Option<String>>(9)?
                    .filter(|r| !r.trim().is_empty()));
            let content = content.unwrap_or_default();
            weight += (content.len()
                + tool_calls.as_ref().map_or(0, String::len)
                + reasoning.as_ref().map_or(0, String::len)) as u64;
            fold.row(&Row {
                role: row.get(1)?,
                content,
                tool_call_id: row.get(3)?,
                tool_calls,
                tool_name: row.get(5)?,
                at: ms(row.get::<_, Option<f64>>(6)?.unwrap_or(0.0)),
                finish_reason: row.get(7)?,
                reasoning,
                summary,
                display_kind: row.get(11)?,
                display_metadata: row.get(12)?,
                api_content: row.get(13)?,
                uid,
            });
        }
        drop(rows);

        // Hermes counts tokens per session, not per message: the chain's totals.
        if read_any {
            let sum = |at: usize| chain.iter().map(|info| info.tokens[at]).sum::<u64>();
            fold.events.push(Event::Tokens {
                at: chain
                    .iter()
                    .map(|info| info.updated)
                    .max()
                    .unwrap_or(info.updated)
                    .max(fold.latest),
                count: Count::Total,
                response: None,
                model,
                input: sum(0) + sum(1) + sum(2),
                cached: sum(1),
                cache_write: Some(sum(2)),
                output: sum(3),
                reasoning: sum(4),
                context_left: None,
            });
        }
        let cursor = Cursor {
            v: 1,
            id: last,
            gone: gone(last)?,
        };
        Ok(Events {
            cursor: serde_json::to_string(&cursor).unwrap_or_default(),
            events: fold.events,
            more,
            reset,
            whole,
            after: None,
        })
    }

    /// The row after which the newest rows within `tail` bytes start.
    fn tail_start(&self, connection: &Connection, ids: &str, tail: u64) -> rusqlite::Result<i64> {
        let mut statement = connection.prepare(
            "SELECT id, ifnull(length(content), 0) + ifnull(length(tool_calls), 0)
                        + ifnull(length(reasoning), 0)
             FROM messages
             WHERE session_id IN (SELECT value FROM json_each(?1))
               AND NOT (active = 0 AND compacted = 0)
             ORDER BY id DESC",
        )?;
        let mut rows = statement.query([ids])?;
        let mut total: u64 = 0;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            total += row.get::<_, i64>(1)?.max(0) as u64;
            if total > tail {
                return Ok(id);
            }
        }
        Ok(0)
    }
}

/// A row's `message_uid` as lowercase hex text, whether Hermes stored it as text or bytes;
/// the row's own id where it has none.
fn uid(value: ValueRef<'_>, id: i64) -> String {
    match value {
        ValueRef::Text(text) => String::from_utf8_lossy(text).to_lowercase(),
        ValueRef::Blob(bytes) => bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        _ => format!("row-{id}"),
    }
}
