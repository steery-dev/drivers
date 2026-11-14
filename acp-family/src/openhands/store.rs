//! OpenHands' conversations: each a folder `<conversations>/<id>`, `<id>` the conversation's
//! UUID as 32 hex digits, holding `base_state.json` and `events/event-<NNNNN>-<event id>.json`
//! (`openhands_cli/conversations/store/local.py`). Steery names a conversation by the UUID in
//! its hyphenated form, as `base_state.json`, OpenHands' ACP server and its hooks name it.
//!
//! OpenHands writes each event file once, whole, and never again, so a read's cursor is the
//! number of the next event file. A conversation that no person typed into (an ACP session
//! opened and never prompted) is excluded.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use steery_driver_common::mcp::json;
use steery_sdk::wire::harness::{Event, Events, List, Listed, Locate, Located, Read, Session};
use steery_sdk::{Error, refusal};

use super::events::{self, Facts, Record};

/// How long a conversation stands still before a turn it leaves open, while
/// `base_state.json` says no run goes on, is read as interrupted.
pub const SETTLE: Duration = Duration::from_secs(60);

/// The statuses of a conversation whose run goes on (`ConversationExecutionStatus`).
const RUNNING: [&str; 2] = ["running", "waiting_for_confirmation"];

/// Each folder's head, by when its events and state last changed as it was read.
type Heads = HashMap<PathBuf, ((u64, u64), Head)>;

pub struct Store {
    root: PathBuf,
    heads: Mutex<Heads>,
}

/// What a conversation's folder says of it.
#[derive(Clone)]
pub struct Head {
    pub id: String,
    pub cwd: Option<PathBuf>,
    title: Option<String>,
    created_at: Option<u64>,
    person: bool,
}

/// One event file, by its number.
struct File {
    index: u64,
    path: PathBuf,
}

impl Store {
    /// The store under OpenHands' conversations folder.
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            heads: Mutex::default(),
        }
    }

    /// The folder of conversation `id`, in either of its forms.
    pub fn folder(&self, id: &str) -> PathBuf {
        self.root.join(id.replace('-', ""))
    }

    fn folders(&self) -> Vec<PathBuf> {
        let mut folders: Vec<PathBuf> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        folders.sort();
        folders
    }

    /// The folder's head, read again only once it changed.
    fn head(&self, folder: &Path) -> Option<Head> {
        let stamp = stamp(folder)?;
        let mut heads = self.heads.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((kept, head)) = heads.get(folder)
            && *kept == stamp
        {
            return Some(head.clone());
        }
        let state = json(&folder.join("base_state.json"))?;
        let files = files(folder);
        let created_at = files
            .first()
            .and_then(|file| json(&file.path))
            .and_then(|first| first["timestamp"].as_str().and_then(events::millis));
        let title = files
            .iter()
            .filter_map(|file| json(&file.path))
            .find_map(|value| events::prompt(&value));
        let id = state["id"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| hyphenated(folder.file_name()?.to_str()?))?;
        let head = Head {
            id,
            cwd: state["workspace"]["working_dir"]
                .as_str()
                .filter(|cwd| cwd.starts_with('/'))
                .map(PathBuf::from),
            person: title.is_some(),
            title: title.and_then(|title| title.lines().next().map(str::to_owned)),
            created_at,
        };
        heads.insert(folder.to_path_buf(), (stamp, head.clone()));
        Some(head)
    }

    fn session(folder: &Path, head: &Head) -> Session {
        Session {
            external_id: head.id.clone(),
            cwd: head.cwd.clone(),
            title: head.title.clone(),
            created_at: head.created_at,
            updated_at: stamp(folder).map_or(0, |(events, state)| events.max(state)),
            transcripts: vec![folder.join("base_state.json"), folder.join("events")],
            parent: None,
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
        for folder in self.folders() {
            let Some(head) = self.head(&folder) else {
                continue;
            };
            if !seen.insert(head.id.clone()) {
                complete = false;
            }
            if !head.person {
                excluded.push(head.id);
                continue;
            }
            let session = Self::session(&folder, &head);
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

    /// The folder of the conversation `id`, where a person typed into it.
    pub fn conversation(&self, id: &str) -> Result<(PathBuf, Head), Error> {
        let folder = self.folder(id);
        self.head(&folder)
            .filter(|head| head.person && head.id.replace('-', "") == id.replace('-', ""))
            .map(|head| (folder, head))
            .ok_or_else(|| not_found(id))
    }

    /// `sessions/locate`.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let (folder, head) = self.conversation(&locate.external_id)?;
        Ok(Located {
            session: Self::session(&folder, &head),
        })
    }

    /// The conversation's state and its event files, read whole, in order.
    pub fn records(&self, id: &str) -> Result<(Value, Vec<Record>), Error> {
        let (folder, _) = self.conversation(id)?;
        let state = json(&folder.join("base_state.json")).unwrap_or(Value::Null);
        let records = files(&folder)
            .into_iter()
            .filter_map(|file| {
                let bytes = std::fs::read(&file.path).ok()?;
                Some(Record {
                    index: file.index,
                    len: bytes.len() as u64,
                    value: serde_json::from_slice(&bytes).ok()?,
                })
            })
            .collect();
        Ok((state, records))
    }

    /// `sessions/read`. The cursor is the number of the next event file, and the turn that
    /// the read ended as interrupted while the conversation stood still, so that its later
    /// end is not said again.
    pub fn read(&self, read: Read, servers: Vec<String>) -> Result<Events, Error> {
        if let Some(child) = &read.sub_agent {
            return Err(Error::refused(
                refusal::NOT_FOUND,
                format!("OpenHands keeps no sub-agent `{child}` this driver reads"),
            ));
        }
        let (folder, _) = self.conversation(&read.external_id)?;
        let (state, records) = self.records(&read.external_id)?;
        let facts = Facts::of(&state, servers);
        let mut parsed = events::parse(&records, &facts);
        let next = records.last().map_or(0, |record| record.index + 1);
        let status = state["execution_status"].as_str().unwrap_or_default();
        parsed.settle(status, next);
        let (mut from, settled) = match read.cursor.as_deref() {
            None => (0, None),
            Some(cursor) => {
                let (index, turn) = cursor.split_once('/').unwrap_or((cursor, ""));
                let index = index.parse::<u64>().map_err(|_| {
                    Error::invalid(format!("not a cursor of this driver: {cursor}"))
                })?;
                (index, (!turn.is_empty()).then(|| turn.to_owned()))
            }
        };
        let reset = from > next;
        if reset {
            from = 0;
        }
        let settled = settled.filter(|_| !reset);
        let still = stamp(&folder)
            .map(|(events, state)| events.max(state))
            .and_then(|at| UNIX_EPOCH.checked_add(Duration::from_millis(at)))
            .and_then(|at| SystemTime::now().duration_since(at).ok())
            .is_some_and(|age| age >= SETTLE);
        let mut kept = settled
            .clone()
            .filter(|turn| parsed.open.as_ref() == Some(turn));
        if still && !RUNNING.contains(&status) && kept.is_none() {
            kept = parsed.open.clone();
            parsed.interrupt(next);
        }
        let tail_from = read.tail.map(|tail| tail_start(&records, tail));
        let events: Vec<Event> = parsed
            .said
            .into_iter()
            .filter(|(index, _)| match tail_from {
                Some(at) => *index >= at,
                None => *index >= from,
            })
            .map(|(_, event)| event)
            .filter(|event| {
                !matches!(event, Event::TurnEnded { turn, .. }
                    if settled.is_some() && *turn == settled)
            })
            .collect();
        Ok(Events {
            cursor: match &kept {
                Some(turn) => format!("{next}/{turn}"),
                None => next.to_string(),
            },
            events,
            more: false,
            reset,
            whole: from == 0 && tail_from.is_none(),
            after: parsed.newest.filter(|_| tail_from.is_none()),
        })
    }

    /// Whether the last turn of the conversation is still open, and its events.
    pub fn events(&self, id: &str, servers: Vec<String>) -> Result<(Vec<Event>, bool), Error> {
        let (state, records) = self.records(id)?;
        let facts = Facts::of(&state, servers);
        let mut parsed = events::parse(&records, &facts);
        let next = records.last().map_or(0, |record| record.index + 1);
        parsed.settle(state["execution_status"].as_str().unwrap_or_default(), next);
        let open = parsed.open.is_some();
        Ok((
            parsed.said.into_iter().map(|(_, event)| event).collect(),
            open,
        ))
    }
}

/// The event files of a conversation's folder, in order.
fn files(folder: &Path) -> Vec<File> {
    let mut files: Vec<File> = std::fs::read_dir(folder.join("events"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let index = name
                .to_str()?
                .strip_prefix("event-")?
                .split_once('-')?
                .0
                .parse()
                .ok()?;
            Some(File {
                index,
                path: entry.path(),
            })
        })
        .collect();
    files.sort_by_key(|file| file.index);
    files
}

/// The first event file within `tail` bytes of the end that a person's prompt begins, else
/// the first after the bound.
fn tail_start(records: &[Record], tail: u64) -> u64 {
    let mut bytes = 0u64;
    let mut start = records.len();
    for (at, record) in records.iter().enumerate().rev() {
        bytes += record.len;
        if bytes > tail {
            break;
        }
        start = at;
    }
    let at = (start..records.len())
        .find(|&at| events::prompt(&records[at].value).is_some())
        .unwrap_or(start);
    records
        .get(at)
        .map_or_else(|| records.last().map_or(0, |r| r.index + 1), |r| r.index)
}

/// When the folder's events and its state last changed, in milliseconds since 1970: the
/// events folder changes as a file is added to it.
fn stamp(folder: &Path) -> Option<(u64, u64)> {
    let changed = |path: PathBuf| {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .map(|at| at.as_millis() as u64)
    };
    Some((
        changed(folder.join("events")).unwrap_or(0),
        changed(folder.join("base_state.json"))?,
    ))
}

/// The hyphenated form of a UUID written as 32 hex digits.
fn hyphenated(hex: &str) -> Option<String> {
    (hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| {
        format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        )
    })
}

fn not_found(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("OpenHands has no conversation `{id}` that a person typed into here"),
    )
}
