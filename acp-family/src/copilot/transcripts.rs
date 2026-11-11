//! Copilot's conversations: each one a folder `session-state/<id>/` of its home, holding
//! `workspace.yaml` (its folder, title and times) and `events.jsonl`, its transcript. A
//! sub-agent's records are in its lead's transcript, each marked with the call that started
//! it. A resumed conversation goes on in the same file.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use steery_driver_common::lines::{BATCH, Line, RECORD, capped, consumed, first_whole};
use steery_driver_common::quiet::Stamp;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use super::events::{self, Carried, End, Starts, Turn, millis, one_line, text};

/// The client that Copilot names for a program driving it through its SDK: no person is at
/// such a run.
const HEADLESS: &str = "sdk";

/// The version of this driver's cursors.
const CURSOR: &str = "c1";

pub struct Store {
    root: PathBuf,
    /// What each transcript's records said of its conversation, as far as it was scanned.
    scans: Mutex<HashMap<PathBuf, Scan>>,
    /// The shells left running in each transcript, by its path.
    starts: Mutex<HashMap<PathBuf, Starts>>,
}

/// One transcript scanned up to `scanned` bytes, the file it was then.
#[derive(Clone, Default)]
struct Scan {
    file: (u64, u64),
    scanned: u64,
    /// `session.start`'s folder and time.
    cwd: Option<PathBuf>,
    started: Option<u64>,
    sub_agents: Vec<SubAgent>,
    /// What each `task` call asked, by the call, for the sub-agent it starts.
    asked: HashMap<String, String>,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            scans: Mutex::default(),
            starts: Mutex::default(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn folder(&self) -> PathBuf {
        self.root.join("session-state")
    }

    /// `sessions/list`: every conversation updated since `seen_since`. Copilot keeps every
    /// transcript in its home, so the transcripts that moments named add nothing.
    pub fn list(&self, list: List) -> Listed {
        let mut sessions = Vec::new();
        let mut excluded = Vec::new();
        let mut complete = true;
        let mut seen = Vec::new();
        let entries = match std::fs::read_dir(self.folder()) {
            Ok(entries) => entries,
            Err(why) => {
                complete = why.kind() == std::io::ErrorKind::NotFound;
                return listed(sessions, excluded, complete);
            }
        };
        for entry in entries.flatten() {
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let transcript = entry.path().join("events.jsonl");
            if !transcript.is_file() {
                continue;
            }
            seen.push(transcript);
            match self.session(&id) {
                Ok((session, headless)) => {
                    if list
                        .seen_since
                        .is_some_and(|since| session.updated_at < since)
                    {
                        continue;
                    }
                    if headless {
                        excluded.push(id);
                    } else {
                        sessions.push(session);
                    }
                }
                Err(_) => complete = false,
            }
        }
        lock(&self.scans).retain(|path, _| seen.contains(path));
        lock(&self.starts).retain(|path, _| seen.contains(path));
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        listed(sessions, excluded, complete)
    }

    /// `sessions/locate`.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        self.session(&locate.external_id)
            .map(|(session, _)| Located { session })
    }

    /// The conversation `id`, and whether no person is at it.
    fn session(&self, id: &str) -> Result<(Session, bool), Error> {
        let transcript = self.transcript(id)?;
        let stamp = Stamp::at(&transcript).ok_or_else(|| not_found(id))?;
        let scan = self.scan(&transcript, stamp).map_err(|why| {
            Error::refused(refusal::REFUSED, format!("{}: {why}", transcript.display()))
        })?;
        let workspace = std::fs::read_to_string(transcript.with_file_name("workspace.yaml"))
            .map(|said| Workspace::parse(&said))
            .unwrap_or_default();
        let headless = workspace.client.as_deref() == Some(HEADLESS)
            && workspace
                .last_client
                .as_deref()
                .is_none_or(|last| last == HEADLESS);
        let updated_at = millis_of(stamp.modified()).max(workspace.updated_at.unwrap_or(0));
        Ok((
            Session {
                external_id: id.to_owned(),
                cwd: workspace.cwd.or(scan.cwd),
                title: workspace.name,
                created_at: workspace.created_at.or(scan.started),
                updated_at,
                transcripts: vec![transcript],
                parent: None,
                app: false,
                sub_agents: scan.sub_agents,
            },
            headless,
        ))
    }

    /// The transcript of the conversation `id`.
    fn transcript(&self, id: &str) -> Result<PathBuf, Error> {
        let plain = !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', '\0']);
        let transcript = self.folder().join(id).join("events.jsonl");
        if plain && transcript.is_file() {
            Ok(transcript)
        } else {
            Err(not_found(id))
        }
    }

    /// The transcript's records, scanned on from where the last scan stopped, for what they
    /// say of the conversation: its folder, when it began, its sub-agents.
    fn scan(&self, transcript: &Path, stamp: Stamp) -> std::io::Result<Scan> {
        let kept = lock(&self.scans).get(transcript).cloned();
        let mut scan = match kept {
            Some(scan) if scan.file == stamp.file() && scan.scanned <= stamp.length() => scan,
            _ => Scan {
                file: stamp.file(),
                ..Scan::default()
            },
        };
        if scan.scanned < stamp.length() {
            let mut open = File::open(transcript)?;
            open.seek(SeekFrom::Start(scan.scanned))?;
            let total = stamp.length() - scan.scanned;
            let mut reader = BufReader::new(open.take(total));
            let mut line = Vec::new();
            loop {
                let read = capped(&mut reader, &mut line, RECORD)?;
                if read == Line::Partial {
                    break;
                }
                scan.scanned = stamp.length() - total + consumed(&reader, total);
                if read == Line::Whole
                    && let Ok(record) = serde_json::from_slice::<Value>(&line)
                {
                    scan.note(&record);
                }
            }
        }
        lock(&self.scans).insert(transcript.to_owned(), scan.clone());
        Ok(scan)
    }

    /// `sessions/read`.
    pub fn read(&self, read: Read) -> Result<Events, Error> {
        let transcript = self.transcript(&read.external_id)?;
        let stamp = Stamp::at(&transcript).ok_or_else(|| not_found(&read.external_id))?;
        let agent = read.sub_agent.as_deref();
        if let Some(agent) = agent {
            let scan = self
                .scan(&transcript, stamp)
                .map_err(|why| Error::refused(refusal::REFUSED, why.to_string()))?;
            if !scan.sub_agents.iter().any(|sub| sub.external_id == agent) {
                return Err(Error::refused(
                    refusal::NOT_FOUND,
                    format!("no sub-agent {agent} in {}", read.external_id),
                ));
            }
        }
        let starts = lock(&self.starts)
            .entry(transcript.clone())
            .or_default()
            .clone();
        self.read_from(&transcript, stamp, agent, &starts, &read)
            .map_err(|why| Error::refused(refusal::REFUSED, why.to_string()))?
    }

    fn read_from(
        &self,
        transcript: &Path,
        stamp: Stamp,
        agent: Option<&str>,
        starts: &Starts,
        read: &Read,
    ) -> std::io::Result<Result<Events, Error>> {
        let open = File::open(transcript)?;
        let length = open.metadata()?.len();
        let first = first_id(&open, length)?;
        let mut reset = false;
        let (from, carried) = match (&read.cursor, read.tail) {
            (_, Some(tail)) => (
                first_whole(&open, length.saturating_sub(tail), length)?,
                Carried {
                    turn: Turn::Unknown,
                    id: None,
                },
            ),
            (Some(cursor), None) => match Cursor::parse(cursor) {
                Some(cursor) if cursor.at <= length && cursor.first == first => {
                    (cursor.at, cursor.carried)
                }
                Some(_) => {
                    reset = true;
                    (0, closed())
                }
                None => {
                    return Ok(Err(Error::invalid(format!(
                        "this driver did not give the cursor `{cursor}`"
                    ))));
                }
            },
            (None, None) => (0, closed()),
        };
        let (records, ends, more) = records(&open, from, length)?;
        let still = SystemTime::now()
            .duration_since(stamp.modified())
            .is_ok_and(|since| since >= events::SETTLE);
        let end = if still || more {
            End::Decided
        } else {
            End::Open
        };
        let values: Vec<Option<Value>> = records.iter().map(|(value, _)| value.clone()).collect();
        let said = events::read(agent, starts, &values, carried, end);
        let through = said.consumed.checked_sub(1).map_or(from, |last| ends[last]);
        let after = (read.tail.is_none())
            .then(|| {
                records[..said.consumed]
                    .iter()
                    .rev()
                    .find_map(|(_, id)| id.clone())
            })
            .flatten();
        Ok(Ok(Events {
            cursor: Cursor {
                at: through,
                first,
                carried: said.carried,
            }
            .said(),
            events: said.events,
            more: more && said.consumed == records.len(),
            reset,
            whole: from == 0,
            after,
        }))
    }
}

/// The whole records from `from`, each with its `id`, the offset each ends at, and whether
/// more remain past the batch.
#[allow(clippy::type_complexity)]
fn records(
    open: &File,
    from: u64,
    length: u64,
) -> std::io::Result<(Vec<(Option<Value>, Option<String>)>, Vec<u64>, bool)> {
    let mut open = open;
    open.seek(SeekFrom::Start(from))?;
    let total = length - from;
    let mut reader = BufReader::new(open.take(total));
    let mut line = Vec::new();
    let mut records = Vec::new();
    let mut ends = Vec::new();
    let mut more = false;
    loop {
        let read = capped(&mut reader, &mut line, RECORD)?;
        if read == Line::Partial {
            break;
        }
        let end = from + consumed(&reader, total);
        let value = (read == Line::Whole)
            .then(|| serde_json::from_slice::<Value>(&line).ok())
            .flatten();
        let id = value
            .as_ref()
            .and_then(|value| text(value, "id"))
            .map(str::to_owned);
        records.push((value, id));
        ends.push(end);
        if end - from >= BATCH && end < length {
            more = true;
            break;
        }
    }
    Ok((records, ends, more))
}

/// The `id` of the transcript's first record: a file written anew has another.
fn first_id(open: &File, length: u64) -> std::io::Result<String> {
    let mut open = open;
    open.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(open.take(length));
    let mut line = Vec::new();
    Ok(match capped(&mut reader, &mut line, RECORD)? {
        Line::Whole => serde_json::from_slice::<Value>(&line)
            .ok()
            .and_then(|record| text(&record, "id").map(str::to_owned))
            .unwrap_or_default(),
        _ => String::new(),
    })
}

impl Scan {
    fn note(&mut self, record: &Value) {
        let null = Value::Null;
        let data = record.get("data").unwrap_or(&null);
        let at = text(record, "timestamp").and_then(millis);
        match text(record, "type") {
            Some("session.start") => {
                self.started = text(data, "startTime").and_then(millis).or(at);
                self.cwd = data
                    .pointer("/context/cwd")
                    .and_then(Value::as_str)
                    .map(PathBuf::from);
            }
            Some("tool.execution_start") if text(data, "toolName") == Some("task") => {
                if let (Some(call), Some(prompt)) = (
                    text(data, "toolCallId"),
                    data.pointer("/arguments/prompt").and_then(Value::as_str),
                ) && let Some(first) = prompt.lines().find(|line| !line.trim().is_empty())
                {
                    self.asked.insert(call.to_owned(), one_line(first));
                }
            }
            Some("subagent.started") => {
                let Some(call) = text(data, "toolCallId") else {
                    return;
                };
                if self.sub_agents.iter().any(|sub| sub.external_id == call) {
                    return;
                }
                self.sub_agents.push(SubAgent {
                    external_id: call.to_owned(),
                    lead: text(data, "parentToolCallId").map(str::to_owned),
                    name: text(data, "agentDisplayName")
                        .or_else(|| text(data, "agentName"))
                        .map(str::to_owned),
                    task: self
                        .asked
                        .remove(call)
                        .or_else(|| text(data, "agentDescription").map(one_line)),
                    started_at: at,
                    started_by: Some(call.to_owned()),
                    transcripts: Vec::new(),
                });
            }
            _ => {}
        }
    }
}

/// What `workspace.yaml` says: flat `key: value` lines.
#[derive(Default)]
struct Workspace {
    cwd: Option<PathBuf>,
    name: Option<String>,
    client: Option<String>,
    last_client: Option<String>,
    created_at: Option<u64>,
    updated_at: Option<u64>,
}

impl Workspace {
    fn parse(said: &str) -> Self {
        let mut workspace = Self::default();
        for line in said.lines() {
            let Some((key, value)) = line.split_once(": ") else {
                continue;
            };
            let value = scalar(value);
            match key {
                "cwd" => workspace.cwd = Some(value.into()),
                "name" if !value.trim().is_empty() => workspace.name = Some(one_line(&value)),
                "client_name" => workspace.client = Some(value),
                "last_client_name" => workspace.last_client = Some(value),
                "created_at" => workspace.created_at = millis(&value),
                "updated_at" => workspace.updated_at = millis(&value),
                _ => {}
            }
        }
        workspace
    }
}

/// A YAML scalar on one line: plain, `'single'` (`''` is a quote) or `"double"` quoted.
fn scalar(value: &str) -> String {
    let value = value.trim();
    if let Some(inner) = value
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        return inner.replace("''", "'");
    }
    if value.starts_with('"')
        && let Ok(Value::String(inner)) = serde_json::from_str(value)
    {
        return inner;
    }
    value.to_owned()
}

/// Where a read stopped: the offset, the transcript's first record then, and the turn.
struct Cursor {
    at: u64,
    first: String,
    carried: Carried,
}

impl Cursor {
    fn said(&self) -> String {
        let turn = match self.carried.turn {
            Turn::Closed => "c",
            Turn::Open => "o",
            Turn::Unknown => "u",
        };
        format!(
            "{CURSOR}:{}:{turn}:{}:{}",
            self.at,
            self.first,
            self.carried.id.as_deref().unwrap_or_default()
        )
    }

    fn parse(said: &str) -> Option<Self> {
        let mut parts = said.splitn(5, ':');
        if parts.next()? != CURSOR {
            return None;
        }
        let at = parts.next()?.parse().ok()?;
        let turn = match parts.next()? {
            "c" => Turn::Closed,
            "o" => Turn::Open,
            "u" => Turn::Unknown,
            _ => return None,
        };
        let first = parts.next()?.to_owned();
        let id = parts.next()?;
        Some(Self {
            at,
            first,
            carried: Carried {
                turn,
                id: (!id.is_empty()).then(|| id.to_owned()),
            },
        })
    }
}

fn closed() -> Carried {
    Carried {
        turn: Turn::Closed,
        id: None,
    }
}

fn listed(sessions: Vec<Session>, excluded: Vec<String>, complete: bool) -> Listed {
    Listed {
        sessions,
        excluded,
        complete,
        cursor: None,
        after: None,
        gone: Vec::new(),
    }
}

fn not_found(id: &str) -> Error {
    Error::refused(refusal::NOT_FOUND, format!("no conversation {id}"))
}

fn millis_of(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH).map_or(0, |since| {
        u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
