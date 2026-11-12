//! Qwen's conversations: a chat file for each, `<base>/projects/<project>/chats/<id>.jsonl`,
//! beside its title, `<id>.title.json`. `<project>` is the folder Qwen ran in with each
//! character that is not a letter or a digit made `-`; the same folder can be named two
//! ways (`/tmp/x` and `/private/tmp/x`), so a conversation's folder is the `cwd` its
//! records carry. A sub-agent's run is `<project>/subagents/<lead id>/agent-<its id>.jsonl`,
//! beside `agent-<its id>.meta.json`, which says what started it.
//!
//! Every conversation a person prompted is listed: Qwen keeps no mark of which ran in a
//! terminal and which under `-p` or `--acp`.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use memchr::memmem;
use serde_json::Value;
use steery_driver_common::follow::Followed;
use steery_driver_common::lines::RECORD;
use steery_driver_common::tree::Trees;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use super::events;

const PROJECTS: &str = "projects";
const CHATS: &str = "chats";
const SUBAGENTS: &str = "subagents";
const AGENT: &str = "agent-";
/// How many characters of the first prompt a title keeps.
const TITLE: usize = 100;
/// The largest title file read.
const TITLE_FILE: u64 = 64 << 10;

/// Qwen's conversations under its runtime folder, and what was read of each file.
pub struct Store {
    projects: PathBuf,
    scans: Mutex<HashMap<PathBuf, Scan>>,
    trees: Trees<events::Records>,
}

/// What one chat file says of its conversation, as far as it was read.
#[derive(Clone, Default)]
struct Head {
    started: Option<u64>,
    cwd: Option<PathBuf>,
    /// The first line of the first prompt that a person wrote.
    prompt: Option<String>,
}

/// One chat file, read on from where it stopped.
#[derive(Default)]
struct Scan {
    followed: Followed,
    head: Head,
}

/// One conversation: its chat file, the project folder that holds it, and what it says.
struct Conversation {
    id: String,
    file: PathBuf,
    project: PathBuf,
    head: Head,
    changed: Option<u64>,
}

struct Index {
    by_id: BTreeMap<String, Conversation>,
    complete: bool,
}

impl Store {
    pub fn new(base: &Path) -> Self {
        Self {
            projects: base.join(PROJECTS),
            scans: Mutex::default(),
            trees: Trees::default(),
        }
    }

    fn index(&self) -> Index {
        let mut index = Index {
            by_id: BTreeMap::new(),
            complete: true,
        };
        // A file that is gone is forgotten.
        lock(&self.scans).retain(|file, _| file.exists());
        let projects = std::fs::read_dir(&self.projects)
            .into_iter()
            .flatten()
            .flatten();
        for project in projects.map(|entry| entry.path()) {
            let Ok(entries) = std::fs::read_dir(project.join(CHATS)) else {
                continue;
            };
            for file in entries.flatten().map(|entry| entry.path()) {
                let Some(id) = chat_id(&file) else {
                    continue;
                };
                let Some(head) = self.head(&file) else {
                    index.complete = false;
                    continue;
                };
                if head.prompt.is_none() {
                    continue;
                }
                let changed = events::changed(&file);
                if let Some(other) = index.by_id.get(&id) {
                    // One id in two project folders: the listing is unsure which it is.
                    index.complete = false;
                    if other.changed >= changed {
                        continue;
                    }
                }
                index.by_id.insert(
                    id.clone(),
                    Conversation {
                        id,
                        file,
                        project: project.clone(),
                        head,
                        changed,
                    },
                );
            }
        }
        index
    }

    /// What `file` says of its conversation, read on from where the last read stopped.
    fn head(&self, file: &Path) -> Option<Head> {
        let open = File::open(file).ok()?;
        let mut scans = lock(&self.scans);
        let scan = scans.entry(file.to_owned()).or_default();
        scan.read_on(&open)?;
        Some(scan.head.clone())
    }

    /// `sessions/list`.
    pub fn list(&self, list: List) -> Listed {
        let index = self.index();
        let mut sessions: Vec<Session> = index
            .by_id
            .values()
            .map(|conversation| self.session(conversation))
            .filter(|session| {
                list.seen_since
                    .is_none_or(|since| session.updated_at >= since)
            })
            .collect();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        Listed {
            sessions,
            excluded: Vec::new(),
            complete: index.complete,
            cursor: None,
            after: None,
            gone: Vec::new(),
        }
    }

    /// `sessions/locate`.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let index = self.index();
        Ok(Located {
            session: self.session(index.conversation(&locate.external_id)?),
        })
    }

    /// `sessions/read`.
    pub fn read(&self, read: Read) -> Result<Events, Error> {
        let index = self.index();
        let conversation = index.conversation(&read.external_id)?;
        let file = match &read.sub_agent {
            None => conversation.file.clone(),
            Some(child) => self
                .sub_agents(conversation)
                .into_iter()
                .find(|agent| &agent.external_id == child)
                .and_then(|agent| agent.transcripts.into_iter().next())
                .ok_or_else(|| not_found(child))?,
        };
        events::read(&self.trees, &file, read.cursor.as_deref(), read.tail)
    }

    fn session(&self, conversation: &Conversation) -> Session {
        let head = &conversation.head;
        let title = title_file(&conversation.file)
            .map(|title| self::title(&title))
            .or_else(|| head.prompt.clone());
        Session {
            external_id: conversation.id.clone(),
            cwd: head.cwd.clone(),
            title,
            created_at: head.started,
            updated_at: conversation.changed.max(head.started).unwrap_or_default(),
            transcripts: vec![conversation.file.clone()],
            parent: None,
            app: false,
            sub_agents: self.sub_agents(conversation),
        }
    }

    /// The sub-agents of a conversation: each `agent-<id>.jsonl` in its folder under
    /// `subagents/`, with what its `agent-<id>.meta.json` says of it. The folder is the
    /// conversation's id with each character that is not a letter, a digit, `-` or `_`
    /// made `_` (`getSubagentSessionDir`). The records' shape is Qwen 0.27.2's source's:
    /// no sub-agent run was recorded yet.
    fn sub_agents(&self, conversation: &Conversation) -> Vec<SubAgent> {
        let folder = conversation
            .project
            .join(SUBAGENTS)
            .join(folder_name(&conversation.id));
        let mut found: Vec<SubAgent> = std::fs::read_dir(&folder)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let file = entry.path();
                let id = file
                    .file_name()?
                    .to_str()?
                    .strip_prefix(AGENT)?
                    .strip_suffix(".jsonl")?
                    .to_owned();
                let meta = std::fs::read(folder.join(format!("{AGENT}{id}.meta.json")))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .unwrap_or(Value::Null);
                let text = |key: &str| {
                    meta[key]
                        .as_str()
                        .filter(|text| !text.trim().is_empty())
                        .map(str::to_owned)
                };
                let head = self.head(&file).unwrap_or_default();
                Some(SubAgent {
                    lead: text("parentAgentId"),
                    name: text("agentType"),
                    task: text("description").map(|task| title(&task)).or(head.prompt),
                    started_at: text("createdAt")
                        .as_deref()
                        .and_then(events::millis)
                        .or(head.started),
                    started_by: text("toolUseId"),
                    transcripts: vec![file],
                    external_id: text("agentId").unwrap_or(id),
                })
            })
            .collect();
        found.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.external_id.cmp(&b.external_id))
        });
        found
    }
}

impl Index {
    /// The conversation `id` names.
    fn conversation(&self, id: &str) -> Result<&Conversation, Error> {
        self.by_id.get(id).ok_or_else(|| not_found(id))
    }
}

impl Scan {
    /// Reads on from where the last read stopped. A line is parsed only where it can change
    /// the listing, and a line longer than [`RECORD`] never is.
    fn read_on(&mut self, open: &File) -> Option<()> {
        if self.followed.restart(open).ok()? {
            self.head = Head::default();
        }
        let head = &mut self.head;
        self.followed
            .read_on(open, |line, _, _| {
                if line.len() > RECORD {
                    return true;
                }
                let first = head.started.is_none();
                if (first || (head.prompt.is_none() && memmem::find(line, b"real_user").is_some()))
                    && let Ok(record) = serde_json::from_slice::<Value>(line)
                {
                    head.record(&record);
                }
                true
            })
            .ok()
    }
}

impl Head {
    fn record(&mut self, record: &Value) {
        if self.started.is_none() {
            self.started = record["timestamp"].as_str().and_then(events::millis);
        }
        if self.cwd.is_none() {
            self.cwd = record["cwd"]
                .as_str()
                .map(PathBuf::from)
                .filter(|cwd| cwd.is_absolute());
        }
        if self.prompt.is_none() && record["type"] == "user" && record["provenance"] == "real_user"
        {
            let said: String = record["message"]["parts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|part| part["thought"] != true)
                .filter_map(|part| part["text"].as_str())
                .collect();
            self.prompt = said.lines().find(|line| !line.trim().is_empty()).map(title);
        }
    }
}

/// The conversation id of a chat file, `<id>.jsonl`.
fn chat_id(path: &Path) -> Option<String> {
    let id = path.file_name()?.to_str()?.strip_suffix(".jsonl")?;
    (!id.is_empty() && path.is_file()).then(|| id.to_owned())
}

/// The title Qwen keeps beside a chat file, `<id>.title.json` `displayName`.
fn title_file(chat: &Path) -> Option<String> {
    let file = chat.with_extension("title.json");
    let open = File::open(file).ok()?;
    if open.metadata().ok()?.len() > TITLE_FILE {
        return None;
    }
    let title: Value = serde_json::from_reader(open).ok()?;
    title["displayName"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .map(str::to_owned)
}

/// A sub-agents folder's name for a conversation's id.
fn folder_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A title, at most [`TITLE`] characters of `text`'s first line.
fn title(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    match line.char_indices().nth(TITLE) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_owned(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn not_found(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("Qwen Code keeps no conversation `{id}` that a person prompted here"),
    )
}
