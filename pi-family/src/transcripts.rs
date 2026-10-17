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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use steery_driver_common::follow::Followed;
use steery_driver_common::lines::{Line, RECORD, capped};
use steery_driver_common::tree::{Cursor, Trees};
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use crate::Harness;
use crate::events::{self, Reading, millis, text};
use crate::tree::Entries;

/// How deep sub-agents are filed below their conversation's folder: a sub-agent's, the
/// one it started, and the one that one started (omp 18.4).
const SUB_AGENT_DEPTH: usize = 3;
/// The records the head of a transcript is read from: its `title` and `session`.
const HEAD: usize = 4;
/// The longest head record read.
const HEAD_RECORD: usize = 64 << 10;
/// The longest line of a sub-agent's task said.
const TASK: usize = 100;

/// The harness's conversations here: its agent folder, the folder its environment told it
/// to keep them in, and the folders outside it its moments named, remembered for as long
/// as this driver runs; and the trees, titles and sub-agents' starts of the transcripts
/// read.
pub struct Store {
    harness: Harness,
    home: PathBuf,
    root: PathBuf,
    /// `PI_CODING_AGENT_SESSION_DIR`, where this driver was started with it.
    chosen: Option<PathBuf>,
    named: Mutex<BTreeSet<PathBuf>>,
    trees: Trees<Entries>,
    titles: Titles,
    starts: Starts,
    listings: Listings,
}

/// Every transcript filed as a conversation, by the id it is filed under.
struct Filed {
    by_id: BTreeMap<String, Vec<PathBuf>>,
    /// Every folder was read whole.
    complete: bool,
}

impl Store {
    pub fn new(harness: Harness, home: &Path, root: PathBuf, chosen: Option<PathBuf>) -> Self {
        Self {
            harness,
            home: home.to_owned(),
            root,
            chosen,
            named: Mutex::new(BTreeSet::new()),
            trees: Trees::default(),
            titles: Titles::default(),
            starts: Starts::default(),
            listings: Listings::default(),
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
                let found = self
                    .sub_agents(&file)
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
    /// Looked up in each folder's listing as it stands, never by indexing every file: the
    /// runtime asks this of every conversation on every sweep.
    pub fn transcripts(&self, id: &str) -> Vec<PathBuf> {
        if id.is_empty() || id.chars().any(char::is_control) {
            return Vec::new();
        }
        let (folders, _) = self.folders();
        let mut found = Vec::new();
        for folder in folders {
            if let Some((files, _)) = self.listings.files(&folder) {
                found.extend(files.iter().filter(|path| id_of(path) == Some(id)).cloned());
            }
        }
        found
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
        // Each of the harness's own folders is a directory (never a link) right in
        // `sessions`, so it resolves to `own/<name>`: a folder named elsewhere whose parent
        // is not `own` is none of them, with no `realpath` of theirs. Resolving each of 107
        // on every call made `held` 20 times slower (0.8 to 16.7 ms) once one folder outside
        // was named, and a sweep's calls kept the driver busy (2026-10-17).
        let mut seen: BTreeSet<PathBuf> = match &own {
            Some(_) => BTreeSet::new(),
            None => folders
                .iter()
                .filter_map(|folder| folder.canonicalize().ok())
                .collect(),
        };
        let chosen = self
            .chosen
            .clone()
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
            let Some((files, whole)) = self.listings.files(&folder) else {
                complete = false;
                continue;
            };
            complete &= whole;
            for path in files.iter() {
                if let Some(id) = id_of(path) {
                    by_id.entry(id.to_owned()).or_default().push(path.clone());
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
            sub_agents: self.sub_agents(file),
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
    first_line(asked, MOST)
}

/// The first line of `said` that is no markup, as at most `most` characters of whole
/// words.
fn first_line(said: &str, most: usize) -> Option<String> {
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
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > most {
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

impl Store {
    /// The sub-agents filed in a conversation's folder, and in theirs: each a transcript
    /// whose header names who started it (`parentSession`), named for its job, with the
    /// `task` call that started it and what that call asked of it.
    fn sub_agents(&self, file: &Path) -> Vec<SubAgent> {
        let mut found = Vec::new();
        self.visit(file, None, 1, &mut found);
        found.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.external_id.cmp(&b.external_id))
        });
        found
    }

    /// The sub-agents `starter` started, filed in the folder named for its transcript, and
    /// those they started in turn, `lead` being the sub-agent `starter` is.
    fn visit(&self, starter: &Path, lead: Option<&str>, depth: usize, out: &mut Vec<SubAgent>) {
        let folder = starter.with_extension("");
        let Some((files, _)) = self.listings.files(&folder) else {
            return;
        };
        let filed: Vec<(&PathBuf, String, Arc<SubHead>)> = files
            .iter()
            .filter_map(|path| {
                let job = path.file_stem()?.to_str()?.to_owned();
                Some((path, job, self.listings.head(&folder, path)?))
            })
            .collect();
        if filed.is_empty() {
            return;
        }
        let jobs = self
            .starts
            .jobs(starter, filed.iter().map(|(_, job, _)| job.as_str()));
        for ((path, job, head), started) in filed.into_iter().zip(jobs) {
            let external_id = head.id.clone().unwrap_or_else(|| job.clone());
            let (started_by, task) =
                started.map_or((None, None), |started| (Some(started.call), started.task));
            out.push(SubAgent {
                external_id: external_id.clone(),
                lead: lead.map(str::to_owned),
                name: Some(job),
                task,
                started_at: head.created_at,
                started_by,
                transcripts: vec![path.clone()],
            });
            if depth < SUB_AGENT_DEPTH {
                self.visit(path, Some(&external_id), depth + 1, out);
            }
        }
    }
}

/// How old a folder's change stamp must be before what was read of it is kept: a stamp
/// as coarse as a whole second (HFS+, some network folders) could hide a file added in
/// the tick it was listed in.
const SETTLED: Duration = Duration::from_secs(2);

/// The transcripts in each folder read, as they stood when it last changed, and the heads
/// of its sub-agents' transcripts: a folder is listed again, and its heads read again, only
/// once its own change stamp moved - a transcript added, removed, renamed or made anew in
/// it - and never while that stamp is younger than [`SETTLED`]. A transcript's `session`
/// record is written once, where it starts. Every `sessions/read` finds its conversation
/// among all of them, and a sub-agent's among its siblings: measured 2026-10-10 on a Mac
/// with 104 omp folders, 2,798 transcripts and 755 sub-agents read every sweep, that
/// listing and those heads made each read 12 ms and the runtime's sweep wait 8-16 s.
#[derive(Default)]
struct Listings {
    kept: Mutex<HashMap<PathBuf, Listing>>,
}

struct Listing {
    changed: SystemTime,
    files: Arc<[PathBuf]>,
    /// Each sub-agent's head read from its file, by the file.
    heads: HashMap<PathBuf, Arc<SubHead>>,
}

/// What a sub-agent's `session` record says of it.
struct SubHead {
    id: Option<String>,
    created_at: Option<u64>,
}

impl Listings {
    /// The `.jsonl` files in `folder`, never a link, and whether every entry was read;
    /// `None` where it cannot be read.
    fn files(&self, folder: &Path) -> Option<(Arc<[PathBuf]>, bool)> {
        // Stamped before it is read: a change while it is read moves the stamp past this.
        let changed = std::fs::metadata(folder).and_then(|m| m.modified()).ok()?;
        if let Some(listing) = self.lock().get(folder)
            && listing.changed == changed
        {
            return Some((listing.files.clone(), true));
        }
        let mut whole = true;
        let mut files = Vec::new();
        for entry in std::fs::read_dir(folder).ok()? {
            let Ok(entry) = entry else {
                whole = false;
                continue;
            };
            let path = entry.path();
            // A link is never read: it could name a file anywhere.
            if entry.file_type().is_ok_and(|kind| kind.is_file())
                && path.extension().is_some_and(|ext| ext == "jsonl")
            {
                files.push(path);
            }
        }
        let files: Arc<[PathBuf]> = files.into();
        let mut kept = self.lock();
        if whole && changed.elapsed().is_ok_and(|age| age >= SETTLED) {
            let listing = Listing {
                changed,
                files: files.clone(),
                heads: HashMap::new(),
            };
            kept.insert(folder.to_owned(), listing);
        } else {
            kept.remove(folder);
        }
        Some((files, whole))
    }

    /// The head of `file` in `folder`, where it is a sub-agent's: its header names who
    /// started it (`parentSession`).
    fn head(&self, folder: &Path, file: &Path) -> Option<Arc<SubHead>> {
        if let Some(head) = self
            .lock()
            .get(folder)
            .and_then(|listing| listing.heads.get(file))
        {
            return Some(head.clone());
        }
        let read = head(file);
        read.parent.as_ref()?;
        let found = Arc::new(SubHead {
            id: read.id,
            created_at: read.created_at,
        });
        if let Some(listing) = self.lock().get_mut(folder) {
            listing.heads.insert(file.to_owned(), found.clone());
        }
        Some(found)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Listing>> {
        self.kept.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What the `task` calls of each transcript started, read on from where the last read left
/// the file, which omp only appends to.
#[derive(Default)]
struct Starts {
    kept: Mutex<HashMap<PathBuf, Started>>,
}

/// The call that started a sub-agent, and the first line of what it asked of it.
#[derive(Clone)]
struct Start {
    call: String,
    task: Option<String>,
}

/// What one transcript's `task` calls started, as far as it has been read.
#[derive(Default)]
struct Started {
    /// How far it was read: a file written whole again is read again from its start.
    followed: Followed,
    /// By job, as omp's result names each sub-agent it filed (`details.progress[].id`).
    jobs: BTreeMap<String, Start>,
    /// The tasks of a call whose result is not written yet, by the call: each task's name,
    /// which its job is, or ends with after a `.`.
    asked: BTreeMap<String, Vec<(String, Start)>>,
}

impl Starts {
    /// What started each of `jobs`, filed for `starter`.
    fn jobs<'a>(&self, starter: &Path, jobs: impl Iterator<Item = &'a str>) -> Vec<Option<Start>> {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        let started = kept.entry(starter.to_owned()).or_default();
        if started.read_on(starter).is_err() {
            *started = Started::default();
        }
        jobs.map(|job| started.job(job).cloned()).collect()
    }
}

impl Started {
    fn job(&self, job: &str) -> Option<&Start> {
        self.jobs.get(job).or_else(|| {
            self.asked
                .values()
                .flatten()
                .find(|(name, _)| {
                    job.strip_suffix(name.as_str())
                        .is_some_and(|before| before.is_empty() || before.ends_with('.'))
                })
                .map(|(_, start)| start)
        })
    }

    fn read_on(&mut self, file: &Path) -> io::Result<()> {
        let open = File::open(file)?;
        if self.followed.restart(&open)? {
            self.jobs.clear();
            self.asked.clear();
        }
        let mut messages = Vec::new();
        self.followed.read_on(&open, |line, _, _| {
            if line.len() <= RECORD
                && line.windows(6).any(|bytes| bytes == b"\"task\"")
                && let Ok(mut record) = serde_json::from_slice::<Value>(line)
                && text(&record, "type") == Some("message")
                && let Some(message) = record.get_mut("message")
            {
                messages.push(message.take());
            }
            true
        })?;
        for message in &messages {
            self.record(message);
        }
        Ok(())
    }

    /// A `task` call, each of its tasks named; and its result, naming the job each task
    /// was filed as, by the task's place in the call.
    fn record(&mut self, message: &Value) {
        let task = |said: Option<&str>| said.and_then(|said| first_line(said, TASK));
        match text(message, "role") {
            Some("assistant") => {
                let calls = message
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|block| {
                        text(block, "type") == Some("toolCall")
                            && text(block, "name") == Some("task")
                    });
                for block in calls {
                    let Some(call) = text(block, "id") else {
                        continue;
                    };
                    let tasks = block
                        .pointer("/arguments/tasks")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|asked| {
                            let name = text(asked, "name").filter(|name| !name.is_empty())?;
                            Some((
                                name.to_owned(),
                                Start {
                                    call: call.to_owned(),
                                    task: task(text(asked, "task")),
                                },
                            ))
                        })
                        .collect();
                    self.asked.insert(call.to_owned(), tasks);
                }
            }
            Some("toolResult") if text(message, "toolName") == Some("task") => {
                let Some(call) = text(message, "toolCallId") else {
                    return;
                };
                let filed: Vec<&Value> = message
                    .pointer("/details/progress")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .collect();
                if filed.is_empty() {
                    return;
                }
                let asked = self.asked.remove(call).unwrap_or_default();
                for job in filed {
                    let Some(id) = text(job, "id").filter(|id| !id.is_empty()) else {
                        continue;
                    };
                    let said = job
                        .get("index")
                        .and_then(Value::as_u64)
                        .and_then(|at| asked.get(usize::try_from(at).ok()?))
                        .and_then(|(_, start)| start.task.clone());
                    self.jobs.insert(
                        id.to_owned(),
                        Start {
                            call: call.to_owned(),
                            task: task(text(job, "assignment")).or(said),
                        },
                    );
                }
            }
            _ => {}
        }
    }
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

/// What a cursor's `mark` keeps: whether the fork was said. Its cursors are
/// `3.<device>.<inode>.<tip>.<at>.<aside>.<forked>` (`steery_driver_common::tree::Cursor`).
const CURSOR: &str = "3";

fn parse_cursor(cursor: &str) -> Result<Cursor, Error> {
    Cursor::parse(CURSOR, cursor)
        .filter(|cursor| cursor.mark <= 1)
        .ok_or_else(|| Error::invalid(format!("`{cursor}` is no cursor this driver gave")))
}

/// The events of a transcript's active path after `cursor`, or of its entries within
/// `tail` bytes of its end (`Walk::read`). What a branch left behind spent is still
/// spent: once the path is read to its end, every entry off it written since the last
/// read is given as its `tokens` alone, each response once.
fn read_from(
    trees: &Trees<Entries>,
    reading: &Reading,
    cursor: Option<&str>,
    tail: Option<u64>,
) -> Result<Events, Error> {
    let given = cursor.map(parse_cursor).transpose()?;
    let still = std::fs::metadata(&reading.file)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .is_some_and(|still| still >= events::SETTLE);
    trees
        .walk(&reading.file, Entries::default(), |walk| {
            let path = walk.tree.path()?;
            let read = walk.read(&path, given.as_ref(), tail)?;
            let records = &read.records;
            // Where a tail begins inside a fork's copy of its parent, the fork is still to
            // come.
            let forked = match read.mark {
                Some(mark) => mark == 1,
                None if read.from == 0 => false,
                None => reading.fork.as_ref().is_none_or(|(_, began)| {
                    records
                        .iter()
                        .filter_map(|record| record.value.as_ref())
                        .find_map(|record| text(record, "timestamp").and_then(millis))
                        .is_none_or(|at| at >= *began)
                }),
            };
            let end = if read.rest || !still {
                events::End::Open
            } else {
                events::End::Still
            };
            let mut said = events::read(reading, records, forked, end);
            // A batch that held back everything it read would never move on: what it held
            // stands.
            if read.rest && said.consumed == 0 {
                said = events::read(reading, records, forked, events::End::Forced);
            }
            for record in &read.aside {
                if let Some(value) = &record.value {
                    said.events.extend(events::spent(reading, value));
                }
            }
            Ok(Events {
                cursor: walk
                    .cursor(
                        &path,
                        read.from + said.consumed,
                        read.through,
                        u64::from(said.forked),
                    )
                    .said(CURSOR),
                events: said.events,
                more: read.more,
                reset: read.reset,
                whole: read.from == 0,
            })
        })
        .map_err(|why| Error::refused(refusal::REFUSED, why))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use steery_sdk::wire::harness::{Event, Voice};

    /// A home of one test's own, gone once the test ends.
    pub(crate) struct Home(pub PathBuf);

    impl Home {
        pub(crate) fn new(name: &str) -> Self {
            static MADE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let made = MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let home = std::env::temp_dir().join(format!(
                "steery-pi-family-{name}-{}-{made}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home).unwrap();
            Self(home)
        }

        /// A transcript at `path` under the home, one record a line.
        pub(crate) fn write<S: AsRef<str>>(&self, path: &str, records: &[S]) -> PathBuf {
            let file = self.0.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            let lines: String = records
                .iter()
                .map(|record| format!("{}\n", record.as_ref()))
                .collect();
            std::fs::write(&file, lines).unwrap();
            file
        }

        /// More records at the end of a transcript.
        pub(crate) fn append<S: AsRef<str>>(&self, file: &Path, records: &[S]) {
            let mut open = std::fs::OpenOptions::new().append(true).open(file).unwrap();
            for record in records {
                std::io::Write::write_all(&mut open, format!("{}\n", record.as_ref()).as_bytes())
                    .unwrap();
            }
        }

        /// The harness's conversations under this home, its own agent folder's.
        pub(crate) fn store(&self, harness: Harness, chosen: Option<PathBuf>) -> Store {
            let root = self.0.join(match harness {
                Harness::Omp => ".omp/agent",
                Harness::Pi => ".pi/agent",
            });
            Store::new(harness, &self.0, root, chosen)
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// `sessions/read` of a conversation, or of one of its sub-agents, on from `cursor`.
    pub(crate) fn read(
        store: &Store,
        id: &str,
        sub_agent: Option<&str>,
        cursor: Option<&str>,
    ) -> Events {
        store
            .read(Read {
                external_id: id.into(),
                sub_agent: sub_agent.map(str::to_owned),
                cursor: cursor.map(str::to_owned),
                tail: None,
            })
            .unwrap()
    }

    fn listed(store: &Store, transcripts: &[PathBuf]) -> Listed {
        store.list(List {
            seen_since: None,
            transcripts: transcripts.to_vec(),
        })
    }

    fn ids(listed: &Listed) -> Vec<&str> {
        listed
            .sessions
            .iter()
            .map(|session| session.external_id.as_str())
            .collect()
    }
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
        let store = Store::new(Harness::Pi, &home, home.join(".pi/agent"), None);
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
        let store = Store::new(Harness::Pi, &home, home.join(".pi/agent"), None);
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

    /// A long, branch-heavy transcript read for the first time is said in batches: what
    /// the branches left behind spent counts against the same bound as the path, so no
    /// read runs past its own, and the cursor goes on from where it stopped among them
    /// until every response off the path is given, each once.
    #[test]
    fn branches_left_behind_are_read_in_batches_and_each_once() {
        let home = std::env::temp_dir().join(format!("steery-pi-batches-{}", std::process::id()));
        let folder = home.join(".pi/agent/sessions/--tmp--");
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("2026-10-01T10-00-00-000Z_c3.jsonl");
        let answer = |id: &str, said: &str| {
            format!(
                "{}\n",
                serde_json::json!({"type": "message", "id": id, "parentId": "a",
                    "timestamp": "2026-10-01T10:00:00Z",
                    "message": {"role": "assistant", "stopReason": "stop",
                        "content": [{"type": "text", "text": said}],
                        "usage": {"input": 10, "output": 1}}})
            )
        };
        let mut text = format!(
            "{}{}",
            "{\"type\":\"session\",\"version\":3,\"id\":\"c3\",\"timestamp\":\"2026-10-01T10:00:00Z\"}\n",
            entry("a", None, "user", "ask"),
        );
        // Six branches, each leaving 1 MiB behind.
        for n in 0..6 {
            text.push_str(&answer(&format!("gone{n}"), &"x".repeat(1 << 20)));
        }
        text.push_str(&answer("kept", "the way it went"));
        std::fs::write(&file, text).unwrap();
        let store = Store::new(Harness::Pi, &home, home.join(".pi/agent"), None);
        let mut cursor = None;
        let mut responses = Vec::new();
        let mut reads = 0;
        loop {
            let read = store
                .read(Read {
                    external_id: "c3".into(),
                    sub_agent: None,
                    cursor,
                    tail: None,
                })
                .unwrap();
            reads += 1;
            assert!(!read.reset);
            responses.extend(read.events.iter().filter_map(|event| match event {
                Event::Tokens { response, .. } => response.clone(),
                _ => None,
            }));
            cursor = Some(read.cursor);
            if !read.more {
                break;
            }
            assert!(reads < 10, "a read that never ends");
        }
        assert!(reads > 1, "more than one batch");
        assert_eq!(
            responses,
            ["kept", "gone0", "gone1", "gone2", "gone3", "gone4", "gone5"]
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    /// omp files a sub-agent inside its lead's folder, `<lead>/<job>.jsonl`, and one a
    /// sub-agent started inside that one's: each is the conversation's sub-agent, named
    /// for its job, with the `task` call that started it and the first line of what that
    /// call asked of it — by the job its result names, or while no result is written yet,
    /// by the task's name. None is a conversation, even where a moment names its file.
    #[test]
    fn sub_agents_are_their_conversations_and_never_conversations_of_their_own() {
        let home = Home::new("sub-agents");
        let lead = "01a10000-0000-7000-8000-000000000001";
        let scout = "01a10000-0000-7000-8000-000000000002";
        let probe = "01a10000-0000-7000-8000-000000000003";
        let filed = format!(".omp/agent/sessions/--tmp--/2026-10-09T10-00-00-000Z_{lead}");
        let lead_file = home.write(
            &format!("{filed}.jsonl"),
            &[
                r#"{"type":"title","v":1,"title":""}"#.to_owned(),
                format!(
                    r#"{{"type":"session","version":3,"id":"{lead}","timestamp":"2026-10-09T10:00:00Z","cwd":"/tmp"}}"#
                ),
                r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-10-09T10:00:00Z","message":{"role":"user","content":[{"type":"text","text":"map it"}]}}"#.to_owned(),
                r##"{"type":"message","id":"b","parentId":"a","timestamp":"2026-10-09T10:00:05Z","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"toolCall","id":"toolu_1","name":"task","arguments":{"agent":"task","tasks":[{"name":"Scout","task":"# Target\nMap the dashboard\nwith pointers"}]}}]}}"##.to_owned(),
                r##"{"type":"message","id":"c","parentId":"b","timestamp":"2026-10-09T10:00:06Z","message":{"role":"toolResult","toolCallId":"toolu_1","toolName":"task","content":[{"type":"text","text":"started"}],"details":{"async":true,"progress":[{"index":0,"id":"Scout","status":"pending","assignment":"# Target\nMap the dashboard\nwith pointers"}]}}}"##.to_owned(),
            ],
        );
        let scout_file = home.write(
            &format!("{filed}/Scout.jsonl"),
            &[
                r#"{"type":"title","v":1,"title":""}"#.to_owned(),
                format!(
                    r#"{{"type":"session","version":3,"id":"{scout}","parentSession":"{}","timestamp":"2026-10-09T10:01:00Z","cwd":"/tmp"}}"#,
                    lead_file.display()
                ),
                r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-10-09T10:01:00Z","message":{"role":"user","attribution":"agent","content":[{"type":"text","text":"Complete assignment thoroughly:\n\n# Target\nMap the dashboard"}]}}"#.to_owned(),
                r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-10-09T10:01:05Z","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"toolCall","id":"toolu_2","name":"task","arguments":{"agent":"explore","tasks":[{"name":"Probe","task":"Find the two readers"}]}}]}}"#.to_owned(),
            ],
        );
        home.write(
            &format!("{filed}/Scout/Scout.Probe.jsonl"),
            &[
                format!(
                    r#"{{"type":"session","version":3,"id":"{probe}","parentSession":"{}","timestamp":"2026-10-09T10:02:00Z","cwd":"/tmp"}}"#,
                    scout_file.display()
                ),
                r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-10-09T10:02:00Z","message":{"role":"user","attribution":"agent","content":[{"type":"text","text":"Find the two readers"}]}}"#.to_owned(),
                r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-10-09T10:02:09Z","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"found two"}]}}"#.to_owned(),
            ],
        );
        let store = home.store(Harness::Omp, None);
        let sub_agent = |external_id: &str,
                         lead: Option<&str>,
                         name: &str,
                         task: &str,
                         started_at: &str,
                         started_by: &str,
                         file: PathBuf| SubAgent {
            external_id: external_id.to_owned(),
            lead: lead.map(str::to_owned),
            name: Some(name.to_owned()),
            task: Some(task.to_owned()),
            started_at: millis(started_at),
            started_by: Some(started_by.to_owned()),
            transcripts: vec![file],
        };
        let mut expected = vec![
            sub_agent(
                scout,
                None,
                "Scout",
                "Map the dashboard",
                "2026-10-09T10:01:00Z",
                "toolu_1",
                scout_file.clone(),
            ),
            sub_agent(
                probe,
                Some(scout),
                "Scout.Probe",
                "Find the two readers",
                "2026-10-09T10:02:00Z",
                "toolu_2",
                home.0.join(format!("{filed}/Scout/Scout.Probe.jsonl")),
            ),
        ];

        // A moment named the sub-agent's file: still no conversation of its own.
        let first = listed(&store, std::slice::from_ref(&scout_file));
        assert_eq!(ids(&first), [lead]);
        assert!(first.complete && first.excluded.is_empty());
        assert_eq!(first.sessions[0].sub_agents, expected);

        let probed = read(&store, lead, Some(probe), None);
        assert!(probed.events.iter().any(|event| matches!(
            event,
            Event::Words { voice: Voice::Agent, text, .. } if text == "found two"
        )));

        // The lead starts another: read on from where its transcript was read.
        home.append(
            &lead_file,
            &[r#"{"type":"message","id":"d","parentId":"c","timestamp":"2026-10-09T10:03:00Z","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"toolCall","id":"toolu_3","name":"task","arguments":{"agent":"task","tasks":[{"name":"Second","task":"Check the other one"}]}}]}}"#],
        );
        let second = home.write(
            &format!("{filed}/Second.jsonl"),
            &[format!(
                r#"{{"type":"session","version":3,"id":"second","parentSession":"{}","timestamp":"2026-10-09T10:03:01Z"}}"#,
                lead_file.display()
            )],
        );
        expected.push(sub_agent(
            "second",
            None,
            "Second",
            "Check the other one",
            "2026-10-09T10:03:01Z",
            "toolu_3",
            second,
        ));
        assert_eq!(listed(&store, &[]).sessions[0].sub_agents, expected);
    }

    /// omp's title is the one it keeps current at the head of its file, else the one the
    /// conversation started with; pi's is its newest `session_info` name, else what its
    /// person asked for first. Each reads the same file its own way.
    #[test]
    fn each_harness_titles_its_conversations_its_own_way() {
        let home = Home::new("titles");
        let both = [
            r#"{"type":"title","v":1,"title":"What omp calls it","source":"auto"}"#,
            r#"{"type":"session","version":3,"id":"t1","title":"What it was called first","cwd":"/tmp","timestamp":"2026-10-09T10:00:00Z"}"#,
            r#"{"type":"session_info","id":"b","parentId":null,"name":"What pi calls it"}"#,
            r#"{"type":"message","id":"c","parentId":"b","timestamp":"2026-10-09T10:00:01Z","message":{"role":"user","content":[{"type":"text","text":"fix the build"}]}}"#,
        ];
        home.write(
            ".omp/agent/sessions/--tmp--/2026-10-09T10-00-00-000Z_t1.jsonl",
            &both,
        );
        home.write(
            ".pi/agent/sessions/--tmp--/2026-10-09T10-00-00-000Z_t1.jsonl",
            &both,
        );
        home.write(
            ".omp/agent/sessions/--tmp--/2026-10-09T10-00-00-000Z_t2.jsonl",
            &[
                r#"{"type":"title","v":1,"title":""}"#,
                r#"{"type":"session","version":3,"id":"t2","title":"What it was called first","timestamp":"2026-10-09T10:00:00Z"}"#,
            ],
        );
        home.write(
            ".pi/agent/sessions/--tmp--/2026-10-09T10-00-00-000Z_t2.jsonl",
            &[
                r#"{"type":"session","version":3,"id":"t2","timestamp":"2026-10-09T10:00:00Z"}"#,
                r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-10-09T10:00:01Z","message":{"role":"toolResult","toolCallId":"k1","content":[{"type":"text","text":"total 320"}]}}"#,
                r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-10-09T10:00:02Z","message":{"role":"user","content":[{"type":"text","text":"why pi never got the steery skill"}]}}"#,
                r#"{"type":"session_info","id":"c","parentId":"b","name":"For a while"}"#,
                r#"{"type":"session_info","id":"d","parentId":"c"}"#,
            ],
        );
        let titles = |harness| {
            let mut titles: Vec<(String, Option<String>)> = listed(&home.store(harness, None), &[])
                .sessions
                .into_iter()
                .map(|session| (session.external_id, session.title))
                .collect();
            titles.sort();
            titles
        };
        let title = |id: &str, title: &str| (id.to_owned(), Some(title.to_owned()));
        assert_eq!(
            titles(Harness::Omp),
            [
                title("t1", "What omp calls it"),
                title("t2", "What it was called first")
            ]
        );
        assert_eq!(
            titles(Harness::Pi),
            [
                title("t1", "What pi calls it"),
                title("t2", "why pi never got the steery skill")
            ]
        );
    }

    /// A conversation is found wherever the harness was told to keep it, straight in that
    /// folder: pi's `sessionDir` (`~` its person's home), `PI_CODING_AGENT_SESSION_DIR`, and
    /// the folder of a transcript a moment named, remembered from then on. A chosen folder
    /// that is one of the agent folder's own is walked once; two transcripts for one id
    /// are neither read, and the listing says it is not complete.
    #[test]
    fn conversations_are_found_wherever_the_harness_was_told_to_keep_them() {
        let home = Home::new("chosen");
        let transcript = |folder: &str, id: &str| {
            home.write(
                &format!("{folder}/2026-10-09T10-00-00-000Z_{id}.jsonl"),
                &[
                    format!(
                        r#"{{"type":"session","version":3,"id":"{id}","cwd":"/tmp/pi","timestamp":"2026-10-09T10:00:00Z"}}"#
                    ),
                    r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-10-09T10:00:01Z","message":{"role":"user","content":[{"type":"text","text":"where pi keeps it"}]}}"#.to_owned(),
                ],
            )
        };
        let settings = |dir: &str| {
            std::fs::create_dir_all(home.0.join(".pi/agent")).unwrap();
            std::fs::write(
                home.0.join(".pi/agent/settings.json"),
                serde_json::json!({ "sessionDir": dir }).to_string(),
            )
            .unwrap();
        };
        let pi = home.store(Harness::Pi, None);

        settings("~/elsewhere");
        let file = transcript("elsewhere", "p1");
        let found = listed(&pi, &[]);
        assert_eq!(ids(&found), ["p1"]);
        assert_eq!(found.sessions[0].cwd.as_deref(), Some(Path::new("/tmp/pi")));
        assert_eq!(
            found.sessions[0].title.as_deref(),
            Some("where pi keeps it")
        );
        // Found where the folder resolves: on a Mac the temporary folder is behind `/var`,
        // a link to `/private/var`.
        assert_eq!(pi.find("p1").unwrap(), file.canonicalize().unwrap());

        std::fs::remove_file(&file).unwrap();
        let own = transcript(".pi/agent/sessions/--tmp-pi--", "p1");
        settings(own.parent().unwrap().to_str().unwrap());
        let once = listed(&pi, &[]);
        assert_eq!(ids(&once), ["p1"]);
        assert!(once.complete);

        transcript(".pi/agent/sessions/--tmp-pi-again--", "p1");
        let twice = listed(&pi, &[]);
        assert!(ids(&twice).is_empty() && !twice.complete);
        assert!(pi.find("p1").is_err());

        let omp = home.store(Harness::Omp, Some(home.0.join("by-environment")));
        transcript("by-environment", "o1");
        let outside = transcript("outside", "o2");
        assert_eq!(ids(&listed(&omp, &[])), ["o1"]);
        let named = listed(&omp, std::slice::from_ref(&outside));
        let mut found = ids(&named);
        found.sort_unstable();
        assert_eq!(found, ["o1", "o2"]);
        assert_eq!(ids(&listed(&omp, &[])).len(), 2, "remembered");
    }
}
