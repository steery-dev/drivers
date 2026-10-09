//! Claude Code's conversations, from their transcripts: `<root>/projects/<folder>/<id>.jsonl`,
//! the folder named for the directory the conversation runs in, and the `projects` folder
//! of any other configuration root Claude Code's own moments named a transcript in.
//!
//! A transcript is one JSON record per line, appended; a conversation resumed goes on in
//! the same file, under the same id. A sub-agent's transcript is filed under its
//! conversation's, `<folder>/<id>/subagents/agent-<agent>.jsonl`, with the
//! `agent-<agent>.meta.json` Claude Code writes beside it naming its type, the description
//! its lead gave it and the call that started it: those are the conversation's sub-agents,
//! never conversations of their own. Nor is a run a program started (`claude -p`, any
//! agent-SDK caller), which every record of it says as `entrypoint: sdk-cli` until a
//! person carries it on, whose records say `cli` from then on.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader, Read as _, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::UNIX_EPOCH;

use serde_json::Value;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use crate::events::{self, Reading, Record, Turn, agent_file, agent_id, millis, one_line, text};
use crate::tree::{self, Node, Trees};

/// The longest record read; a longer one is stepped over.
const RECORD: usize = 8 << 20;
/// How many bytes of records one `sessions/read` answers at most; `more` says the rest.
const BATCH: u64 = 4 << 20;
/// The longest record read for what a conversation's head says. A line is a whole message
/// with its metadata, and the directory sits past what a person typed.
const HEAD_RECORD: usize = 64 << 10;
/// How many opening records say how a conversation began: when, and from where
/// (`entrypoint`). The first few are queue bookkeeping and hook attachments; the field
/// appears by the third record of a headless run and the sixth of a session somebody
/// opened.
const OPENING: usize = 16;
/// How many opening records of a sub-agent's transcript are read for when it began and
/// what it was asked.
const AGENT_OPENING: usize = 48;
/// How much of a transcript's end is read for its newest `entrypoint`: every user and
/// assistant record carries one, and a few of them fit in this.
const ENTRYPOINT_TAIL: u64 = 256 << 10;
/// What Claude Code calls itself where a program started it rather than a person.
const HEADLESS: &str = "sdk-cli";
/// How a record naming the conversation opens: Claude Code appends one whenever its person
/// names it, and the last one stands.
const CUSTOM_TITLE: &[u8] = br#"{"type":"custom-title""#;

/// Claude Code's conversations here: its configuration root, the other `projects` folders
/// its moments named, what each transcript's head says, kept while the file only grows,
/// and each transcript's tree as far as it was read.
pub struct Store {
    root: PathBuf,
    named: Mutex<BTreeSet<PathBuf>>,
    heads: Mutex<HashMap<PathBuf, Kept<Head>>>,
    agents: Mutex<HashMap<PathBuf, Kept<Agent>>>,
    trees: Trees,
}

/// What was read of one file, and the file as it was then.
struct Kept<T> {
    stamp: Stamp,
    value: T,
}

/// A file as it stands: its device and inode, its length, and when it last changed, in
/// nanoseconds.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    length: u64,
    modified: i128,
}

impl Stamp {
    fn of(meta: &Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            length: meta.len(),
            modified: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        }
    }
}

/// Every transcript filed as a conversation, by the id it is filed under.
struct Filed {
    by_id: BTreeMap<String, Vec<PathBuf>>,
    /// Every folder was read whole.
    complete: bool,
}

/// What one transcript says of its conversation.
#[derive(Clone)]
struct Facts {
    head: Head,
    /// A program started it, and no person carried it on.
    headless: bool,
    /// When it last changed, in milliseconds since 1970.
    updated_at: u64,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            named: Mutex::new(BTreeSet::new()),
            heads: Mutex::new(HashMap::new()),
            agents: Mutex::new(HashMap::new()),
            trees: Trees::default(),
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
        let mut excluded = Vec::new();
        let walked: HashSet<PathBuf> = filed.by_id.values().flatten().cloned().collect();
        for (id, files) in filed.by_id {
            let mut read = Vec::new();
            for file in files {
                match self.facts(&file) {
                    Some(facts) => read.push((file, facts)),
                    None => complete = false,
                }
            }
            if read.is_empty()
                || list
                    .seen_since
                    .is_some_and(|since| read.iter().all(|(_, facts)| facts.updated_at < since))
            {
                continue;
            }
            read.retain(|(_, facts)| !facts.headless);
            match read.as_slice() {
                [] => excluded.push(id),
                [(file, facts)] => sessions.push(self.described(&id, file, facts)),
                // Two transcripts answering to one id name it ambiguously: neither is read.
                _ => complete = false,
            }
        }
        lock(&self.heads).retain(|file, _| walked.contains(file));
        lock(&self.agents).retain(|file, _| regular(file));
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        Listed {
            sessions,
            excluded,
            complete,
        }
    }

    /// `sessions/locate`.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        Ok(Located {
            session: self.session(&locate.external_id)?,
        })
    }

    /// Conversation `id`, as `sessions/locate` answers it.
    pub fn session(&self, id: &str) -> Result<Session, Error> {
        let (file, facts) = self.find(id)?;
        Ok(self.described(id, &file, &facts))
    }

    /// `sessions/read`: the events of a conversation, or of one of its sub-agents.
    pub fn read(&self, read: Read) -> Result<Events, Error> {
        let (file, _) = self.find(&read.external_id)?;
        let agents = file.with_extension("").join("subagents");
        let (file, own) = match &read.sub_agent {
            None => (file, false),
            Some(agent) => {
                let path = agent_file(&agents, agent);
                // A sub-agent's id becomes one component of a path: one, or nothing.
                if !agent_id(agent) || !regular(&path) {
                    return Err(Error::refused(
                        refusal::NOT_FOUND,
                        format!(
                            "Claude Code has no sub-agent {agent} in {}",
                            read.external_id
                        ),
                    ));
                }
                (path, true)
            }
        };
        read_from(
            &self.trees,
            &file,
            &Reading { own, agents },
            read.cursor.as_deref(),
            read.tail,
        )
    }

    /// The one transcript conversation `id` is: of those filed under it, the one a person
    /// is in.
    fn find(&self, id: &str) -> Result<(PathBuf, Facts), Error> {
        let files = self.filed().by_id.remove(id).unwrap_or_default();
        if files.is_empty() {
            return Err(Error::refused(
                refusal::NOT_FOUND,
                format!("Claude Code has no conversation {id} here"),
            ));
        }
        let mut people: Vec<(PathBuf, Facts)> = files
            .into_iter()
            .filter_map(|file| self.facts(&file).map(|facts| (file, facts)))
            .filter(|(_, facts)| !facts.headless)
            .collect();
        match people.len() {
            0 => Err(Error::refused(
                refusal::NOT_FOUND,
                format!("Claude Code's {id} is a run no person is in"),
            )),
            1 => Ok(people.remove(0)),
            _ => Err(Error::refused(
                refusal::REFUSED,
                format!("more than one Claude Code transcript answers to {id}"),
            )),
        }
    }

    /// A conversation as `sessions/list` says it.
    fn described(&self, id: &str, file: &Path, facts: &Facts) -> Session {
        let head = &facts.head;
        Session {
            external_id: id.to_owned(),
            cwd: head.cwd.clone(),
            title: head.title.clone().or_else(|| head.asked.clone()),
            created_at: head.created_at,
            updated_at: facts.updated_at,
            transcripts: vec![file.to_owned()],
            // A fork opens with copies of its parent's records and names nothing.
            parent: None,
            app: head
                .entrypoint
                .as_deref()
                .is_some_and(|entrypoint| entrypoint.starts_with("claude-desktop")),
            sub_agents: self.sub_agents(file),
        }
    }

    /// What `file` says of its conversation, its head read on from where it was last read
    /// while the file only grew.
    fn facts(&self, file: &Path) -> Option<Facts> {
        let mut open = File::open(file).ok()?;
        let meta = open.metadata().ok()?;
        let stamp = Stamp::of(&meta);
        let updated_at = u64::try_from(
            meta.modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_millis(),
        )
        .ok()?;
        let mut heads = lock(&self.heads);
        let kept = heads.remove(file).filter(|kept| {
            (kept.stamp.device, kept.stamp.inode) == (stamp.device, stamp.inode)
                && kept.value.through <= stamp.length
        });
        let changed = kept.as_ref().is_none_or(|kept| kept.stamp != stamp);
        let mut head = kept.map(|kept| kept.value).unwrap_or_default();
        if changed {
            head.read(&mut open, stamp.length).ok()?;
            head.headless = headless(&mut open, stamp.length, &head).ok()?;
        }
        let facts = Facts {
            headless: head.headless,
            head: head.clone(),
            updated_at,
        };
        heads.insert(file.to_owned(), Kept { stamp, value: head });
        Some(facts)
    }

    /// The sub-agents filed under a conversation, oldest first.
    fn sub_agents(&self, file: &Path) -> Vec<SubAgent> {
        let dir = file.with_extension("").join("subagents");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        let mut kept = lock(&self.agents);
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(agent) = name
                .to_str()
                .and_then(|name| name.strip_prefix("agent-")?.strip_suffix(".jsonl"))
                .filter(|agent| agent_id(agent))
            else {
                continue;
            };
            let path = entry.path();
            if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let Some(read) = Agent::kept(
                &mut kept,
                &path,
                &dir.join(format!("agent-{agent}.meta.json")),
            ) else {
                continue;
            };
            found.push(SubAgent {
                external_id: agent.to_owned(),
                lead: None,
                name: read.name,
                task: read.task,
                started_at: read.started_at,
                started_by: read.started_by,
                transcripts: vec![path],
            });
        }
        found.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.external_id.cmp(&b.external_id))
        });
        found
    }

    /// Keep the `projects` folders of transcripts Claude Code's moments named.
    fn remember(&self, named: &[PathBuf]) {
        let folders = named
            .iter()
            .filter(|file| file.is_absolute() && file.extension().is_some_and(|ext| ext == "jsonl"))
            .filter_map(|file| file.parent())
            .filter(|folder| folder.file_name().is_some_and(|name| name != "subagents"))
            .filter_map(Path::parent)
            .filter(|projects| projects.file_name().is_some_and(|name| name == "projects"))
            .filter_map(|projects| projects.canonicalize().ok());
        lock(&self.named).extend(folders);
    }

    /// The folders conversations are filed in: each of `<root>/projects/*`, then each of
    /// those of the `projects` folders Claude Code's moments named, each once. A folder
    /// that is a link is never read: it could name one anywhere.
    fn folders(&self) -> (Vec<PathBuf>, bool) {
        let own = self.root.join("projects");
        let mut seen: BTreeSet<PathBuf> = own.canonicalize().into_iter().collect();
        let named: Vec<PathBuf> = lock(&self.named)
            .iter()
            .filter(|projects| seen.insert((*projects).clone()))
            .cloned()
            .collect();
        let mut complete = true;
        let mut folders = Vec::new();
        for projects in std::iter::once(own).chain(named) {
            match std::fs::read_dir(&projects) {
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
                if let Some(id) = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .filter(|id| !id.is_empty() && !id.chars().any(char::is_control))
                {
                    by_id.entry(id.to_owned()).or_default().push(path);
                }
            }
        }
        Filed { by_id, complete }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether `path` is a regular file, not a link to one.
fn regular(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// What the head of a conversation's transcript says, and how far it has been read.
#[derive(Clone, Default)]
struct Head {
    /// The offset after the last whole record read.
    through: u64,
    /// How many records were read.
    records: usize,
    /// The directory of the first record that names one.
    cwd: Option<PathBuf>,
    /// The first stamp among the opening records.
    created_at: Option<u64>,
    /// The first `entrypoint` among the opening records.
    entrypoint: Option<String>,
    /// The name its person last gave it (`custom-title`).
    title: Option<String>,
    /// What its person asked first, as one short line: its name where they gave none.
    asked: Option<String>,
    headless: bool,
}

impl Head {
    /// Read on from `through` to the last whole record before `length`. A `custom-title`
    /// is looked for in every record; every other record is parsed only until the head
    /// knows how the conversation began and what was asked first.
    fn read(&mut self, open: &mut File, length: u64) -> std::io::Result<()> {
        open.seek(SeekFrom::Start(self.through))?;
        let total = length - self.through;
        let mut reader = BufReader::with_capacity(1 << 16, (&*open).take(total));
        let mut line = Vec::new();
        let start = self.through;
        loop {
            let kind = capped(&mut reader, &mut line, HEAD_RECORD)?;
            if matches!(kind, Line::Partial) {
                return Ok(());
            }
            self.through = start + consumed(&reader, total);
            self.records += 1;
            if !matches!(kind, Line::Whole) {
                continue;
            }
            if line.starts_with(CUSTOM_TITLE) {
                if let Some(named) = serde_json::from_slice::<Value>(&line)
                    .ok()
                    .and_then(|record| text(&record, "customTitle").and_then(clean))
                {
                    self.title = Some(named);
                }
                continue;
            }
            let opening = self.records <= OPENING;
            if !opening && self.cwd.is_some() && self.asked.is_some() {
                continue;
            }
            let Ok(record) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if opening {
                if self.created_at.is_none() {
                    self.created_at = text(&record, "timestamp").and_then(millis);
                }
                if self.entrypoint.is_none() {
                    self.entrypoint = text(&record, "entrypoint").map(str::to_owned);
                }
            }
            if self.cwd.is_none() {
                self.cwd = text(&record, "cwd")
                    .filter(|cwd| cwd.starts_with('/') && !cwd.chars().any(char::is_control))
                    .map(PathBuf::from);
            }
            if self.asked.is_none()
                && record.get("isSidechain").and_then(Value::as_bool) != Some(true)
            {
                self.asked = events::instruction(&record).as_deref().and_then(asked);
            }
        }
    }
}

/// Whether a program started the conversation and no person carried it on: the newest
/// `entrypoint` near its end says `sdk-cli`, or, where none there says any, the first. A
/// transcript that names none is a person's: hiding their conversation is the worse
/// mistake.
fn headless(open: &mut File, length: u64, head: &Head) -> std::io::Result<bool> {
    let start = first_whole(open, length.saturating_sub(ENTRYPOINT_TAIL), length)?;
    open.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new((&*open).take(length - start));
    let mut line = Vec::new();
    let mut newest = None;
    loop {
        match capped(&mut reader, &mut line, RECORD)? {
            Line::Partial => break,
            Line::Long => {}
            Line::Whole => {
                if let Some(entrypoint) = serde_json::from_slice::<Value>(&line)
                    .ok()
                    .and_then(|record| text(&record, "entrypoint").map(str::to_owned))
                {
                    newest = Some(entrypoint);
                }
            }
        }
    }
    Ok(newest.as_deref().or(head.entrypoint.as_deref()) == Some(HEADLESS))
}

/// What a person asked for, as one short line: the first line of their words that is
/// not markup, cut at a word, short enough to read on a phone.
fn asked(said: &str) -> Option<String> {
    const MOST: usize = 72;
    let first = said
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

/// A title as a person reads it: trimmed, one short line, and none where Claude Code wrote
/// anything else.
fn clean(title: &str) -> Option<String> {
    let title = title.trim();
    (!title.is_empty() && title.chars().count() <= 120 && !title.chars().any(char::is_control))
        .then(|| title.to_owned())
}

/// What a sub-agent's transcript and the record beside it say of it.
#[derive(Clone)]
struct Agent {
    /// The record beside the transcript, as it stood when this was read.
    meta: Option<Stamp>,
    name: Option<String>,
    task: Option<String>,
    started_at: Option<u64>,
    started_by: Option<String>,
}

impl Agent {
    /// What `file` and its `meta` say, read again only once either changed.
    fn kept(kept: &mut HashMap<PathBuf, Kept<Agent>>, file: &Path, meta: &Path) -> Option<Agent> {
        let stamp = Stamp::of(&std::fs::metadata(file).ok()?);
        let beside = std::fs::symlink_metadata(meta)
            .ok()
            .filter(Metadata::is_file)
            .map(|meta| Stamp::of(&meta));
        if let Some(read) = kept
            .get(file)
            .filter(|read| read.stamp == stamp && read.value.meta == beside)
        {
            return Some(read.value.clone());
        }
        let meta: Value = beside
            .and_then(|_| std::fs::read(meta).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null);
        let opening = opening(file);
        let first_line = |said: &str| said.lines().map(one_line).find(|line| !line.is_empty());
        let agent = Agent {
            meta: beside,
            name: text(&meta, "agentType")
                .map(one_line)
                .filter(|name| !name.is_empty()),
            task: text(&meta, "description").and_then(first_line).or_else(|| {
                opening
                    .iter()
                    .find_map(events::instruction)
                    .as_deref()
                    .and_then(first_line)
            }),
            started_at: opening
                .iter()
                .find_map(|record| text(record, "timestamp").and_then(millis)),
            started_by: text(&meta, "toolUseId").map(str::to_owned),
        };
        kept.insert(
            file.to_owned(),
            Kept {
                stamp,
                value: agent.clone(),
            },
        );
        Some(agent)
    }
}

/// The first [`AGENT_OPENING`] records of a transcript, each read at most
/// [`HEAD_RECORD`] bytes.
fn opening(file: &Path) -> Vec<Value> {
    let Ok(open) = File::open(file) else {
        return Vec::new();
    };
    let mut reader = BufReader::new(open);
    let mut line = Vec::new();
    let mut out = Vec::new();
    for _ in 0..AGENT_OPENING {
        match capped(&mut reader, &mut line, HEAD_RECORD) {
            Ok(Line::Whole) => out.extend(serde_json::from_slice(&line).ok()),
            Ok(Line::Long) => {}
            Ok(Line::Partial) | Err(_) => break,
        }
    }
    out
}

/// Where a read of one transcript stands: the file it was, by its device and inode; the
/// last record of its path whose events were said (`tip`, counted from 1, 0 for none) and
/// where that record starts; how far into the file the records off the path were counted
/// (`aside`); and where the turn stood.
struct Cursor {
    device: u64,
    inode: u64,
    tip: u32,
    at: u64,
    aside: u64,
    turn: Turn,
}

impl Cursor {
    const VERSION: &str = "2";

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
        let (device, inode, tip, at, aside, turn) = (
            number()?,
            number()?,
            number()?,
            number()?,
            number()?,
            number()?,
        );
        let turn = match turn {
            0 => Turn::Closed,
            1 => Turn::Open,
            2 => Turn::Unknown,
            _ => return Err(invalid()),
        };
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(Self {
            device,
            inode,
            tip: u32::try_from(tip).map_err(|_| invalid())?,
            at,
            aside,
            turn,
        })
    }

    fn said(&self) -> String {
        let turn = match self.turn {
            Turn::Closed => 0,
            Turn::Open => 1,
            Turn::Unknown => 2,
        };
        format!(
            "{}.{}.{}.{}.{}.{}.{turn}",
            Self::VERSION,
            self.device,
            self.inode,
            self.tip,
            self.at,
            self.aside
        )
    }
}

/// One record of the file, `None` where it is longer than read or no JSON.
fn record(open: &mut File, node: Node, file: &Path) -> Result<Record, String> {
    let value = if node.end - node.start > RECORD as u64 {
        None
    } else {
        let bytes = tree::bytes(open, node.start, node.end)
            .map_err(|e| format!("{}: {e}", file.display()))?;
        serde_json::from_slice(&bytes).ok()
    };
    Ok(Record {
        start: node.start,
        value,
    })
}

/// The events of a transcript's path after `cursor`, or of its records within `tail`
/// bytes of its end. A cursor goes on from its record where that record is still on the
/// path, in the same file; where the conversation went back to an earlier record and on
/// from there (a rewind), or the file was replaced or written whole again, the events
/// start again from the beginning (`reset`). What a branch left behind spent is still
/// spent: once the path is read to its end, every record off it written since the last
/// read is given as its `tokens` alone.
fn read_from(
    trees: &Trees,
    file: &Path,
    reading: &Reading,
    cursor: Option<&str>,
    tail: Option<u64>,
) -> Result<Events, Error> {
    let given = cursor.map(Cursor::parse).transpose()?;
    trees
        .walk(file, reading.own, |walk| {
            let path = walk.tree.path()?;
            // Where on the path the cursor's record is: the records before `from` are read.
            let on = given.as_ref().and_then(|cursor| {
                ((cursor.device, cursor.inode) == walk.file).then_some(())?;
                if cursor.tip == 0 {
                    return Some((0, cursor));
                }
                let tip = cursor.tip - 1;
                let at = path.iter().position(|node| *node == tip)?;
                (walk.tree.node(tip).start == cursor.at).then_some((at + 1, cursor))
            });
            let reset = given.is_some() && on.is_none();
            let (from, turn, mut aside) = match (on, tail) {
                (Some((from, cursor)), _) => (from, cursor.turn, cursor.aside),
                (None, Some(tail)) if !reset => {
                    let edge = walk.len.saturating_sub(tail);
                    let from = path
                        .iter()
                        .position(|node| walk.tree.node(*node).start >= edge)
                        .unwrap_or(path.len());
                    let turn = if from == 0 {
                        Turn::Closed
                    } else {
                        Turn::Unknown
                    };
                    (from, turn, edge)
                }
                (None, _) => (0, Turn::Closed, 0),
            };
            let mut records = Vec::new();
            let mut taken = 0;
            for node in &path[from..] {
                if taken >= BATCH {
                    break;
                }
                let node = walk.tree.node(*node);
                taken += node.end - node.start;
                records.push(record(walk.open, node, file)?);
            }
            let more = from + records.len() < path.len();
            let mut read = events::read(reading, &records, turn);
            if !more {
                let mut on_path = vec![false; walk.tree.nodes().len()];
                for node in &path {
                    on_path[*node as usize] = true;
                }
                let mut through = aside;
                for (node, on) in walk.tree.nodes().iter().zip(on_path) {
                    through = through.max(node.end + 1);
                    if on || node.start < aside {
                        continue;
                    }
                    if let Some(value) = record(walk.open, *node, file)?.value {
                        read.events.extend(events::spent(&value));
                    }
                }
                aside = through;
            }
            let tip = (from + records.len()).checked_sub(1).map(|at| path[at]);
            Ok(Events {
                cursor: Cursor {
                    device: walk.file.0,
                    inode: walk.file.1,
                    tip: tip.map_or(0, |tip| tip + 1),
                    at: tip.map_or(0, |tip| walk.tree.node(tip).start),
                    aside,
                    turn: read.turn,
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

/// The offset of the first whole record at or after `start`: `start` itself where a
/// record begins there, else just past the next newline, else `length`.
fn first_whole(open: &mut File, start: u64, length: u64) -> std::io::Result<u64> {
    if start == 0 {
        return Ok(0);
    }
    open.seek(SeekFrom::Start(start - 1))?;
    let total = length - (start - 1);
    let mut reader = BufReader::new((&*open).take(total));
    let mut skipped = Vec::new();
    Ok(match capped(&mut reader, &mut skipped, 0)? {
        Line::Partial => length,
        Line::Whole | Line::Long => start - 1 + consumed(&reader, total),
    })
}

/// How many bytes a reader over `total` bytes has consumed.
fn consumed<R: std::io::Read>(reader: &BufReader<std::io::Take<R>>, total: u64) -> u64 {
    total - reader.get_ref().limit() - reader.buffer().len() as u64
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;
    use steery_sdk::wire::harness::{Event, Voice};

    use super::*;

    /// A configuration root of its own for one test.
    fn scratch() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "steery-cc-transcripts-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("projects")).unwrap();
        root
    }

    fn write(file: &Path, records: &[Value]) {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let text: String = records.iter().map(|record| format!("{record}\n")).collect();
        std::fs::write(file, text).unwrap();
    }

    fn listed(store: &Store) -> Listed {
        store.list(List::default())
    }

    fn refused(error: Error) -> String {
        match error {
            Error::Refused { word, .. } => word,
            other => panic!("{other:?}"),
        }
    }

    fn reading(store: &Store, id: &str, cursor: Option<&str>, tail: Option<u64>) -> Events {
        store
            .read(Read {
                external_id: id.to_owned(),
                sub_agent: None,
                cursor: cursor.map(str::to_owned),
                tail,
            })
            .unwrap()
    }

    /// A conversation nobody named is named by what its person asked first, never by the
    /// call's result in front of it; a name somebody gave it outranks that (the runtime's
    /// `a_claude_code_conversation_is_named_by_what_its_person_asked_first`).
    #[test]
    fn a_conversation_is_named_by_what_its_person_asked_first() {
        let root = scratch();
        let project = root.join("projects/-private-tmp");
        let result = json!({"type":"user","cwd":"/private/tmp","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"total 320"}]}});
        let turn = json!({"type":"user","cwd":"/private/tmp","message":{"role":"user","content":"count the tools whose name contains steery"}});
        // As Claude Code writes it, `type` first: `json!` would put `customTitle` first.
        let titled = r#"{"type":"custom-title","customTitle":"the name its person typed"}"#;
        write(
            &project.join("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.jsonl"),
            &[result, turn.clone()],
        );
        std::fs::write(
            project.join("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb.jsonl"),
            format!("{turn}\n{titled}\n"),
        )
        .unwrap();
        let store = Store::new(root.clone());
        let named = |id: &str| store.session(id).unwrap();
        let read = named("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        assert_eq!(
            read.title.as_deref(),
            Some("count the tools whose name contains steery"),
            "the first thing the person said, not the tool result in front of it"
        );
        assert_eq!(read.cwd.as_deref(), Some(Path::new("/private/tmp")));
        assert_eq!(
            named("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
                .title
                .as_deref(),
            Some("the name its person typed"),
            "a name somebody chose outranks one read off their opening sentence"
        );
        // A name given later is read on from where the head was read.
        let file = project.join("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.jsonl");
        let mut more = std::fs::read_to_string(&file).unwrap();
        more.push_str("{\"type\":\"custom-title\",\"customTitle\":\"renamed\"}\n");
        std::fs::write(&file, more).unwrap();
        assert_eq!(
            named("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
                .title
                .as_deref(),
            Some("renamed")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A run a program started is no conversation, and is said excluded so it is not read
    /// as gone; one somebody carried on is a conversation; a second transcript under its
    /// name a person may be in keeps it from being excluded (the runtime's
    /// `old_sdk_exclusions_yield_to_adopted_or_uncertain_transcripts`).
    #[test]
    fn a_headless_run_is_excluded_until_a_person_is_in_it() {
        let root = scratch();
        let id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
        let file = root.join(format!("projects/first/{id}.jsonl"));
        let sdk = json!({"type":"user","entrypoint":"sdk-cli","cwd":"/private/tmp","message":{"role":"user"}});
        write(&file, std::slice::from_ref(&sdk));
        let store = Store::new(root.clone());
        let old = listed(&store);
        assert_eq!(old.excluded, [id]);
        assert!(old.sessions.is_empty());
        assert_eq!(refused(store.session(id).unwrap_err()), refusal::NOT_FOUND);

        let uncertain = root.join(format!("projects/second/{id}.jsonl"));
        write(&uncertain, &[json!({"type":"user"})]);
        let both = listed(&store);
        assert!(
            both.excluded.is_empty(),
            "an uncertain transcript prevents exclusion"
        );
        assert_eq!(
            both.sessions[0].transcripts,
            std::slice::from_ref(&uncertain)
        );
        std::fs::remove_file(uncertain).unwrap();

        write(
            &file,
            &[
                sdk,
                json!({"type":"user","entrypoint":"cli","cwd":"/private/tmp","message":{"role":"user"}}),
            ],
        );
        let adopted = listed(&store);
        assert!(adopted.excluded.is_empty());
        assert_eq!(adopted.sessions[0].external_id, id);
        std::fs::remove_file(&file).unwrap();
        let gone = listed(&store);
        assert!(
            gone.excluded.is_empty() && gone.sessions.is_empty(),
            "a missing file is no exclusion"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Only a transcript whose newest records still say `sdk-cli` is a headless run, however
    /// far back a person carried it on (the runtime's
    /// `a_headless_run_somebody_carried_on_is_a_conversation`).
    #[test]
    fn a_headless_run_somebody_carried_on_is_a_conversation() {
        let root = scratch();
        let id = "924cbccb-f5da-4e35-9c2e-d6630e3ce407";
        let file = root.join(format!("projects/-private-tmp/{id}.jsonl"));
        let headless = json!({"type":"user","entrypoint":"sdk-cli","cwd":"/private/tmp","message":{"role":"user"}});
        let carried = json!({"type":"user","entrypoint":"cli","cwd":"/private/tmp","message":{"role":"user"}});
        let filler = json!({"type":"assistant","message":{"content":"x".repeat(2000)}});
        write(&file, &[headless.clone(), headless.clone()]);
        let store = Store::new(root.clone());
        assert!(
            store.facts(&file).unwrap().headless,
            "a run a program started"
        );
        let mut later = vec![headless.clone(), headless];
        later.extend(std::iter::repeat_n(filler.clone(), 200));
        later.extend([carried, filler]);
        write(&file, &later);
        assert!(
            !store.facts(&file).unwrap().headless,
            "carried on by a session that says cli"
        );
        assert_eq!(store.session(id).unwrap().external_id, id);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// An id is data a caller supplied: anything not exactly a conversation's own name
    /// selects nothing, and a sub-agent's name is no conversation's (the runtime's
    /// `an_identifier_that_names_no_conversation_finds_nothing`).
    #[test]
    fn an_identifier_that_names_no_conversation_finds_nothing() {
        let root = scratch();
        let id = "11111111-1111-4111-8111-111111111111";
        write(
            &root.join(format!("projects/-private-tmp/{id}.jsonl")),
            &[json!({"type":"user"})],
        );
        write(
            &root.join(format!(
                "projects/-private-tmp/{id}/subagents/agent-a1b2.jsonl"
            )),
            &[json!({"type":"user","isSidechain":true})],
        );
        let store = Store::new(root.clone());
        for attempt in [
            "",
            "..",
            "../../../../etc/passwd",
            "-private-tmp/11111111-1111-4111-8111-111111111111",
            "11111111-1111-4111-8111-111111111111.jsonl",
            "11111111-1111-4111-8111-11111111111",
            "a1b2",
            "agent-a1b2",
        ] {
            assert_eq!(
                refused(store.session(attempt).unwrap_err()),
                refusal::NOT_FOUND,
                "{attempt:?} named a conversation"
            );
        }
        assert_eq!(
            listed(&store).sessions.len(),
            1,
            "a sub-agent is no conversation"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Two transcripts answering to one name are refused, and the listing says it is not
    /// complete (the runtime's `two_transcripts_with_one_name_are_refused`).
    #[test]
    fn two_transcripts_with_one_name_are_refused() {
        let root = scratch();
        let id = "44444444-4444-4444-8444-444444444444";
        for folder in ["-private-tmp", "-private-tmp-elsewhere"] {
            write(
                &root.join(format!("projects/{folder}/{id}.jsonl")),
                &[json!({"type":"user"})],
            );
        }
        let store = Store::new(root.clone());
        assert_eq!(refused(store.session(id).unwrap_err()), refusal::REFUSED);
        let listing = listed(&store);
        assert!(listing.sessions.is_empty() && !listing.complete);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A link planted where Claude Code files its conversations, a folder or a transcript,
    /// is never read (the runtime's `a_transcript_reached_by_leaving_its_harness_is_refused`).
    #[test]
    fn a_transcript_reached_by_a_link_is_never_read() {
        let root = scratch();
        let outside = root.join("outside/-private-tmp");
        write(
            &outside.join("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa.jsonl"),
            &[json!({})],
        );
        std::os::unix::fs::symlink(root.join("outside"), root.join("projects/linked")).unwrap();
        let elsewhere = root.join("somewhere-else.jsonl");
        std::fs::write(&elsewhere, "{}\n").unwrap();
        std::fs::create_dir_all(root.join("projects/-private-tmp")).unwrap();
        std::os::unix::fs::symlink(
            &elsewhere,
            root.join("projects/-private-tmp/bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb.jsonl"),
        )
        .unwrap();
        let store = Store::new(root.clone());
        for id in [
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        ] {
            assert_eq!(
                refused(store.session(id).unwrap_err()),
                refusal::NOT_FOUND,
                "{id}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A conversation says how it began and where: its first stamp, and that the Claude app
    /// started it (`entrypoint` `claude-desktop`); its sub-agents are filed under it, each
    /// named by the record beside it, and read by their own transcript (the runtime's
    /// `every_harness_reports_its_sub_agents_in_one_shape`, `born_in_app` and
    /// `claude_and_codex_sub_agents_are_their_leads`).
    #[test]
    fn a_conversation_says_how_it_began_and_who_works_in_it() {
        let root = scratch();
        let lead = "44444444-4444-4444-8444-444444444444";
        let folder = root.join("projects/-private-tmp");
        write(
            &folder.join(format!("{lead}.jsonl")),
            &[
                json!({"type":"queue-operation","operation":"enqueue"}),
                json!({"type":"user","timestamp":"2026-09-26T10:00:00Z","entrypoint":"claude-desktop","cwd":"/private/tmp","origin":{"kind":"human"},"message":{"role":"user","content":"hi there"}}),
            ],
        );
        write(
            &folder.join(format!("{lead}/subagents/agent-a1b2.jsonl")),
            &[
                json!({"type":"user","isSidechain":true,"agentId":"a1b2","timestamp":"2026-09-26T10:00:10Z","message":{"role":"user","content":"Map the topology"}}),
            ],
        );
        std::fs::write(
            folder.join(format!("{lead}/subagents/agent-a1b2.meta.json")),
            r#"{"agentType":"general-purpose","description":"Explore edge topology","toolUseId":"toolu_1","spawnDepth":1}"#,
        )
        .unwrap();
        write(
            &folder.join(format!("{lead}/subagents/agent-c3d4.jsonl")),
            &[
                json!({"type":"user","isSidechain":true,"timestamp":"2026-09-26T10:00:20Z","message":{"role":"user","content":"\nCheck the docs\nthoroughly"}}),
            ],
        );
        let store = Store::new(root.clone());
        let session = store.session(lead).unwrap();
        assert!(session.app, "started in the Claude app");
        assert_eq!(session.created_at, Some(1_790_416_800_000));
        assert_eq!(session.title.as_deref(), Some("hi there"));
        assert_eq!(
            session.sub_agents,
            [
                SubAgent {
                    external_id: "a1b2".into(),
                    lead: None,
                    name: Some("general-purpose".into()),
                    task: Some("Explore edge topology".into()),
                    started_at: Some(1_790_416_810_000),
                    started_by: Some("toolu_1".into()),
                    transcripts: vec![folder.join(format!("{lead}/subagents/agent-a1b2.jsonl"))],
                },
                SubAgent {
                    external_id: "c3d4".into(),
                    lead: None,
                    name: None,
                    task: Some("Check the docs".into()),
                    started_at: Some(1_790_416_820_000),
                    started_by: None,
                    transcripts: vec![folder.join(format!("{lead}/subagents/agent-c3d4.jsonl"))],
                },
            ]
        );
        let sub = |agent: &str| {
            store.read(Read {
                external_id: lead.to_owned(),
                sub_agent: Some(agent.to_owned()),
                cursor: None,
                tail: None,
            })
        };
        assert!(matches!(
            sub("a1b2").unwrap().events.first(),
            Some(Event::TurnBegan { .. })
        ));
        for missing in ["e5f6", "../x", ""] {
            assert_eq!(
                refused(sub(missing).unwrap_err()),
                refusal::NOT_FOUND,
                "{missing:?}"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A transcript a moment named under another configuration root is read with every
    /// conversation filed beside it.
    #[test]
    fn a_transcript_a_moment_named_elsewhere_is_listed() {
        let root = scratch();
        let other = scratch();
        let file = other.join("projects/-w/77777777-7777-4777-8777-777777777777.jsonl");
        write(&file, &[json!({"type":"user","cwd":"/w"})]);
        write(
            &other.join("projects/-v/88888888-8888-4888-8888-888888888888.jsonl"),
            &[json!({"type":"user"})],
        );
        let store = Store::new(root.clone());
        assert!(listed(&store).sessions.is_empty());
        let listing = store.list(List {
            seen_since: None,
            transcripts: vec![file.clone()],
        });
        assert_eq!(listing.sessions.len(), 2);
        assert_eq!(listed(&store).sessions.len(), 2, "remembered");
        let since = store.list(List {
            seen_since: Some(u64::MAX),
            transcripts: Vec::new(),
        });
        assert!(since.sessions.is_empty() && since.complete);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(other).unwrap();
    }

    /// A read goes on from its cursor; a record still being written is left for the next;
    /// a transcript rewritten starts again; a tail reads only the newest whole records, and
    /// a turn begun before it ends without having begun in it.
    #[test]
    fn a_read_goes_on_from_its_cursor() {
        let root = scratch();
        let id = "99999999-9999-4999-8999-999999999999";
        let file = root.join(format!("projects/-w/{id}.jsonl"));
        let asked = json!({"type":"user","uuid":"u1","parentUuid":null,"origin":{"kind":"human"},"message":{"role":"user","content":"first"}});
        let answered = json!({"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}});
        write(&file, std::slice::from_ref(&asked));
        let mut partial = std::fs::read_to_string(&file).unwrap();
        partial.push_str(r#"{"type":"assistant""#);
        std::fs::write(&file, &partial).unwrap();
        let store = Store::new(root.clone());
        let first = reading(&store, id, None, None);
        assert!(first.whole && !first.reset && !first.more);
        assert!(matches!(
            first.events.as_slice(),
            [
                Event::TurnBegan { .. },
                Event::Words {
                    voice: Voice::Person,
                    ..
                }
            ]
        ));

        write(&file, &[asked.clone(), answered.clone()]);
        let next = reading(&store, id, Some(&first.cursor), None);
        assert!(!next.whole && !next.reset);
        assert!(matches!(
            next.events.as_slice(),
            [
                Event::Words {
                    voice: Voice::Agent,
                    ..
                },
                Event::TurnEnded { .. }
            ]
        ));
        assert!(
            reading(&store, id, Some(&next.cursor), None)
                .events
                .is_empty()
        );

        let tail = reading(
            &store,
            id,
            None,
            Some(answered.to_string().len() as u64 + 1),
        );
        assert!(!tail.whole);
        assert!(matches!(
            tail.events.as_slice(),
            [Event::Words { .. }, Event::TurnEnded { .. }]
        ));

        let replaced = root.join("replaced.jsonl");
        write(&replaced, std::slice::from_ref(&asked));
        std::fs::rename(&replaced, &file).unwrap();
        let again = reading(&store, id, Some(&next.cursor), None);
        assert!(again.reset && again.whole);
        assert!(matches!(
            store.read(Read {
                external_id: id.to_owned(),
                sub_agent: None,
                cursor: Some("1.0.0.0.0".to_owned()),
                tail: None,
            }),
            Err(Error::Invalid(_))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A rewind leaves both paths in the file, and the conversation is the one it took: a
    /// read finds the path no longer passes the record its cursor followed, and starts
    /// again from the beginning along the new one. What the path left behind spent is
    /// still given, once, by its own response (the runtime's
    /// `claude_code_leaves_the_abandoned_branch_alone`).
    #[test]
    fn a_rewind_reads_again_along_the_path_it_took() {
        let root = scratch();
        let id = "abababab-abab-4bab-8bab-abababababab";
        let file = root.join(format!("projects/-w/{id}.jsonl"));
        let answer = |uuid: &str, response: &str, said: &str, output: u64| {
            json!({"type":"assistant","uuid":uuid,"parentUuid":"a","message":{"id":response,"role":"assistant",
                "model":"claude-opus-5","stop_reason":"end_turn","content":[{"type":"text","text":said}],
                "usage":{"input_tokens":10,"output_tokens":output}}})
        };
        let start = json!({"type":"user","uuid":"a","parentUuid":null,"origin":{"kind":"human"},"message":{"role":"user","content":"start here"}});
        write(
            &file,
            &[start.clone(), answer("b", "m1", "the way nobody took", 5)],
        );
        let store = Store::new(root.clone());
        let first = reading(&store, id, None, None);
        let said = |events: &[Event]| -> Vec<String> {
            events
                .iter()
                .filter_map(|event| match event {
                    Event::Words { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect()
        };
        let spent = |events: &[Event]| -> Vec<(String, u64)> {
            events
                .iter()
                .filter_map(|event| match event {
                    Event::Tokens {
                        response, output, ..
                    } => Some((response.clone()?, *output)),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(said(&first.events), ["start here", "the way nobody took"]);

        write(
            &file,
            &[
                start,
                answer("b", "m1", "the way nobody took", 5),
                answer("c", "m2", "the way it went", 7),
            ],
        );
        let rewound = reading(&store, id, Some(&first.cursor), None);
        assert!(rewound.reset && rewound.whole);
        assert_eq!(said(&rewound.events), ["start here", "the way it went"]);
        assert_eq!(
            spent(&rewound.events),
            [("m2".to_owned(), 7), ("m1".to_owned(), 5)],
            "the branch left behind spent too"
        );
        let on = reading(&store, id, Some(&rewound.cursor), None);
        assert!(
            !on.reset && on.events.is_empty(),
            "each response given once"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
