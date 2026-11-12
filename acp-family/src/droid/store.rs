//! Droid's conversations: each `<id>.jsonl` under `~/.factory/sessions/`, in one of three
//! layouts that Droid's own session index names: `sessions/<folder>/<id>.jsonl`, where
//! `<folder>` is the conversation's folder with each `/` as `-`; `sessions/<id>.jsonl`; and
//! `sessions/btw/<id>.jsonl`, a side question forked from a conversation. Beside each,
//! `<id>.settings.json` holds the model and the tokens the conversation spent.
//!
//! A file whose records hold no prompt of a person (Droid writes `session_start` as it opens
//! a conversation that may never get one) is excluded.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use memchr::memmem;
use serde_json::Value;
use steery_sdk::wire::harness::{
    Count, Event, Events, List, Listed, Locate, Located, Read, Session, TurnState,
};
use steery_sdk::{Error, refusal};

use super::events::{self, Facts};

/// How long a file stands still before a turn it leaves open is read as interrupted: Droid
/// writes every message as it comes, and an outcome as the turn ends.
pub const SETTLE: Duration = Duration::from_secs(60);

/// What marks a person's prompt in a record: Droid writes the interaction mode only there.
const PROMPT: &[u8] = b"\"interactionMode\":";

/// Each file's head, by the file's length and when it last changed as it was read.
type Heads = HashMap<PathBuf, ((u64, u64), Head)>;

pub struct Store {
    root: PathBuf,
    heads: Mutex<Heads>,
}

/// What a file's first records say of its conversation.
#[derive(Clone)]
pub struct Head {
    id: String,
    title: Option<String>,
    cwd: Option<PathBuf>,
    parent: Option<String>,
    created_at: Option<u64>,
    person: bool,
}

impl Store {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            heads: Mutex::default(),
        }
    }

    fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }

    /// Every session file, in each layout.
    fn files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let entries = |folder: &Path| {
            std::fs::read_dir(folder)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .collect::<Vec<_>>()
        };
        for path in entries(&self.sessions()) {
            if path.is_dir() {
                files.extend(entries(&path).into_iter().filter(|path| jsonl(path)));
            } else if jsonl(&path) {
                files.push(path);
            }
        }
        files.sort();
        files
    }

    /// The file's head, read again only once the file changed.
    fn head(&self, file: &Path) -> Option<Head> {
        let stamp = stamp(file)?;
        let mut heads = self.heads.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((kept, head)) = heads.get(file)
            && *kept == stamp
        {
            return Some(head.clone());
        }
        let bytes = std::fs::read(file).ok()?;
        let mut lines = bytes.split(|&b| b == b'\n');
        let start: Value = serde_json::from_slice(lines.next()?).ok()?;
        if start["type"] != "session_start" {
            return None;
        }
        let text = |key: &str| {
            start[key]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let created_at = lines
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .find_map(|record| record["timestamp"].as_str().and_then(events::millis));
        let head = Head {
            id: text("id").or_else(|| stem(file))?,
            title: text("title"),
            cwd: text("cwd").map(PathBuf::from),
            parent: text("parent"),
            created_at,
            person: memmem::find(&bytes, PROMPT).is_some(),
        };
        heads.insert(file.to_path_buf(), (stamp, head.clone()));
        Some(head)
    }

    fn session(file: &Path, head: &Head) -> Session {
        Session {
            external_id: head.id.clone(),
            cwd: head.cwd.clone(),
            title: head.title.clone(),
            created_at: head.created_at,
            updated_at: stamp(file).map_or(0, |(_, at)| at),
            transcripts: vec![file.to_path_buf()],
            parent: head.parent.clone(),
            app: false,
            sub_agents: Vec::new(),
        }
    }

    /// `sessions/list`.
    pub fn list(&self, list: List) -> Listed {
        let mut sessions = Vec::new();
        let mut excluded = Vec::new();
        let mut seen = HashSet::new();
        let mut complete = true;
        for file in self.files() {
            let Some(head) = self.head(&file) else {
                continue;
            };
            if !seen.insert(head.id.clone()) {
                complete = false;
            }
            if !head.person {
                excluded.push(head.id);
                continue;
            }
            let session = Self::session(&file, &head);
            if list
                .seen_since
                .is_none_or(|since| session.updated_at >= since)
            {
                sessions.push(session);
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

    /// The file of the conversation `id`, where a person typed into it.
    pub fn file(&self, id: &str) -> Result<(PathBuf, Head), Error> {
        let named = format!("{id}.jsonl");
        self.files()
            .into_iter()
            .filter(|file| file.file_name().is_some_and(|name| *name == *named))
            .find_map(|file| {
                let head = self.head(&file)?;
                (head.person && head.id == id).then_some((file, head))
            })
            .ok_or_else(|| not_found(id))
    }

    /// `sessions/locate`.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let (file, head) = self.file(&locate.external_id)?;
        Ok(Located {
            session: Self::session(&file, &head),
        })
    }

    /// `sessions/read`. The cursor is the length read, and the turn that the read ended as
    /// interrupted while the file stood still, so that its later end is not said again.
    pub fn read(&self, read: Read) -> Result<Events, Error> {
        if let Some(child) = &read.sub_agent {
            return Err(Error::refused(
                refusal::NOT_FOUND,
                format!("Droid keeps no sub-agent `{child}` this driver reads"),
            ));
        }
        let (file, _) = self.file(&read.external_id)?;
        let bytes = std::fs::read(&file)
            .map_err(|e| Error::refused(refusal::NOT_FOUND, format!("{}: {e}", file.display())))?;
        let settings = settings(&file);
        let facts = Facts {
            model: settings["model"].as_str().map(str::to_owned),
            provider: settings["providerLock"].as_str().map(str::to_owned),
        };
        let mut parsed = events::parse(&bytes, &facts);
        let (mut from, settled) = match read.cursor.as_deref() {
            None => (0, None),
            Some(cursor) => {
                let (length, turn) = cursor.split_once('/').unwrap_or((cursor, ""));
                let length = length.parse::<u64>().map_err(|_| {
                    Error::invalid(format!("not a cursor of this driver: {cursor}"))
                })?;
                (length, (!turn.is_empty()).then(|| turn.to_owned()))
            }
        };
        let reset = from > parsed.len;
        if reset {
            from = 0;
        }
        let settled = settled.filter(|_| !reset);
        let still = stamp(&file)
            .and_then(|(_, at)| UNIX_EPOCH.checked_add(Duration::from_millis(at)))
            .and_then(|at| SystemTime::now().duration_since(at).ok())
            .is_some_and(|age| age >= SETTLE);
        let mut kept = settled
            .clone()
            .filter(|turn| parsed.open.as_ref() == Some(turn));
        if still
            && let Some(turn) = parsed.open.clone()
            && kept.is_none()
        {
            parsed.said.push(events::Said {
                start: parsed.len,
                end: parsed.len + 1,
                event: Event::TurnEnded {
                    at: parsed.at,
                    turn: Some(turn.clone()),
                    state: TurnState::Interrupted,
                    failure: None,
                },
            });
            kept = Some(turn);
        }
        let tail_from = read.tail.map(|tail| parsed.len.saturating_sub(tail));
        let mut said: Vec<Event> = parsed
            .said
            .into_iter()
            .filter(|said| match tail_from {
                Some(at) => said.start >= at,
                None => said.end > from,
            })
            .map(|said| said.event)
            .filter(|event| {
                !matches!(event, Event::TurnEnded { turn, .. }
                    if settled.is_some() && *turn == settled)
            })
            .collect();
        let whole = from == 0 && tail_from.is_none();
        if whole && let Some(model) = &facts.model {
            said.insert(
                0,
                Event::Model {
                    at: parsed.first.unwrap_or(parsed.at),
                    model: model.clone(),
                    provider: facts.provider.clone(),
                    thinking: settings["reasoningEffort"].as_str().map(str::to_owned),
                },
            );
        }
        if !said.is_empty()
            && let Some(tokens) = tokens(&settings["tokenUsage"], &facts, parsed.at)
        {
            said.push(tokens);
        }
        Ok(Events {
            cursor: match &kept {
                Some(turn) => format!("{}/{turn}", parsed.len),
                None => parsed.len.to_string(),
            },
            events: said,
            more: false,
            reset,
            whole,
            after: parsed.newest.filter(|_| tail_from.is_none()),
        })
    }
}

/// The conversation's tokens so far, as its settings file counts them.
fn tokens(usage: &Value, facts: &Facts, at: u64) -> Option<Event> {
    let count = |key: &str| usage[key].as_u64();
    Some(Event::Tokens {
        at,
        count: Count::Total,
        response: None,
        model: facts.model.clone(),
        input: count("inputTokens")?,
        cached: count("cacheReadTokens").unwrap_or(0),
        cache_write: count("cacheCreationTokens"),
        output: count("outputTokens")?,
        reasoning: count("thinkingTokens").unwrap_or(0),
        context_left: None,
    })
}

/// `<id>.settings.json` beside the file, or null.
fn settings(file: &Path) -> Value {
    std::fs::read(file.with_extension("settings.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

fn jsonl(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
}

fn stem(file: &Path) -> Option<String> {
    Some(file.file_stem()?.to_str()?.to_owned())
}

/// A file's length and when it last changed, in milliseconds since 1970.
fn stamp(file: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(file).ok()?;
    let at = metadata.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((metadata.len(), u64::try_from(at.as_millis()).ok()?))
}

fn not_found(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("Droid keeps no conversation `{id}` that a person typed into here"),
    )
}
