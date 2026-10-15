//! Codex's conversations, from its rollouts: `<CODEX_HOME>/sessions/<yyyy>/<mm>/<dd>/
//! rollout-<started-at>-<thread>.jsonl`.
//!
//! A rollout opens with `session_meta`: the thread's own `id`, the conversation its person
//! is in (`session_id`, equal to `id` except in a sub-agent's), its folder, what started it
//! (`originator`, `source`), and where it came from: `forked_from_id`, and `history_base
//! {thread_id, end_byte_offset}`, the rollout whose first bytes are this thread's history
//! (Codex 0.162: a fork names its parent there and copies nothing). A rollout continued in
//! another file is a shard of the same thread whose `history_base` names the thread itself.
//! A sub-agent's rollout is filed beside its lead's, under the same name pattern.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufReader, Read as _, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_driver_common::lines::{BATCH, Line, RECORD, capped, consumed, first_whole};
use steery_sdk::wire::harness::{
    Event, Events, List, Listed, Locate, Located, Read, Session, SubAgent,
};
use steery_sdk::{Error, refusal};

use crate::events::{self, Place, State, stamped, text};
use crate::words;

/// How many opening records are read for the first thing a person asked.
const OPENING: usize = 64;
/// How many threads up a history is followed.
const DEPTH: usize = 8;
/// The longest title and sub-agent task said.
const TASK: usize = 200;

/// Codex's conversations here: its home, and the rollouts its moments named.
pub struct Store {
    root: PathBuf,
    named: Mutex<BTreeSet<PathBuf>>,
    heads: Mutex<HashMap<PathBuf, Kept>>,
}

/// A rollout's opening as last read: the file it was read from, by its device and inode,
/// how long that file was then, and whether the opening is whole (`Opening::whole`).
struct Kept {
    dev: u64,
    ino: u64,
    len: u64,
    opening: Opening,
}

/// What a rollout's opening says, and whether more of the file could change it: an
/// opening is whole once it found the first thing a person asked, or read every record
/// of the opening ([`OPENING`]) without reaching the end of the file.
#[derive(Clone)]
struct Opening {
    head: Head,
    whole: bool,
}

/// What a rollout's opening says.
#[derive(Debug, Clone, Default)]
pub struct Head {
    pub id: Option<String>,
    /// The conversation its person is in.
    pub session_id: Option<String>,
    pub born: Option<u64>,
    pub first_at: Option<u64>,
    pub cwd: Option<PathBuf>,
    pub originator: Option<String>,
    pub source: Option<String>,
    pub forked_from: Option<String>,
    /// `history_base`: the thread whose rollout's first bytes are this one's history.
    pub base: Option<(String, u64)>,
    /// A sub-agent's: the thread that spawned it, and what Codex calls it.
    pub spawned_by: Option<String>,
    pub nickname: Option<String>,
    /// The first thing a person asked, in one line.
    pub asked: Option<String>,
}

impl Head {
    /// A shard continuing its own thread.
    fn continues(&self) -> bool {
        self.base
            .as_ref()
            .is_some_and(|(thread, _)| Some(thread) == self.id.as_ref())
    }

    /// The thread this one was forked from.
    fn parent(&self) -> Option<&str> {
        let own = self.id.as_deref();
        self.forked_from
            .as_deref()
            .or(self.base.as_ref().map(|(thread, _)| thread.as_str()))
            .filter(|parent| !parent.is_empty() && Some(*parent) != own)
    }

    fn sub_agent(&self) -> bool {
        self.session_id
            .as_ref()
            .zip(self.id.as_ref())
            .is_some_and(|(conversation, own)| conversation != own)
    }
}

/// One rollout as filed: its path, when it last changed, and its opening.
#[derive(Clone)]
struct Filed {
    file: PathBuf,
    modified: u64,
    head: Head,
}

/// Every rollout here by the thread it is, and whether every folder was read whole.
struct Threads {
    by_id: BTreeMap<String, Vec<Filed>>,
    complete: bool,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            named: Mutex::new(BTreeSet::new()),
            heads: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `sessions/list`.
    pub fn list(&self, list: List) -> Listed {
        self.remember(&list.transcripts);
        let threads = self.threads();
        let mut complete = threads.complete;
        let mut excluded = Vec::new();
        let mut subs: BTreeMap<String, Vec<SubAgent>> = BTreeMap::new();
        let mut sessions = Vec::new();
        for (id, filed) in &threads.by_id {
            let head = &newest(filed).head;
            if head.sub_agent() {
                excluded.push(id.clone());
                if let Some(conversation) = &head.session_id {
                    subs.entry(conversation.clone())
                        .or_default()
                        .push(sub_agent(id, conversation, filed));
                }
                continue;
            }
            if headless(head) {
                excluded.push(id.clone());
                continue;
            }
            match session(id, filed) {
                Some(session) => sessions.push(session),
                None => complete = false,
            }
        }
        for session in &mut sessions {
            if let Some(mut found) = subs.remove(&session.external_id) {
                found.sort_by(|a, b| {
                    a.started_at
                        .cmp(&b.started_at)
                        .then_with(|| a.external_id.cmp(&b.external_id))
                });
                session.sub_agents = found;
            }
        }
        sessions.retain(|session| {
            list.seen_since
                .is_none_or(|since| session.updated_at >= since)
        });
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        Listed {
            sessions,
            excluded,
            complete,
        }
    }

    /// `sessions/locate`.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let threads = self.threads();
        let filed = self.family(&threads, &locate.external_id)?;
        let head = &newest(filed).head;
        if head.sub_agent() || headless(head) {
            return Err(Error::refused(
                refusal::NOT_FOUND,
                format!("{} is no conversation a person is in", locate.external_id),
            ));
        }
        let mut session = session(&locate.external_id, filed).ok_or_else(|| {
            Error::refused(
                refusal::REFUSED,
                format!(
                    "more than one rollout of {} is equally current",
                    locate.external_id
                ),
            )
        })?;
        session.sub_agents = threads
            .by_id
            .iter()
            .filter(|(_, filed)| {
                let head = &newest(filed).head;
                head.sub_agent() && head.session_id.as_deref() == Some(locate.external_id.as_str())
            })
            .map(|(id, filed)| sub_agent(id, &locate.external_id, filed))
            .collect();
        Ok(Located { session })
    }

    /// `sessions/read`: the events of a conversation, or of one of its sub-agents, after a
    /// cursor. The history a thread was forked from is read first, cut where the fork
    /// took it, and a `fork` event marks where its own begins.
    pub fn read(&self, read: Read) -> Result<Events, Error> {
        let threads = self.threads();
        let id = read.sub_agent.as_deref().unwrap_or(&read.external_id);
        let filed = self.family(&threads, id)?;
        if let Some(agent) = &read.sub_agent {
            let head = &newest(filed).head;
            if head.session_id.as_deref() != Some(read.external_id.as_str()) {
                return Err(Error::refused(
                    refusal::NOT_FOUND,
                    format!("Codex has no sub-agent {agent} in {}", read.external_id),
                ));
            }
        }
        let segments = segments(&threads, newest(filed), read.sub_agent.is_none());
        read_from(&segments, read.cursor.as_deref(), read.tail)
    }

    /// Every rollout of thread `id`, or why there is none.
    fn family<'a>(&self, threads: &'a Threads, id: &str) -> Result<&'a [Filed], Error> {
        threads
            .by_id
            .get(id)
            .map(Vec::as_slice)
            .filter(|filed| !filed.is_empty())
            .ok_or_else(|| {
                Error::refused(
                    refusal::NOT_FOUND,
                    format!("Codex has no conversation {id} here"),
                )
            })
    }

    /// The newest rollout of a conversation, which a turn continues and a holder holds.
    pub fn current(&self, id: &str) -> Result<PathBuf, Error> {
        let threads = self.threads();
        let filed = self.family(&threads, id)?;
        if !equally_current(filed) {
            Ok(newest(filed).file.clone())
        } else {
            Err(Error::refused(
                refusal::REFUSED,
                format!("more than one rollout of {id} is equally current"),
            ))
        }
    }

    /// The conversation's rollouts, oldest first.
    pub fn transcripts(&self, id: &str) -> Result<Vec<PathBuf>, Error> {
        let threads = self.threads();
        let mut filed = self.family(&threads, id)?.to_vec();
        filed.sort_by_key(|filed| filed.modified);
        Ok(filed.into_iter().map(|filed| filed.file).collect())
    }

    /// Keep the rollouts Codex's moments named outside its own folders.
    fn remember(&self, named: &[PathBuf]) {
        let files = named
            .iter()
            .filter(|file| file.is_absolute() && is_rollout(file))
            .filter_map(|file| file.canonicalize().ok());
        self.named
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(files);
    }

    /// Every rollout here, read for its opening.
    fn threads(&self) -> Threads {
        let mut complete = true;
        let mut files = Vec::new();
        let sessions = self.root.join("sessions");
        walk(&sessions, 3, &mut files, &mut complete);
        // A rollout named outside the walk joins it unless the walk found it too; the walk's
        // own paths are resolved only then, which costs a lookup of every rollout here.
        let named = self.named.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !named.is_empty() {
            let own: BTreeSet<PathBuf> =
                files.iter().filter_map(|f| f.canonicalize().ok()).collect();
            files.extend(named.into_iter().filter(|file| !own.contains(file)));
        }
        let mut by_id: BTreeMap<String, Vec<Filed>> = BTreeMap::new();
        let mut kept = HashMap::new();
        for file in files {
            let Ok(meta) = std::fs::symlink_metadata(&file) else {
                complete = false;
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }
            let Some(named) = named_id(&file) else {
                continue;
            };
            let opening = self.opening(&file, &meta);
            let head = opening.head.clone();
            let id = head.id.clone().unwrap_or(named);
            let modified = meta
                .modified()
                .ok()
                .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |at| at.as_millis() as u64);
            kept.insert(
                file.clone(),
                Kept {
                    dev: meta.dev(),
                    ino: meta.ino(),
                    len: meta.len(),
                    opening,
                },
            );
            by_id.entry(id).or_default().push(Filed {
                file,
                modified,
                head,
            });
        }
        *self.heads.lock().unwrap_or_else(|e| e.into_inner()) = kept;
        Threads { by_id, complete }
    }

    /// A rollout's opening, read again only where the file could say more of it: Codex
    /// appends below the opening and never rewrites it, so a whole opening holds for the
    /// file it was read from, and one that is not whole holds while the file has not grown.
    /// Measured 2026-10-10: 767 sub-agent rollouts that never name what a person asked were
    /// read again on every call, 0.3–1 s each, and a machine's every sweep waited minutes.
    fn opening(&self, file: &Path, meta: &std::fs::Metadata) -> Opening {
        if let Some(kept) = self
            .heads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(file)
            && kept.dev == meta.dev()
            && kept.ino == meta.ino()
            && (kept.opening.whole || kept.len == meta.len())
        {
            return kept.opening.clone();
        }
        opening(file)
    }
}

/// Every `rollout-*.jsonl` `depth` folders below `dir`, a link never followed.
fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>, complete: &mut bool) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            *complete = false;
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            *complete = false;
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            *complete = false;
            continue;
        };
        let path = entry.path();
        if depth > 0 && kind.is_dir() {
            walk(&path, depth - 1, out, complete);
        } else if depth == 0 && kind.is_file() && is_rollout(&path) {
            out.push(path);
        }
    }
}

fn is_rollout(file: &Path) -> bool {
    file.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
}

/// The thread a rollout's name files it under: the UUID that ends it.
fn named_id(file: &Path) -> Option<String> {
    let stem = file.file_stem()?.to_str()?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    uuid::Uuid::parse_str(id).ok()?;
    Some(id.to_owned())
}

/// The rollout last written of a family.
fn newest(filed: &[Filed]) -> &Filed {
    filed
        .iter()
        .max_by_key(|filed| filed.modified)
        .expect("a family has a rollout")
}

/// Two rollouts of one thread written last at the same moment: neither is the current.
fn equally_current(filed: &[Filed]) -> bool {
    let newest = newest(filed).modified;
    filed.iter().filter(|f| f.modified == newest).count() > 1
}

/// A run nobody sits at: `codex exec`.
fn headless(head: &Head) -> bool {
    head.source.as_deref() == Some("exec")
}

/// A conversation as `sessions/list` says it; none where its current rollout is
/// ambiguous.
fn session(id: &str, filed: &[Filed]) -> Option<Session> {
    if equally_current(filed) {
        return None;
    }
    let mut ordered = filed.to_vec();
    ordered.sort_by_key(|filed| filed.modified);
    let first = ordered
        .iter()
        .find(|filed| !filed.head.continues())
        .unwrap_or(&ordered[0]);
    let last = ordered.last().expect("a family has a rollout");
    Some(Session {
        external_id: id.to_owned(),
        cwd: last.head.cwd.clone().or_else(|| first.head.cwd.clone()),
        title: ordered.iter().find_map(|filed| filed.head.asked.clone()),
        created_at: ordered
            .iter()
            .filter(|filed| !filed.head.continues())
            .filter_map(|filed| filed.head.born)
            .min(),
        updated_at: last.modified,
        transcripts: ordered.iter().map(|filed| filed.file.clone()).collect(),
        parent: first.head.parent().map(str::to_owned),
        app: first.head.originator.as_deref() == Some("Codex Desktop"),
        sub_agents: Vec::new(),
    })
}

/// A sub-agent of `conversation`, as its own rollout says.
fn sub_agent(id: &str, conversation: &str, filed: &[Filed]) -> SubAgent {
    let mut ordered = filed.to_vec();
    ordered.sort_by_key(|filed| filed.modified);
    let head = &ordered[0].head;
    SubAgent {
        external_id: id.to_owned(),
        lead: head
            .spawned_by
            .clone()
            .filter(|lead| lead != conversation && lead != id),
        name: head.nickname.clone(),
        task: head.asked.clone(),
        started_at: head.first_at.or(head.born),
        started_by: None,
        transcripts: ordered.into_iter().map(|filed| filed.file).collect(),
    }
}

/// What a rollout's opening says, and whether it is whole: it found what a person asked,
/// or read all [`OPENING`] records before the file ended.
fn opening(file: &Path) -> Opening {
    let mut found = Head::default();
    let Ok(open) = File::open(file) else {
        return Opening {
            head: found,
            whole: false,
        };
    };
    let mut reader = BufReader::new(open);
    let mut line = Vec::new();
    let mut whole = true;
    for n in 0..OPENING {
        match capped(&mut reader, &mut line, RECORD) {
            Ok(Line::Whole) => {}
            Ok(Line::Long) => continue,
            Ok(Line::Partial) | Err(_) => {
                whole = false;
                break;
            }
        }
        let Ok(record) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if n == 0 {
            found.first_at = stamped(&record);
        }
        let Some(payload) = record.get("payload") else {
            continue;
        };
        match text(&record, "type") {
            Some("session_meta") if found.id.is_none() && n == 0 => meta(payload, &mut found),
            Some("response_item")
                if text(payload, "type") == Some("message")
                    && text(payload, "role") == Some("user") =>
            {
                let blocks = payload
                    .get("content")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                if let Some((said, _)) = events::person_words(blocks)
                    && let Some(asked) = words::asked_for(&said)
                {
                    found.asked = Some(asked.chars().take(TASK).collect());
                    break;
                }
            }
            _ => {}
        }
    }
    Opening { head: found, whole }
}

fn meta(payload: &Value, found: &mut Head) {
    found.id = text(payload, "id")
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        .map(str::to_owned);
    found.session_id = text(payload, "session_id").map(str::to_owned);
    found.born = text(payload, "timestamp").and_then(events::millis);
    found.cwd = text(payload, "cwd")
        .map(PathBuf::from)
        .filter(|cwd| cwd.is_absolute());
    found.originator = text(payload, "originator").map(str::to_owned);
    found.source = text(payload, "source").map(str::to_owned);
    found.forked_from = text(payload, "forked_from_id")
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    found.base = payload.get("history_base").and_then(|base| {
        Some((
            text(base, "thread_id")?.to_owned(),
            base.get("end_byte_offset")?.as_u64()?,
        ))
    });
    let spawn = payload
        .pointer("/source/subagent/thread_spawn")
        .unwrap_or(&Value::Null);
    found.spawned_by = text(spawn, "parent_thread_id").map(str::to_owned);
    found.nickname = text(spawn, "agent_nickname")
        .or_else(|| text(payload, "agent_nickname"))
        .or_else(|| text(spawn, "agent_role"))
        .map(str::to_owned);
}

/// One stretch of a history: a file, read up to `end` where a later rollout took it only
/// so far, and the thread whose fork begins after it.
#[derive(Debug, Clone)]
struct Segment {
    file: PathBuf,
    end: Option<u64>,
    fork: Option<(String, u64)>,
}

/// The stretches a thread's history is read from, oldest first: the rollouts its
/// `history_base` names, each cut where it was taken, then its own. With `ancestry`
/// false a fork's parent is not read (a sub-agent's own work).
fn segments(threads: &Threads, current: &Filed, ancestry: bool) -> Vec<Segment> {
    let mut chain = vec![Segment {
        file: current.file.clone(),
        end: None,
        fork: None,
    }];
    let mut at = current;
    for _ in 0..DEPTH {
        let Some((thread, end)) = at.head.base.clone() else {
            break;
        };
        let own = at.head.id.clone();
        let forked = own.as_deref() != Some(thread.as_str());
        if forked && !ancestry {
            break;
        }
        let Some(base) = threads.by_id.get(&thread).and_then(|filed| {
            filed
                .iter()
                .filter(|filed| filed.file != at.file && filed.modified <= at.modified)
                .max_by_key(|filed| filed.modified)
                .or_else(|| filed.iter().find(|filed| filed.file != at.file))
        }) else {
            break;
        };
        if chain.iter().any(|segment| segment.file == base.file) {
            break;
        }
        chain.insert(
            0,
            Segment {
                file: base.file.clone(),
                end: Some(end),
                fork: forked.then(|| (thread.clone(), at.head.born.unwrap_or(0))),
            },
        );
        at = base;
    }
    // A fork whose parent's rollout is gone, or one named only by `forked_from_id`, says
    // so first: the history before it is not here.
    if ancestry && let Some(parent) = at.head.parent() {
        chain.insert(
            0,
            Segment {
                file: PathBuf::new(),
                end: Some(0),
                fork: Some((parent.to_owned(), at.head.born.unwrap_or(0))),
            },
        );
    }
    chain
}

/// Where a read stands: the stretch, the file it was by device and inode, the offset after
/// the last whole record read, and what the records so far carry on.
#[derive(Serialize, Deserialize)]
struct Cursor {
    v: u8,
    seg: usize,
    dev: u64,
    ino: u64,
    off: u64,
    #[serde(default)]
    st: State,
}

impl Cursor {
    fn parse(cursor: &str) -> Result<Self, Error> {
        let invalid = || Error::invalid(format!("`{cursor}` is no cursor this driver gave"));
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(cursor)
            .map_err(|_| invalid())?;
        let cursor: Self = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if cursor.v != 1 {
            return Err(invalid());
        }
        Ok(cursor)
    }

    fn said(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(self).expect("a cursor is JSON"))
    }
}

/// The events of a history after `cursor`, or within `tail` bytes of its end.
fn read_from(
    segments: &[Segment],
    cursor: Option<&str>,
    tail: Option<u64>,
) -> Result<Events, Error> {
    let given = cursor.map(Cursor::parse).transpose()?;
    let stat = |segment: &Segment| -> Option<(u64, u64, u64)> {
        if segment.file.as_os_str().is_empty() {
            return Some((0, 0, 0));
        }
        let meta = std::fs::metadata(&segment.file).ok()?;
        let len = segment.end.map_or(meta.len(), |end| end.min(meta.len()));
        Some((meta.dev(), meta.ino(), len))
    };
    let same = given.as_ref().filter(|cursor| {
        segments
            .get(cursor.seg)
            .and_then(stat)
            .is_some_and(|(dev, ino, len)| {
                dev == cursor.dev && ino == cursor.ino && cursor.off <= len
            })
    });
    let reset = given.is_some() && same.is_none();
    let (mut seg, mut off, mut state, whole) = match (same, tail) {
        (Some(cursor), _) => (cursor.seg, cursor.off, cursor.st.clone(), false),
        (None, Some(tail)) if !reset => {
            let last = segments.len() - 1;
            let (_, _, len) =
                stat(&segments[last]).ok_or_else(|| unreadable(&segments[last].file))?;
            (last, len.saturating_sub(tail), State::default(), false)
        }
        (None, _) => (0, 0, State::default(), true),
    };
    let from_tail = same.is_none() && !reset && tail.is_some() && off > 0;
    let mut events = Vec::new();
    let mut budget = BATCH;
    let mut more = false;
    let mut first = from_tail;
    loop {
        let segment = &segments[seg];
        let (dev, ino, len) = stat(segment).ok_or_else(|| unreadable(&segment.file))?;
        if off < len {
            let open = File::open(&segment.file).map_err(|_| unreadable(&segment.file))?;
            let (read, cut) =
                records(&open, off, len, budget, first).map_err(|_| unreadable(&segment.file))?;
            first = false;
            // Every stretch before a fork is the history it took from its parent.
            let copied = segments[seg..].iter().any(|later| later.fork.is_some());
            for (start, record) in &read.records {
                if let Some(record) = record {
                    let at = stamped(record).unwrap_or(0);
                    state
                        .read(
                            &Place {
                                segment: seg,
                                offset: *start,
                                at,
                                copied,
                                record,
                            },
                            &mut events,
                        )
                        .map_err(|line| {
                            Error::refused(
                                refusal::REFUSED,
                                format!("{}: {line}", segment.file.display()),
                            )
                        })?;
                }
            }
            budget = budget.saturating_sub(read.through - off);
            off = read.through;
            if cut || budget == 0 {
                more = off < len || seg + 1 < segments.len();
                return Ok(answer(
                    seg, dev, ino, off, state, events, more, reset, whole,
                ));
            }
            if off < len {
                // A record still being written ends this read.
                return Ok(answer(
                    seg, dev, ino, off, state, events, false, reset, whole,
                ));
            }
        }
        if seg + 1 == segments.len() {
            return Ok(answer(
                seg, dev, ino, off, state, events, more, reset, whole,
            ));
        }
        if let Some((parent, at)) = &segment.fork {
            events.push(Event::Fork {
                at: *at,
                parent: parent.clone(),
            });
            // What the parent ran on is not the fork's to keep saying.
            state = State::default();
        }
        seg += 1;
        off = 0;
    }
}

#[allow(clippy::too_many_arguments)]
fn answer(
    seg: usize,
    dev: u64,
    ino: u64,
    off: u64,
    st: State,
    events: Vec<Event>,
    more: bool,
    reset: bool,
    whole: bool,
) -> Events {
    Events {
        cursor: Cursor {
            v: 1,
            seg,
            dev,
            ino,
            off,
            st,
        }
        .said(),
        events,
        more,
        reset,
        whole,
    }
}

fn unreadable(file: &Path) -> Error {
    Error::refused(
        refusal::REFUSED,
        format!("{} cannot be read", file.display()),
    )
}

/// The whole records read, each with where it starts, and where the read stopped.
struct Records {
    records: Vec<(u64, Option<Value>)>,
    through: u64,
}

/// The whole records from `start` up to `length`, at most `budget` bytes of them, and
/// whether whole records remain past them. `align` skips to the first record that begins
/// after `start`, for a tail.
fn records(
    open: &File,
    start: u64,
    length: u64,
    budget: u64,
    align: bool,
) -> std::io::Result<(Records, bool)> {
    let mut file = open;
    let mut at = if align {
        first_whole(open, start, length)?
    } else {
        start
    };
    file.seek(SeekFrom::Start(at))?;
    let total = length - at;
    let mut reader = BufReader::with_capacity(1 << 16, file.take(total));
    let from = at;
    let mut records = Vec::new();
    let mut line = Vec::new();
    loop {
        if at - from >= budget {
            return Ok((
                Records {
                    records,
                    through: at,
                },
                at < length,
            ));
        }
        let kind = capped(&mut reader, &mut line, RECORD)?;
        let end = from + consumed(&reader, total);
        match kind {
            Line::Partial => {
                return Ok((
                    Records {
                        records,
                        through: at,
                    },
                    false,
                ));
            }
            Line::Long => records.push((at, None)),
            Line::Whole => records.push((at, serde_json::from_slice(&line).ok())),
        }
        at = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use steery_sdk::wire::harness::{Compactor, Voice};

    fn home() -> PathBuf {
        let home = std::env::temp_dir().join(format!("steery-codex-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(home.join("sessions/2026/09/28")).unwrap();
        home
    }

    fn write(file: &Path, records: &[Value], touched: std::time::SystemTime) {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let lines: String = records.iter().map(|r| format!("{r}\n")).collect();
        std::fs::write(file, lines).unwrap();
        File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_modified(touched)
            .unwrap();
    }

    fn meta(id: &str, session: &str, born: &str, extra: Value) -> Value {
        let mut payload =
            json!({"id": id, "session_id": session, "cwd": "/tmp/w", "timestamp": born});
        for (key, value) in extra.as_object().unwrap() {
            payload[key] = value.clone();
        }
        json!({"type": "session_meta", "timestamp": born, "payload": payload})
    }

    fn ago(seconds: u64) -> std::time::SystemTime {
        std::time::SystemTime::now() - std::time::Duration::from_secs(seconds)
    }

    /// Codex continuation shards report one conversation: its current rollout last, its
    /// birth the original's.
    #[test]
    fn codex_shards_report_one_conversation_with_original_birth() {
        let home = home();
        let native = "11111111-1111-4111-8111-111111111111";
        let suffix = "22222222-2222-4222-8222-222222222222";
        let dir = home.join("sessions/2026/09/28");
        let original = dir.join(format!("rollout-2026-09-28T10-00-00-{native}.jsonl"));
        write(
            &original,
            &[meta(native, native, "2026-09-20T10:00:00Z", json!({}))],
            ago(7200),
        );
        let latest = dir.join(format!(
            "rollout-2026-09-28T11-00-00-{native}_{suffix}.jsonl"
        ));
        write(
            &latest,
            &[meta(
                native,
                native,
                "2026-09-26T10:00:00Z",
                json!({"cwd": "/tmp/current", "history_base": {"thread_id": native, "end_byte_offset": 42}}),
            )],
            ago(60),
        );
        let store = Store::new(home.clone());
        let listed = store.list(List::default());
        assert!(listed.complete);
        let [session] = listed.sessions.as_slice() else {
            panic!("{listed:#?}")
        };
        assert_eq!(session.external_id, native);
        assert_eq!(session.cwd.as_deref(), Some(Path::new("/tmp/current")));
        assert_eq!(session.created_at, events::millis("2026-09-20T10:00:00Z"));
        assert_eq!(session.transcripts, vec![original, latest.clone()]);
        assert_eq!(store.current(native).unwrap(), latest);
        std::fs::remove_dir_all(home).unwrap();
    }

    /// Two equally current shards name the conversation ambiguously; others stand.
    #[test]
    fn equally_current_shards_leave_other_sessions_and_say_incomplete() {
        let home = home();
        let native = "11111111-1111-4111-8111-111111111111";
        let other = "33333333-3333-4333-8333-333333333333";
        let dir = home.join("sessions/2026/09/28");
        let when = ago(60);
        for name in [
            format!("rollout-2026-09-28T10-00-00-{native}.jsonl"),
            format!(
                "rollout-2026-09-28T11-00-00-{native}_22222222-2222-4222-8222-222222222222.jsonl"
            ),
        ] {
            write(
                &dir.join(name),
                &[meta(native, native, "2026-09-28T10:00:00Z", json!({}))],
                when,
            );
        }
        write(
            &dir.join(format!("rollout-2026-09-28T12-00-00-{other}.jsonl")),
            &[meta(other, other, "2026-09-28T12:00:00Z", json!({}))],
            when,
        );
        let listed = Store::new(home.clone()).list(List::default());
        assert!(!listed.complete);
        assert_eq!(
            listed
                .sessions
                .iter()
                .map(|s| s.external_id.as_str())
                .collect::<Vec<_>>(),
            [other]
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    /// Codex files a sub-agent beside its lead; its opening names its lead and its
    /// nickname, and the first thing it was asked is its task. A finished one started
    /// by another sub-agent names that one as its lead.
    #[test]
    fn sub_agents_are_their_lead_s_never_conversations() {
        let home = home();
        let dir = home.join("sessions/2026/09/26");
        let lead = "11111111-1111-4111-8111-111111111111";
        let turing = "22222222-2222-4222-8222-222222222222";
        let nested = "33333333-3333-4333-8333-333333333333";
        write(
            &dir.join(format!("rollout-2026-09-26T10-00-00-{lead}.jsonl")),
            &[meta(lead, lead, "2026-09-26T10:00:00Z", json!({}))],
            ago(600),
        );
        write(
            &dir.join(format!("rollout-2026-09-26T10-01-00-{turing}.jsonl")),
            &[
                meta(
                    turing,
                    lead,
                    "2026-09-26T10:01:00Z",
                    json!({"source": {"subagent": {"thread_spawn":
                {"parent_thread_id": lead, "depth": 1, "agent_nickname": "Turing"}}}}),
                ),
                json!({"timestamp": "2026-09-26T10:01:01Z", "type": "response_item", "payload": {"type": "message",
                "role": "user", "content": [{"type": "input_text", "text": "Survey the web dashboard\nwith pointers"}]}}),
            ],
            ago(300),
        );
        write(
            &dir.join(format!("rollout-2026-09-26T10-02-00-{nested}.jsonl")),
            &[meta(
                nested,
                lead,
                "2026-09-26T10:02:00Z",
                json!({"source": {"subagent": {"thread_spawn":
                {"parent_thread_id": turing, "depth": 2, "agent_nickname": "Noether"}}}}),
            )],
            ago(200),
        );
        let listed = Store::new(home.clone()).list(List::default());
        let [session] = listed.sessions.as_slice() else {
            panic!("{listed:#?}")
        };
        assert_eq!(session.external_id, lead);
        let agents = &session.sub_agents;
        assert_eq!(agents.len(), 2, "{agents:#?}");
        assert_eq!(agents[0].external_id, turing);
        assert_eq!(agents[0].name.as_deref(), Some("Turing"));
        assert_eq!(
            agents[0].lead, None,
            "started by the conversation's own agent"
        );
        assert_eq!(agents[0].task.as_deref(), Some("Survey the web dashboard"));
        assert_eq!(agents[1].external_id, nested);
        assert_eq!(agents[1].lead.as_deref(), Some(turing));
        assert_eq!(listed.excluded.len(), 2);
        std::fs::remove_dir_all(home).unwrap();
    }

    /// A conversation nobody named is titled by what its person first asked, past the
    /// harness's own preamble: by the same driver that listed it before they asked, once
    /// they have, though it keeps what it read of every rollout's opening between calls.
    #[test]
    fn a_conversation_nobody_named_says_what_was_asked_for() {
        let home = home();
        let id = "44444444-4444-4444-8444-444444444444";
        let file = home.join(format!(
            "sessions/2026/09/28/rollout-2026-09-28T10-00-00-{id}.jsonl"
        ));
        let opened = [
            meta(id, id, "2026-09-28T10:00:00Z", json!({})),
            json!({"timestamp": "2026-09-28T10:00:01Z", "type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "<environment_context>\n<cwd>/w</cwd>\n</environment_context>"}]}}),
        ];
        write(&file, &opened, ago(120));
        let store = Store::new(home.clone());
        let listed = store.list(List::default());
        assert_eq!(listed.sessions[0].title, None, "nothing asked yet");
        let asked = json!({"timestamp": "2026-09-28T10:00:02Z", "type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "Fix the flaky login test"}]}});
        write(
            &file,
            &[opened[0].clone(), opened[1].clone(), asked],
            ago(60),
        );
        let listed = store.list(List::default());
        assert_eq!(
            listed.sessions[0].title.as_deref(),
            Some("Fix the flaky login test")
        );
        std::fs::remove_dir_all(home).unwrap();
    }

    /// A fork reads its parent's history up to where it was taken, then says the fork,
    /// then its own; a compaction in what it took was its parent's. A cursor goes on where
    /// the last read stopped, and a tail reads only the newest records.
    #[test]
    fn a_fork_reads_its_parent_s_history_first_and_a_cursor_goes_on() {
        let home = home();
        let dir = home.join("sessions/2026/10/09");
        let parent = "01a12008-a20f-7be0-9c7e-335310b171a6";
        let fork = "01a12009-72d9-7650-8f6d-7e0af99af9d8";
        let said = |at: &str, text: &str| {
            json!({"timestamp": at, "type": "response_item", "payload":
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]}})
        };
        // Codex's `compacted` as it wrote it from 2026-09 on, its history left out.
        let compacted = |at: &str, window: &str, total: u64| {
            json!({"timestamp": at, "type": "compacted", "payload": {"message": "",
                "replacement_history": [], "compaction_response_id": "msg_011CfEqmkBnmXf3CgQfUFzUj",
                "latest_token_usage_record": {"thread_id": parent, "turn_id": "01a0bf31-a654",
                    "response_id": "msg_011CfEqmkBnmXf3CgQfUFzUj",
                    "usage": {"input_tokens": total - 1085, "cached_input_tokens": 13782,
                        "cache_write_input_tokens": 0, "output_tokens": 1085,
                        "reasoning_output_tokens": 148, "total_tokens": total}},
                "window_id": window, "first_window_id": "01a12008-w0", "previous_window_id": "01a12008-w0",
                "window_number": 1}})
        };
        let parent_file = dir.join(format!("rollout-2026-10-09T09-39-59-{parent}.jsonl"));
        let taken = [
            meta(parent, parent, "2026-10-09T09:39:59Z", json!({})),
            said("2026-10-09T09:40:00Z", "first"),
            compacted("2026-10-09T09:40:30Z", "01a12008-w1", 33_052),
        ];
        write(
            &parent_file,
            &[
                taken[0].clone(),
                taken[1].clone(),
                taken[2].clone(),
                said("2026-10-09T09:41:00Z", "after the fork"),
            ],
            ago(30),
        );
        let end: usize = taken.iter().map(|r| r.to_string().len() + 1).sum();
        let fork_file = dir.join(format!("rollout-2026-10-09T09-40-52-{fork}.jsonl"));
        let own = [
            meta(
                fork,
                fork,
                "2026-10-09T09:40:52Z",
                json!({"forked_from_id": parent,
            "history_base": {"thread_id": parent, "end_byte_offset": end}}),
            ),
            said("2026-10-09T09:40:53Z", "again"),
            // As Codex wrote it until 2026-07: no window, no tokens.
            json!({"timestamp": "2026-10-09T09:40:54Z", "type": "compacted",
                "payload": {"message": "", "replacement_history": [],
                    "compaction_response_id": null, "latest_token_usage_record": null}}),
            compacted("2026-10-09T09:40:55Z", "01a12009-w2", 412_647),
        ];
        let unnamed: usize = own[..2].iter().map(|r| r.to_string().len() + 1).sum();
        write(&fork_file, &own, ago(20));
        let store = Store::new(home.clone());
        let listed = store.list(List::default());
        let session = listed
            .sessions
            .iter()
            .find(|s| s.external_id == fork)
            .unwrap();
        assert_eq!(session.parent.as_deref(), Some(parent));
        assert_eq!(session.transcripts, vec![fork_file.clone()]);

        let read = store
            .read(Read {
                external_id: fork.into(),
                sub_agent: None,
                cursor: None,
                tail: None,
            })
            .unwrap();
        assert!(read.whole && !read.more && !read.reset);
        let shape: Vec<String> = read
            .events
            .iter()
            .map(|event| match event {
                Event::Words {
                    voice: Voice::Person,
                    text,
                    ..
                } => text.clone(),
                Event::Fork { parent, .. } => format!("fork {parent}"),
                Event::Compacted {
                    id,
                    by: Compactor::Harness { reason: None },
                    tokens_before,
                    tokens_after: None,
                    ..
                } => format!("compacted {id} {tokens_before:?}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "first".to_owned(),
                format!("fork {parent}"),
                "again".to_owned(),
                format!("compacted 1:{unnamed} None"),
                "compacted 01a12009-w2 Some(412647)".to_owned(),
            ]
        );

        let mut more = std::fs::OpenOptions::new()
            .append(true)
            .open(&fork_file)
            .unwrap();
        use std::io::Write as _;
        writeln!(more, "{}", said("2026-10-09T09:42:00Z", "later")).unwrap();
        write!(more, "{{\"partial\":").unwrap();
        let next = store
            .read(Read {
                external_id: fork.into(),
                sub_agent: None,
                cursor: Some(read.cursor.clone()),
                tail: None,
            })
            .unwrap();
        assert!(!next.whole && !next.reset);
        assert!(matches!(next.events.as_slice(), [Event::Words { text, .. }] if text == "later"));
        let again = store
            .read(Read {
                external_id: fork.into(),
                sub_agent: None,
                cursor: Some(next.cursor.clone()),
                tail: None,
            })
            .unwrap();
        assert!(
            again.events.is_empty(),
            "a record still being written waits"
        );

        let tail = store
            .read(Read {
                external_id: fork.into(),
                sub_agent: None,
                cursor: None,
                tail: Some(40),
            })
            .unwrap();
        assert!(!tail.whole);
        assert!(
            tail.events
                .iter()
                .all(|e| !matches!(e, Event::Words { text, .. } if text == "first"))
        );

        // A rollout replaced under the cursor starts again.
        std::fs::remove_file(&fork_file).unwrap();
        write(
            &fork_file,
            &[meta(fork, fork, "2026-10-09T09:40:52Z", json!({}))],
            ago(1),
        );
        let reset = store
            .read(Read {
                external_id: fork.into(),
                sub_agent: None,
                cursor: Some(next.cursor),
                tail: None,
            })
            .unwrap();
        assert!(reset.reset && reset.whole);
        std::fs::remove_dir_all(home).unwrap();
    }
}
