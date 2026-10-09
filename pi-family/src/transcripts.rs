//! A pi-family harness's conversations, from their transcripts:
//! `<root>/sessions/<folder>/<started-at>_<id>.jsonl`, and any folder the harness was told
//! to keep them in instead (`--session-dir`, `PI_CODING_AGENT_SESSION_DIR`, pi's
//! `sessionDir` setting), which the harness's own moments name.
//!
//! A transcript opens, in omp, with a fixed-width `title` record rewritten in place, then
//! the `session` header naming the conversation, its folder and, for a fork, the
//! conversation it came from; then entries, one JSON record per line, appended, each
//! naming its parent ([`crate::tree`]). An omp sub-agent's transcript is filed inside its
//! lead's folder, `<conversation>/<job>.jsonl`, and one a sub-agent started inside that
//! one's: those are the conversation's sub-agents, never conversations of their own.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use crate::Harness;
use crate::events::{self, Reading, Record, millis, text};
use crate::tree::{self, Trees};

/// How deep sub-agents are filed below their conversation's folder: a sub-agent's, the
/// one it started, and the one that one started (omp 18.4).
const SUB_AGENT_DEPTH: usize = 3;
/// The longest record read; a longer one is stepped over.
const RECORD: u64 = 8 << 20;
/// How many bytes of records one `sessions/read` answers at most; `more` says the rest.
const BATCH: u64 = 4 << 20;
/// The records the head of a transcript is read from: its `title` and `session`.
const HEAD: usize = 4;
/// The longest head record read.
const HEAD_RECORD: usize = 64 << 10;

/// The harness's conversations here: its agent folder, and the folders outside it its
/// moments named, remembered for as long as this driver runs; and the trees of the
/// transcripts read.
pub struct Store {
    harness: Harness,
    home: PathBuf,
    root: PathBuf,
    named: Mutex<BTreeSet<PathBuf>>,
    trees: Trees,
    titles: Titles,
}

/// Every transcript filed as a conversation, by the id it is filed under.
struct Filed {
    by_id: BTreeMap<String, Vec<PathBuf>>,
    /// Every folder was read whole.
    complete: bool,
}

impl Store {
    pub fn new(harness: Harness, home: &Path, root: PathBuf) -> Self {
        Self {
            harness,
            home: home.to_owned(),
            root,
            named: Mutex::new(BTreeSet::new()),
            trees: Trees::default(),
            titles: Titles::default(),
        }
    }

    pub fn harness(&self) -> Harness {
        self.harness
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
            let Some(session) = self.session(&id, file) else {
                continue;
            };
            if list
                .seen_since
                .is_none_or(|since| session.updated_at >= since)
            {
                sessions.push(session);
            }
        }
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        Listed {
            sessions,
            excluded: Vec::new(),
            complete,
        }
    }

    /// `sessions/locate`.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let file = self.find(&locate.external_id)?;
        let session = self.session(&locate.external_id, &file).ok_or_else(|| {
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
                            format!(
                                "{} has no sub-agent {agent} in {}",
                                self.harness.id(),
                                read.external_id
                            ),
                        )
                    })?;
                (found.transcripts.into_iter().next().unwrap_or(file), None)
            }
        };
        let reading = Reading {
            harness: self.harness,
            blobs: events::blobs(&file, &self.root),
            cwd: head.cwd,
            servers: crate::calls::servers(&self.root),
            fork,
            file,
        };
        read_from(&self.trees, &reading, read.cursor.as_deref(), read.tail)
    }

    /// The one transcript of conversation `id`.
    pub fn find(&self, id: &str) -> Result<PathBuf, Error> {
        let mut files = self.transcripts(id);
        match files.len() {
            0 => Err(Error::refused(
                refusal::NOT_FOUND,
                format!("{} has no conversation {id} here", self.harness.id()),
            )),
            1 => Ok(files.remove(0)),
            _ => Err(Error::refused(
                refusal::REFUSED,
                format!(
                    "more than one {} transcript answers to {id}",
                    self.harness.id()
                ),
            )),
        }
    }

    /// Every transcript filed under `id`: one, where the harness's store is as it keeps it.
    pub fn transcripts(&self, id: &str) -> Vec<PathBuf> {
        if id.is_empty() || id.chars().any(char::is_control) {
            return Vec::new();
        }
        self.filed().by_id.remove(id).unwrap_or_default()
    }

    /// Keep the folders of transcripts the harness's moments named.
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
    /// `PI_CODING_AGENT_SESSION_DIR` names, in pi the one its `sessionDir` setting names,
    /// and those the harness's moments named, each once, none a sub-agent's.
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
            .into_iter()
            .chain(self.setting())
            .filter(|dir| dir.is_absolute())
            .filter_map(|dir| dir.canonicalize().ok());
        let named = self.named.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for folder in chosen.chain(named) {
            let filed_by_harness = own.is_some() && folder.parent() == own.as_deref();
            if filed_by_harness || companion(&folder).is_file() || !seen.insert(folder.clone()) {
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

    /// pi's `sessionDir` in its `settings.json`, `~` read as the person's home.
    fn setting(&self) -> Option<PathBuf> {
        if self.harness != Harness::Pi {
            return None;
        }
        let bytes = std::fs::read(self.root.join("settings.json")).ok()?;
        let bytes = bytes.strip_prefix("\u{feff}".as_bytes()).unwrap_or(&bytes);
        let settings: Value = serde_json::from_slice(bytes).ok()?;
        let dir = text(&settings, "sessionDir")?.trim();
        Some(match dir.strip_prefix('~') {
            Some("") => self.home.clone(),
            Some(rest) => self.home.join(rest.strip_prefix('/')?),
            None => PathBuf::from(dir),
        })
    }

    /// A conversation as `sessions/list` says it.
    fn session(&self, id: &str, file: &Path) -> Option<Session> {
        let updated_at = modified(file)?;
        let head = head(file);
        let parent = parent(id, &head);
        let title = match self.harness {
            Harness::Omp => head.title,
            Harness::Pi => self.titles.title(file, updated_at),
        };
        Some(Session {
            external_id: id.to_owned(),
            parent,
            title,
            cwd: head.cwd,
            created_at: head.created_at,
            updated_at,
            transcripts: vec![file.to_owned()],
            app: false,
            sub_agents: sub_agents(file),
        })
    }
}

/// pi's titles, by transcript, as they stood when the file last changed. pi keeps no
/// title at the head of its file: the newest `session_info` name holds, and one with no
/// name clears it, after which pi shows its person's opening words again.
#[derive(Default)]
struct Titles {
    kept: Mutex<BTreeMap<PathBuf, (u64, Option<String>)>>,
}

impl Titles {
    fn title(&self, file: &Path, changed: u64) -> Option<String> {
        if let Some((at, title)) = self
            .kept
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(file)
            && *at == changed
        {
            return title.clone();
        }
        let title = pi_title(file);
        self.kept
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(file.to_owned(), (changed, title.clone()));
        title
    }
}

/// pi's title for a transcript: its newest `session_info` name, else what its person
/// asked for first.
fn pi_title(file: &Path) -> Option<String> {
    let mut reader = BufReader::new(File::open(file).ok()?);
    let mut line = Vec::with_capacity(4096);
    let (mut named, mut asked) = (None, None);
    loop {
        match capped(&mut reader, &mut line, HEAD_RECORD) {
            Ok(Line::Whole) => {}
            Ok(Line::Long) => continue,
            Ok(Line::Partial) | Err(_) => break,
        }
        if line.starts_with(br#"{"type":"session_info""#)
            && let Ok(record) = serde_json::from_slice::<Value>(&line)
        {
            named = text(&record, "name").and_then(clean);
        } else if asked.is_none()
            && line.starts_with(br#"{"type":"message""#)
            && let Ok(record) = serde_json::from_slice::<Value>(&line)
            && let Some(message) = record.get("message")
            && text(message, "role") == Some("user")
        {
            asked = asked_for(message.get("content"));
        }
    }
    named.or(asked)
}

/// What a person asked for, as a title: the first line of their words that is no markup,
/// at most 72 characters of whole words, after `## My request:` where a harness wrapped
/// it; none where the words open with a preamble a harness put there.
fn asked_for(content: Option<&Value>) -> Option<String> {
    const MARKER: &str = "## My request:";
    const PREAMBLE: [&str; 4] = [
        "# AGENTS.md",
        "<environment_context>",
        "# Files mentioned",
        "<user_instructions>",
    ];
    const MOST: usize = 72;
    let whole = match content? {
        Value::String(said) => said.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| text(part, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let asked = match whole.split_once(MARKER) {
        Some((_, asked)) => asked,
        None if PREAMBLE
            .iter()
            .any(|start| whole.trim_start().starts_with(start)) =>
        {
            return None;
        }
        None => &whole,
    };
    let first = asked
        .lines()
        .map(str::trim)
        .find(|line| {
            line.len() > 2
                && !line.starts_with('<')
                && !line.starts_with('#')
                && !line.starts_with("```")
        })?
        .trim_start_matches(['-', '*', '>', ' '])
        .trim();
    let mut line = String::new();
    for word in first.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > MOST {
            line.push('…');
            break;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    clean(&line)
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
pub fn id_of(file: &Path) -> Option<&str> {
    Some(file.file_stem()?.to_str()?.split_once('_')?.1)
        .filter(|id| !id.is_empty() && !id.chars().any(char::is_control))
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

/// A title as a person reads it: trimmed, and only where it is one short line. A harness
/// that wrote something else wrote something no person is shown, so it is dropped rather
/// than repaired.
fn clean(title: &str) -> Option<String> {
    let title = title.trim();
    (!title.is_empty() && !title.chars().any(char::is_control) && title.chars().count() <= 120)
        .then(|| title.to_owned())
}

/// Where a read of one transcript stands: the file it was, by its device and inode; the
/// last entry of its path whose events were said (`tip`, counted from 1, 0 for none) and
/// where that entry's record starts; how far into the file the entries off the path were
/// counted (`aside`); and whether the fork was said.
struct Cursor {
    device: u64,
    inode: u64,
    tip: u32,
    at: u64,
    aside: u64,
    forked: bool,
}

impl Cursor {
    const VERSION: &str = "3";

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
        let (device, inode, tip, at, aside, forked) = (
            number()?,
            number()?,
            number()?,
            number()?,
            number()?,
            number()?,
        );
        if parts.next().is_some() || forked > 1 {
            return Err(invalid());
        }
        Ok(Self {
            device,
            inode,
            tip: u32::try_from(tip).map_err(|_| invalid())?,
            at,
            aside,
            forked: forked == 1,
        })
    }

    fn said(&self) -> String {
        format!(
            "{}.{}.{}.{}.{}.{}.{}",
            Self::VERSION,
            self.device,
            self.inode,
            self.tip,
            self.at,
            self.aside,
            u8::from(self.forked)
        )
    }
}

/// One record of the file, `None` where it is longer than read or no JSON.
fn record(walk: &mut tree::Walk<'_>, node: tree::Node, file: &Path) -> Result<Record, String> {
    let value = if node.end - node.start > RECORD {
        None
    } else {
        let bytes = tree::bytes(walk.open, node.start, node.end)
            .map_err(|e| format!("{}: {e}", file.display()))?;
        serde_json::from_slice(&bytes).ok()
    };
    Ok(Record {
        start: node.start,
        value,
    })
}

/// The events of a transcript's active path after `cursor`, or of its entries within
/// `tail` bytes of its end. A cursor goes on from its entry where that entry is still on
/// the path, in the same file; where the conversation went back to an earlier entry and
/// on from there (a branch), or the file was replaced or written whole again, the events
/// start again from the beginning (`reset`). What a branch left behind spent is still
/// spent: once the path is read to its end, every entry off it written since the last
/// read is given as its `tokens` alone, each response once.
fn read_from(
    trees: &Trees,
    reading: &Reading,
    cursor: Option<&str>,
    tail: Option<u64>,
) -> Result<Events, Error> {
    let given = cursor.map(Cursor::parse).transpose()?;
    let still = std::fs::metadata(&reading.file)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .is_some_and(|still| still >= events::SETTLE);
    trees
        .walk(&reading.file, |mut walk| {
            let path = walk.tree.path()?;
            let length = walk.open.metadata().map_err(|e| e.to_string())?.len();
            // Where on the path the cursor's entry is: the entries before `from` are read.
            let on = given.as_ref().and_then(|cursor| {
                ((cursor.device, cursor.inode) == walk.file).then_some(())?;
                if cursor.tip == 0 {
                    return Some(0);
                }
                let tip = cursor.tip - 1;
                let at = path.iter().position(|node| *node == tip)?;
                (walk.tree.node(tip).start == cursor.at).then_some(at + 1)
            });
            let reset = given.is_some() && on.is_none();
            // From where in the file the entries off the path are still to be counted.
            let mut aside = match (on, tail) {
                (Some(_), _) => given.as_ref().map_or(0, |cursor| cursor.aside),
                (None, Some(tail)) if !reset => length.saturating_sub(tail),
                (None, _) => 0,
            };
            let (from, forked) = match (on, tail) {
                (Some(from), _) => (from, given.as_ref().map(|cursor| cursor.forked)),
                (None, Some(tail)) if !reset => {
                    let edge = length.saturating_sub(tail);
                    let from = path
                        .iter()
                        .position(|node| walk.tree.node(*node).start >= edge)
                        .unwrap_or(path.len());
                    (from, (from == 0).then_some(false))
                }
                (None, _) => (0, Some(false)),
            };
            let mut records = Vec::new();
            let mut taken = 0;
            for node in &path[from..] {
                if taken >= BATCH {
                    break;
                }
                let node = walk.tree.node(*node);
                taken += node.end - node.start;
                records.push(record(&mut walk, node, &reading.file)?);
            }
            let more = from + records.len() < path.len();
            // Where a tail begins inside a fork's copy of its parent, the fork is still to
            // come.
            let forked = forked.unwrap_or_else(|| {
                reading.fork.as_ref().is_none_or(|(_, began)| {
                    records
                        .iter()
                        .filter_map(|record| record.value.as_ref())
                        .find_map(|record| text(record, "timestamp").and_then(millis))
                        .is_none_or(|at| at >= *began)
                })
            });
            let mut read = events::read(reading, &records, forked, !more && still);
            // A batch that held back everything it read would never move on: what it held
            // stands.
            if more && read.consumed == 0 {
                read = events::read(reading, &records, forked, true);
            }
            if !more {
                let mut on_path = vec![false; walk.tree.entries()];
                for node in &path {
                    on_path[*node as usize] = true;
                }
                let mut through = aside;
                for (at, on) in on_path.into_iter().enumerate() {
                    let node = walk.tree.node(at as u32);
                    through = through.max(node.end + 1);
                    if on || node.start < aside {
                        continue;
                    }
                    if let Some(value) = record(&mut walk, node, &reading.file)?.value {
                        read.events.extend(events::spent(reading, &value));
                    }
                }
                aside = through;
            }
            let tip = (from + read.consumed).checked_sub(1).map(|at| path[at]);
            Ok(Events {
                cursor: Cursor {
                    device: walk.file.0,
                    inode: walk.file.1,
                    tip: tip.map_or(0, |tip| tip + 1),
                    at: tip.map_or(0, |tip| walk.tree.node(tip).start),
                    aside,
                    forked: read.forked,
                }
                .said(),
                events: read.events,
                more,
                reset,
                whole: from == 0,
            })
        })
        .map_err(|why| Error::refused(refusal::REFUSED, why))
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

#[cfg(test)]
mod tests {
    use super::*;
    use steery_sdk::wire::harness::{Event, Voice};

    fn entry(id: &str, parent: Option<&str>, role: &str, said: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": "message", "id": id, "parentId": parent,
                "timestamp": "2026-10-01T10:00:00Z",
                "message": {"role": role, "content": [{"type": "text", "text": said}],
                            "stopReason": "stop"}})
        )
    }

    fn words(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Words {
                    voice: Voice::Person | Voice::Agent,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// A conversation is read along its active path: a read on from a cursor gives what
    /// followed it, and once the conversation went back to an earlier entry and on from
    /// there, the events start again from the beginning along the new path, and the
    /// abandoned branch is never read.
    #[test]
    fn a_conversation_is_read_along_its_active_path() {
        let home = std::env::temp_dir().join(format!("steery-pi-store-{}", std::process::id()));
        let folder = home.join(".pi/agent/sessions/--tmp--");
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("2026-10-01T10-00-00-000Z_c1.jsonl");
        std::fs::write(
            &file,
            format!(
                "{}{}{}",
                "{\"type\":\"session\",\"version\":3,\"id\":\"c1\",\"cwd\":\"/tmp\",\"timestamp\":\"2026-10-01T10:00:00Z\"}\n",
                entry("a", None, "user", "first"),
                entry("b", Some("a"), "assistant", "answer one"),
            ),
        )
        .unwrap();
        let store = Store::new(Harness::Pi, &home, home.join(".pi/agent"));
        let read = |cursor: Option<String>| {
            store
                .read(Read {
                    external_id: "c1".into(),
                    sub_agent: None,
                    cursor,
                    tail: None,
                })
                .unwrap()
        };
        let first = read(None);
        assert_eq!(words(&first.events), ["first", "answer one"]);
        assert!(first.whole && !first.reset && !first.more);

        let mut appended = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        std::io::Write::write_all(
            &mut appended,
            entry("c", Some("b"), "user", "second").as_bytes(),
        )
        .unwrap();
        let on = read(Some(first.cursor.clone()));
        assert_eq!(words(&on.events), ["second"]);
        assert!(!on.reset && !on.whole);

        // Back to `b`'s parent and on from there: a branch.
        std::io::Write::write_all(
            &mut appended,
            entry("d", Some("a"), "assistant", "answer two").as_bytes(),
        )
        .unwrap();
        let branched = read(Some(on.cursor.clone()));
        assert!(branched.reset && branched.whole);
        assert_eq!(words(&branched.events), ["first", "answer two"]);
        std::fs::remove_dir_all(&home).unwrap();
    }

    /// What a branch left behind spent is still given, once per response: the first read
    /// gives every answer's tokens, the abandoned one's included; a read on from its
    /// cursor gives only what was written since, on the path or off it.
    #[test]
    fn a_branch_left_behind_is_counted_once_per_response() {
        let home = std::env::temp_dir().join(format!("steery-pi-spent-{}", std::process::id()));
        let folder = home.join(".pi/agent/sessions/--tmp--");
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("2026-10-01T10-00-00-000Z_c2.jsonl");
        let answer = |id: &str, parent: &str, output: u64| {
            format!(
                "{}\n",
                serde_json::json!({"type": "message", "id": id, "parentId": parent,
                    "timestamp": "2026-10-01T10:00:00Z",
                    "message": {"role": "assistant", "stopReason": "stop",
                        "content": [{"type": "text", "text": id}],
                        "usage": {"input": 10, "output": output}}})
            )
        };
        std::fs::write(
            &file,
            format!(
                "{}{}{}{}",
                "{\"type\":\"session\",\"version\":3,\"id\":\"c2\",\"timestamp\":\"2026-10-01T10:00:00Z\"}\n",
                entry("a", None, "user", "ask"),
                answer("b", "a", 5),
                answer("c", "a", 7),
            ),
        )
        .unwrap();
        let store = Store::new(Harness::Pi, &home, home.join(".pi/agent"));
        let read = |cursor: Option<String>| {
            store
                .read(Read {
                    external_id: "c2".into(),
                    sub_agent: None,
                    cursor,
                    tail: None,
                })
                .unwrap()
        };
        let spent = |events: &[Event]| {
            let mut spent: Vec<(String, u64)> = events
                .iter()
                .filter_map(|event| match event {
                    Event::Tokens {
                        response: Some(response),
                        output,
                        ..
                    } => Some((response.clone(), *output)),
                    _ => None,
                })
                .collect();
            spent.sort();
            spent
        };
        let first = read(None);
        assert_eq!(words(&first.events), ["ask", "c"], "the path alone is read");
        assert_eq!(
            spent(&first.events),
            [("b".to_owned(), 5), ("c".to_owned(), 7)],
            "the branch left behind spent too, each response once"
        );
        let again = read(Some(first.cursor.clone()));
        assert!(spent(&again.events).is_empty(), "nothing is counted twice");
        let mut appended = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        std::io::Write::write_all(&mut appended, answer("d", "a", 9).as_bytes()).unwrap();
        let on = read(Some(again.cursor.clone()));
        assert!(on.reset, "the leaf moved to another branch");
        let counted: Vec<String> = spent(&on.events).into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            counted,
            ["b", "c", "d"],
            "each response, said again, still once each"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }
}
