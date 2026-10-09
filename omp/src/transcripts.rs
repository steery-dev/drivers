//! omp's conversations, from their transcripts: `<root>/sessions/<folder>/<started-at>_<id>.jsonl`,
//! and any folder omp was told to keep them in instead (`--session-dir`,
//! `PI_CODING_AGENT_SESSION_DIR`), which the harness's own moments name.
//!
//! A transcript opens with a fixed-width `title` record omp rewrites in place, then the
//! `session` header naming the conversation, its folder and, for a fork, the conversation
//! it came from; then entries, one JSON record per line, appended. A sub-agent's
//! transcript is filed inside its lead's folder, `<conversation>/<job>.jsonl`, and one a
//! sub-agent started inside that one's: those are the conversation's sub-agents, never
//! conversations of their own.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read as _, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use crate::events::{self, Reading, Record, millis, text};

/// How deep sub-agents are filed below their conversation's folder: a sub-agent's, the
/// one it started, and the one that one started (omp 18.4).
const SUB_AGENT_DEPTH: usize = 3;
/// The longest record read; a longer one is stepped over.
const RECORD: usize = 8 << 20;
/// How many bytes of records one `sessions/read` answers at most; `more` says the rest.
const BATCH: u64 = 4 << 20;
/// The records the head of a transcript is read from: its `title` and `session`.
const HEAD: usize = 4;
/// The longest head record read.
const HEAD_RECORD: usize = 64 << 10;

/// omp's conversations here: its agent folder, and the folders outside it its moments
/// named, remembered for as long as this driver runs.
pub struct Store {
    root: PathBuf,
    named: Mutex<BTreeSet<PathBuf>>,
}

/// Every transcript filed as a conversation, by the id it is filed under.
struct Filed {
    by_id: BTreeMap<String, Vec<PathBuf>>,
    /// Every folder was read whole.
    complete: bool,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            named: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `sessions/list`: every conversation updated since `seen_since`.
    pub fn list(&self, list: List) -> Listed {
        self.remember(&list.transcripts);
        let filed = self.filed();
        let mut complete = filed.complete;
        let mut sessions = Vec::new();
        for (id, files) in filed.by_id {
            // Two transcripts answering to one id name it ambiguously: neither is read.
            let [file] = files.as_slice() else {
                complete = false;
                continue;
            };
            let Some(session) = session(&id, file) else {
                continue;
            };
            if list
                .seen_since
                .is_none_or(|since| session.updated_at >= since)
            {
                sessions.push(session);
            }
        }
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Listed {
            sessions,
            excluded: Vec::new(),
            complete,
        }
    }

    /// `sessions/locate`.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let file = self.find(&locate.external_id)?;
        let session = session(&locate.external_id, &file).ok_or_else(|| {
            Error::refused(
                refusal::REFUSED,
                format!("{} cannot be read", file.display()),
            )
        })?;
        Ok(Located { session })
    }

    /// `sessions/read`: the events of a conversation, or of one of its sub-agents.
    pub fn read(&self, read: Read) -> Result<Events, Error> {
        let file = self.find(&read.external_id)?;
        let head = head(&file);
        let (file, fork) = match &read.sub_agent {
            None => {
                let fork = parent(&read.external_id, &head).zip(head.created_at);
                (file, fork)
            }
            Some(agent) => {
                let found = sub_agents(&file)
                    .into_iter()
                    .find(|found| {
                        found.external_id == *agent || found.name.as_deref() == Some(agent)
                    })
                    .ok_or_else(|| {
                        Error::refused(
                            refusal::NOT_FOUND,
                            format!("omp has no sub-agent {agent} in {}", read.external_id),
                        )
                    })?;
                (found.transcripts.into_iter().next().unwrap_or(file), None)
            }
        };
        let reading = Reading {
            blobs: events::blobs(&file, &self.root),
            cwd: head.cwd,
            servers: crate::calls::servers(&self.root),
            fork,
            file,
        };
        read_from(&reading, read.cursor.as_deref(), read.tail)
    }

    /// The one transcript of conversation `id`.
    pub fn find(&self, id: &str) -> Result<PathBuf, Error> {
        let mut files = self.transcripts(id);
        match files.len() {
            0 => Err(Error::refused(
                refusal::NOT_FOUND,
                format!("omp has no conversation {id} here"),
            )),
            1 => Ok(files.remove(0)),
            _ => Err(Error::refused(
                refusal::REFUSED,
                format!("more than one omp transcript answers to {id}"),
            )),
        }
    }

    /// Every transcript filed under `id`: one, where omp's store is as it keeps it.
    pub fn transcripts(&self, id: &str) -> Vec<PathBuf> {
        if id.is_empty() || id.chars().any(char::is_control) {
            return Vec::new();
        }
        self.filed().by_id.remove(id).unwrap_or_default()
    }

    /// Keep the folders of transcripts omp's moments named.
    fn remember(&self, named: &[PathBuf]) {
        let folders = named
            .iter()
            .filter(|file| file.is_absolute() && file.extension().is_some_and(|ext| ext == "jsonl"))
            .filter_map(|file| file.parent())
            .filter_map(|folder| folder.canonicalize().ok());
        self.named
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(folders);
    }

    /// The folders conversations are filed in: each of `<root>/sessions/*`, then the one
    /// `PI_CODING_AGENT_SESSION_DIR` names and those omp's moments named, each once, none a
    /// sub-agent's.
    fn folders(&self) -> (Vec<PathBuf>, bool) {
        let mut complete = true;
        let mut folders = Vec::new();
        let sessions = self.root.join("sessions");
        match std::fs::read_dir(&sessions) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) if entry.file_type().is_ok_and(|kind| kind.is_dir()) => {
                            folders.push(entry.path());
                        }
                        Ok(_) => {}
                        Err(_) => complete = false,
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => complete = false,
        }
        let own = sessions.canonicalize().ok();
        let mut seen: BTreeSet<PathBuf> = folders
            .iter()
            .filter_map(|folder| folder.canonicalize().ok())
            .collect();
        let chosen = std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .and_then(|dir| dir.canonicalize().ok());
        let named = self.named.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for folder in chosen.into_iter().chain(named) {
            let filed_by_omp = own.is_some() && folder.parent() == own.as_deref();
            if filed_by_omp || companion(&folder).is_file() || !seen.insert(folder.clone()) {
                continue;
            }
            folders.push(folder);
        }
        (folders, complete)
    }

    fn filed(&self) -> Filed {
        let (folders, mut complete) = self.folders();
        let mut by_id: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        for folder in folders {
            let Ok(entries) = std::fs::read_dir(&folder) else {
                complete = false;
                continue;
            };
            for entry in entries {
                let Ok(entry) = entry else {
                    complete = false;
                    continue;
                };
                let path = entry.path();
                // A link is never read: it could name a file anywhere.
                if !entry.file_type().is_ok_and(|kind| kind.is_file())
                    || path.extension().is_none_or(|ext| ext != "jsonl")
                {
                    continue;
                }
                if let Some(id) = id_of(&path) {
                    by_id.entry(id.to_owned()).or_default().push(path);
                }
            }
        }
        Filed { by_id, complete }
    }
}

/// The transcript a folder is the companion of: `<folder>.jsonl` beside it. A folder with
/// one holds that transcript's sub-agents.
fn companion(folder: &Path) -> PathBuf {
    let mut file = folder.as_os_str().to_owned();
    file.push(".jsonl");
    PathBuf::from(file)
}

/// The id a transcript is filed under: all that follows the first `_` of its name. The
/// stamp before it holds none, and an id may.
fn id_of(file: &Path) -> Option<&str> {
    Some(file.file_stem()?.to_str()?.split_once('_')?.1)
        .filter(|id| !id.is_empty() && !id.chars().any(char::is_control))
}

/// A conversation as `sessions/list` says it.
fn session(id: &str, file: &Path) -> Option<Session> {
    let updated_at = modified(file)?;
    let head = head(file);
    Some(Session {
        external_id: id.to_owned(),
        parent: parent(id, &head),
        cwd: head.cwd,
        title: head.title,
        created_at: head.created_at,
        updated_at,
        transcripts: vec![file.to_owned()],
        app: false,
        sub_agents: sub_agents(file),
    })
}

/// The conversation a fork came from, as its header names it: by its id, or by its
/// transcript.
fn parent(id: &str, head: &Head) -> Option<String> {
    let named = head.parent.as_deref()?;
    let parent = if named.contains('/') {
        id_of(Path::new(named))?
    } else {
        named
    };
    (!parent.is_empty() && parent != id).then(|| parent.to_owned())
}

/// The sub-agents filed in a conversation's folder, and in theirs: each a transcript
/// whose header names who started it (`parentSession`), named for its job.
fn sub_agents(file: &Path) -> Vec<SubAgent> {
    fn visit(folder: &Path, lead: Option<&str>, depth: usize, out: &mut Vec<SubAgent>) {
        let Ok(entries) = std::fs::read_dir(folder) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|kind| kind.is_file())
                || path.extension().is_none_or(|ext| ext != "jsonl")
            {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let head = head(&path);
            if head.parent.is_none() {
                continue;
            }
            let name = name.to_owned();
            let started = path.with_extension("");
            out.push(SubAgent {
                external_id: head.id.clone().unwrap_or_else(|| name.clone()),
                lead: lead.map(str::to_owned),
                name: Some(name),
                task: None,
                started_at: head.created_at,
                started_by: None,
                transcripts: vec![path],
            });
            if depth < SUB_AGENT_DEPTH {
                visit(&started, head.id.as_deref(), depth + 1, out);
            }
        }
    }
    let mut found = Vec::new();
    visit(&file.with_extension(""), None, 1, &mut found);
    found.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.external_id.cmp(&b.external_id))
    });
    found
}

/// What the head of a transcript records about its conversation.
#[derive(Default)]
pub struct Head {
    pub id: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    pub created_at: Option<u64>,
    /// The `parentSession` its header names: an id, or a transcript's path.
    pub parent: Option<String>,
}

/// The title omp keeps current, else the one the conversation started with, and what the
/// `session` header says.
pub fn head(file: &Path) -> Head {
    let mut found = Head::default();
    let Ok(open) = File::open(file) else {
        return found;
    };
    let mut reader = BufReader::new(open);
    let mut line = Vec::new();
    let mut started = None;
    for _ in 0..HEAD {
        match capped(&mut reader, &mut line, HEAD_RECORD) {
            Ok(Line::Whole) => {}
            Ok(Line::Long) => continue,
            Ok(Line::Partial) | Err(_) => break,
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        match text(&record, "type") {
            Some("title") if found.title.is_none() => {
                found.title = text(&record, "title").and_then(clean);
            }
            Some("session") => {
                found.id = text(&record, "id")
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned);
                started = text(&record, "title").and_then(clean);
                found.cwd = text(&record, "cwd")
                    .map(PathBuf::from)
                    .filter(|cwd| cwd.is_absolute());
                found.created_at = text(&record, "timestamp").and_then(millis);
                found.parent = text(&record, "parentSession")
                    .filter(|parent| !parent.is_empty())
                    .map(str::to_owned);
                break;
            }
            _ => {}
        }
    }
    found.title = found.title.or(started);
    found
}

/// When a file last changed, in milliseconds since 1970.
fn modified(file: &Path) -> Option<u64> {
    let at = std::fs::metadata(file).ok()?.modified().ok()?;
    u64::try_from(at.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

/// A title as a person reads it: trimmed, and none where empty.
fn clean(title: &str) -> Option<String> {
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_owned())
}

/// Where a read of one transcript stands: the file it was, by its device and inode, the
/// offset after the last whole record read, and whether the fork was said.
struct Cursor {
    device: u64,
    inode: u64,
    offset: u64,
    forked: bool,
}

impl Cursor {
    const VERSION: &str = "1";

    fn parse(cursor: &str) -> Result<Self, Error> {
        let invalid = || Error::invalid(format!("`{cursor}` is no cursor this driver gave"));
        let mut parts = cursor.split('.');
        if parts.next() != Some(Self::VERSION) {
            return Err(invalid());
        }
        let mut number = || -> Result<u64, Error> {
            parts
                .next()
                .and_then(|part| part.parse().ok())
                .ok_or_else(invalid)
        };
        let (device, inode, offset, forked) = (number()?, number()?, number()?, number()?);
        if parts.next().is_some() || forked > 1 {
            return Err(invalid());
        }
        Ok(Self {
            device,
            inode,
            offset,
            forked: forked == 1,
        })
    }

    fn said(&self) -> String {
        format!(
            "{}.{}.{}.{}.{}",
            Self::VERSION,
            self.device,
            self.inode,
            self.offset,
            u8::from(self.forked)
        )
    }
}

/// The events of a transcript after `cursor`, or within `tail` bytes of its end. omp only
/// appends below the title it rewrites in place, so the same file no shorter than the
/// cursor goes on from it; another file there, or a shorter one, was replaced or rewritten.
fn read_from(reading: &Reading, cursor: Option<&str>, tail: Option<u64>) -> Result<Events, Error> {
    let unreadable = |e: std::io::Error| {
        Error::refused(refusal::REFUSED, format!("{}: {e}", reading.file.display()))
    };
    let mut open = File::open(&reading.file).map_err(unreadable)?;
    let meta = open.metadata().map_err(unreadable)?;
    let length = meta.len();
    let given = cursor.map(Cursor::parse).transpose()?;
    let same = given.as_ref().filter(|cursor| {
        cursor.device == meta.dev() && cursor.inode == meta.ino() && cursor.offset <= length
    });
    let reset = given.is_some() && same.is_none();
    let (mut start, forked) = match (same, tail) {
        (Some(cursor), _) => (cursor.offset, Some(cursor.forked)),
        (None, Some(tail)) if !reset => (length.saturating_sub(tail), None),
        (None, _) => (0, Some(false)),
    };
    // A tail starts at the first whole record within it.
    if forked.is_none() && start > 0 {
        open.seek(SeekFrom::Start(start - 1)).map_err(unreadable)?;
        let mut reader = BufReader::new((&open).take(length - (start - 1)));
        let mut skipped = Vec::new();
        start = match capped(&mut reader, &mut skipped, 0).map_err(unreadable)? {
            Line::Partial => length,
            Line::Whole | Line::Long => start - 1 + skipped_len(&reader, length - (start - 1)),
        };
    }
    open.seek(SeekFrom::Start(start)).map_err(unreadable)?;
    let (records, cut) = records(&open, start, length).map_err(unreadable)?;
    // Where a tail begins inside a fork's copy of its parent, the fork is still to come.
    let forked = forked.unwrap_or_else(|| {
        reading.fork.as_ref().is_none_or(|(_, began)| {
            records
                .iter()
                .filter_map(|record| record.value.as_ref())
                .filter(|record| !matches!(text(record, "type"), Some("title" | "session")))
                .find_map(|record| text(record, "timestamp").and_then(millis))
                .is_none_or(|at| at >= *began)
        })
    });
    let still = meta
        .modified()
        .ok()
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .is_some_and(|still| still >= events::SETTLE);
    let mut read = events::read(reading, &records, forked, !cut && still);
    // A batch that held back everything it read would never move on: what it held stands.
    if cut && read.through == start {
        read = events::read(reading, &records, forked, true);
    }
    Ok(Events {
        cursor: Cursor {
            device: meta.dev(),
            inode: meta.ino(),
            offset: read.through,
            forked: read.forked,
        }
        .said(),
        events: read.events,
        more: cut,
        reset,
        whole: start == 0,
    })
}

/// How many bytes a reader over `total` bytes has consumed.
fn skipped_len<R: std::io::Read>(reader: &BufReader<std::io::Take<R>>, total: u64) -> u64 {
    total - reader.get_ref().limit() - reader.buffer().len() as u64
}

/// The whole records from `start` up to `length`, at most [`BATCH`] bytes of them, and
/// whether whole records remain past them.
fn records(open: &File, start: u64, length: u64) -> std::io::Result<(Vec<Record>, bool)> {
    let total = length - start;
    let mut reader = BufReader::with_capacity(1 << 16, open.take(total));
    let mut records = Vec::new();
    let mut line = Vec::new();
    let mut at = start;
    loop {
        if at - start >= BATCH {
            return Ok((records, at < length));
        }
        let kind = capped(&mut reader, &mut line, RECORD)?;
        let end = start + skipped_len(&reader, total);
        match kind {
            // A record still being written is read whole next time.
            Line::Partial => return Ok((records, false)),
            Line::Long => records.push(Record {
                start: at,
                end,
                value: None,
            }),
            Line::Whole => records.push(Record {
                start: at,
                end,
                value: serde_json::from_slice(&line).ok(),
            }),
        }
        at = end;
    }
}

/// How a line read.
enum Line {
    /// Ended by its newline, and kept.
    Whole,
    /// Ended by its newline, and longer than kept.
    Long,
    /// No newline before the end: nothing whole.
    Partial,
}

/// The next line into `line`, without its newline, kept only up to `most` bytes.
fn capped<R: BufRead>(reader: &mut R, line: &mut Vec<u8>, most: usize) -> std::io::Result<Line> {
    line.clear();
    let mut long = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(Line::Partial);
        }
        let (taken, ended) = match buffer.iter().position(|&byte| byte == b'\n') {
            Some(at) => (at, true),
            None => (buffer.len(), false),
        };
        if !long && line.len() + taken <= most {
            line.extend_from_slice(&buffer[..taken]);
        } else {
            long = true;
            line.clear();
        }
        reader.consume(taken + usize::from(ended));
        if ended {
            return Ok(if long { Line::Long } else { Line::Whole });
        }
    }
}
