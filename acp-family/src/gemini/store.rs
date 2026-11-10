//! Gemini's conversations: a chat file for each under `<root>/tmp/<project>/chats/`,
//! `session-<time>-<id8>.jsonl` (an older release wrote `session-<time>-<id8>.json`, one
//! JSON document, which Gemini moves into a `.jsonl` beside it once the conversation goes
//! on). A sub-agent's run is `chats/<lead id>/<its id>.jsonl`. `<project>/.project_root`
//! names the folder the conversations ran in.
//!
//! The chat file does not say whether a person was at it. Gemini's terminal writes each
//! prompt that a person typed to `<project>/logs.json`, and a run with `-p` or `--acp`
//! does not: a conversation whose id is there is listed, and one that is not is excluded.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use memchr::memmem;
use serde_json::Value;
use steery_driver_common::follow::Followed;
use steery_driver_common::lines::RECORD;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use super::events;

const CHATS: &str = "chats";
const PREFIX: &str = "session-";
const PROJECT_ROOT: &str = ".project_root";
const LOGS: &str = "logs.json";
/// How many characters of the first prompt a title keeps.
const TITLE: usize = 100;
/// How deep sub-agents that start sub-agents are followed.
const DEPTH: usize = 4;

/// Gemini's conversations under its root, and what was read of each file.
pub struct Store {
    tmp: PathBuf,
    scans: Mutex<HashMap<PathBuf, Scan>>,
    logs: Mutex<HashMap<PathBuf, Logged>>,
}

/// What one chat file says of its conversation, as far as it was read.
#[derive(Clone, Default)]
struct Head {
    id: Option<String>,
    started: Option<u64>,
    updated: Option<u64>,
    summary: Option<String>,
    /// The first line of the first prompt that a person wrote.
    prompt: Option<String>,
    /// A person wrote a prompt in it.
    person: bool,
    /// The sub-agents that its `invoke_agent` calls started, by the sub-agent's id.
    started_agents: BTreeMap<String, Started>,
}

/// The call that started a sub-agent.
#[derive(Clone)]
struct Started {
    call: String,
    name: Option<String>,
    task: Option<String>,
}

/// One file as far as it was read: a `.jsonl` read on from where it stopped, a `.json`
/// read whole again only when it changed.
#[derive(Default)]
struct Scan {
    followed: Followed,
    /// The device, inode and length of a `.json` file when it was read.
    whole: Option<(u64, u64, u64)>,
    head: Head,
}

/// The ids in one `logs.json`, and the file as it was when they were read.
struct Logged {
    file: (u64, u64, u64),
    ids: BTreeSet<String>,
}

/// One project folder's conversations.
struct Project {
    cwd: Option<PathBuf>,
    /// The ids that a person typed into, or none where `logs.json` did not read whole.
    typed: Option<BTreeSet<String>>,
    chats: PathBuf,
}

/// One conversation: its files oldest first, and what the newest says.
struct Conversation {
    id: String,
    files: Vec<PathBuf>,
    head: Head,
    changed: Option<u64>,
    project: usize,
}

struct Index {
    projects: Vec<Project>,
    by_id: BTreeMap<String, Conversation>,
    complete: bool,
}

impl Store {
    pub fn new(root: &Path) -> Self {
        Self {
            tmp: root.join("tmp"),
            scans: Mutex::default(),
            logs: Mutex::default(),
        }
    }

    fn index(&self) -> Index {
        let mut index = Index {
            projects: Vec::new(),
            by_id: BTreeMap::new(),
            complete: true,
        };
        // A file that is gone is forgotten.
        lock(&self.scans).retain(|file, _| file.exists());
        let folders = std::fs::read_dir(&self.tmp).into_iter().flatten().flatten();
        for folder in folders
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
        {
            let chats = folder.join(CHATS);
            let Ok(entries) = std::fs::read_dir(&chats) else {
                continue;
            };
            let typed = self.typed(&folder.join(LOGS));
            index.complete &= typed.is_some();
            index.projects.push(Project {
                cwd: std::fs::read_to_string(folder.join(PROJECT_ROOT))
                    .ok()
                    .map(|root| PathBuf::from(root.trim()))
                    .filter(|root| root.is_absolute()),
                typed,
                chats,
            });
            let project = index.projects.len() - 1;
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| chat_file(path))
                .collect();
            // Oldest first: a `.json` before the `.jsonl` that Gemini moved it into.
            files.sort();
            for file in files {
                let Some(head) = self.head(&file) else {
                    index.complete = false;
                    continue;
                };
                let Some(id) = head.id.clone() else {
                    continue;
                };
                let changed = events::changed(&file);
                let conversation = index
                    .by_id
                    .entry(id.clone())
                    .or_insert_with(|| Conversation {
                        id,
                        files: Vec::new(),
                        head: Head::default(),
                        changed: None,
                        project,
                    });
                if conversation.project != project {
                    // One id in two project folders: the listing is unsure which it is.
                    index.complete = false;
                }
                conversation.files.push(file);
                if changed >= conversation.changed {
                    conversation.changed = changed;
                    let person = conversation.head.person;
                    conversation.head = head;
                    conversation.head.person |= person;
                }
            }
        }
        index
    }

    /// What `file` says of its conversation, read on from where the last read stopped.
    fn head(&self, file: &Path) -> Option<Head> {
        let open = File::open(file).ok()?;
        let mut scans = lock(&self.scans);
        let scan = scans.entry(file.to_owned()).or_default();
        if file
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            scan.read_whole(&open)?;
        } else {
            scan.read_on(&open)?;
        }
        Some(scan.head.clone())
    }

    /// The ids in `logs`, read again only when it changed; none where it is too long or
    /// does not read.
    fn typed(&self, logs: &Path) -> Option<BTreeSet<String>> {
        let Ok(meta) = std::fs::metadata(logs) else {
            return Some(BTreeSet::new());
        };
        let file = file_identity(&meta);
        let mut kept = lock(&self.logs);
        if let Some(logged) = kept.get(logs).filter(|logged| logged.file == file) {
            return Some(logged.ids.clone());
        }
        if meta.len() > RECORD as u64 {
            return None;
        }
        let entries: Value = serde_json::from_slice(&std::fs::read(logs).ok()?).ok()?;
        let ids: BTreeSet<String> = entries
            .as_array()?
            .iter()
            .filter_map(|entry| entry["sessionId"].as_str().map(str::to_owned))
            .collect();
        kept.insert(
            logs.to_owned(),
            Logged {
                file,
                ids: ids.clone(),
            },
        );
        Some(ids)
    }

    /// `sessions/list`.
    pub fn list(&self, list: List) -> Listed {
        let index = self.index();
        let mut sessions = Vec::new();
        let mut excluded = Vec::new();
        for conversation in index.by_id.values() {
            if !conversation.head.person {
                continue;
            }
            if index.typed(conversation) {
                sessions.push(self.session(&index, conversation));
            } else {
                excluded.push(conversation.id.clone());
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
            complete: index.complete,
            cursor: None,
            after: None,
            gone: Vec::new(),
        }
    }

    /// `sessions/locate`.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let index = self.index();
        let conversation = index.conversation(&locate.external_id)?;
        Ok(Located {
            session: self.session(&index, conversation),
        })
    }

    /// `sessions/read`.
    pub fn read(&self, read: Read) -> Result<Events, Error> {
        let index = self.index();
        let conversation = index.conversation(&read.external_id)?;
        let file = match &read.sub_agent {
            None => conversation
                .files
                .last()
                .cloned()
                .ok_or_else(|| not_found(&read.external_id))?,
            Some(child) => self
                .sub_agents(&index, conversation)
                .into_iter()
                .find(|agent| &agent.external_id == child)
                .and_then(|agent| agent.transcripts.into_iter().next())
                .ok_or_else(|| not_found(child))?,
        };
        events::read(&file, read.cursor.as_deref(), read.tail)
    }

    fn session(&self, index: &Index, conversation: &Conversation) -> Session {
        let head = &conversation.head;
        let updated_at = [head.updated, conversation.changed, head.started]
            .into_iter()
            .flatten()
            .max()
            .unwrap_or_default();
        Session {
            external_id: conversation.id.clone(),
            cwd: index.projects[conversation.project].cwd.clone(),
            title: head.summary.clone().or_else(|| head.prompt.clone()),
            created_at: head.started,
            updated_at,
            transcripts: conversation.files.clone(),
            parent: None,
            app: false,
            sub_agents: self.sub_agents(index, conversation),
        }
    }

    /// The sub-agents of a conversation: each file in `chats/<its id>/`, and in turn the
    /// folders of those sub-agents, named by the calls that started them.
    fn sub_agents(&self, index: &Index, conversation: &Conversation) -> Vec<SubAgent> {
        let chats = &index.projects[conversation.project].chats;
        let mut found = Vec::new();
        let mut leads = vec![(None, conversation.id.clone(), conversation.head.clone())];
        for _ in 0..DEPTH {
            let mut next = Vec::new();
            for (lead, lead_id, lead_head) in leads {
                let files = std::fs::read_dir(chats.join(&lead_id))
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| {
                        path.extension()
                            .is_some_and(|extension| extension == "jsonl")
                    });
                for file in files {
                    let Some(head) = self.head(&file) else {
                        continue;
                    };
                    let Some(id) = head.id.clone().or_else(|| {
                        file.file_stem()
                            .and_then(|stem| stem.to_str())
                            .map(str::to_owned)
                    }) else {
                        continue;
                    };
                    let started = lead_head.started_agents.get(&id);
                    found.push(SubAgent {
                        external_id: id.clone(),
                        lead: lead.clone(),
                        name: started.and_then(|started| started.name.clone()),
                        task: started
                            .and_then(|started| started.task.clone())
                            .or_else(|| head.prompt.clone()),
                        started_at: head.started,
                        started_by: started.map(|started| started.call.clone()),
                        transcripts: vec![file],
                    });
                    next.push((Some(id.clone()), id, head));
                }
            }
            if next.is_empty() {
                break;
            }
            leads = next;
        }
        found.sort_by(|a, b| {
            a.started_at
                .cmp(&b.started_at)
                .then_with(|| a.external_id.cmp(&b.external_id))
        });
        found
    }
}

impl Index {
    /// A person typed into the conversation.
    fn typed(&self, conversation: &Conversation) -> bool {
        self.projects[conversation.project]
            .typed
            .as_ref()
            .is_some_and(|typed| typed.contains(&conversation.id))
    }

    /// The conversation `id` names: one that a person typed into.
    fn conversation(&self, id: &str) -> Result<&Conversation, Error> {
        self.by_id
            .get(id)
            .filter(|conversation| conversation.head.person && self.typed(conversation))
            .ok_or_else(|| not_found(id))
    }
}

impl Scan {
    /// Reads on in a `.jsonl` file from where the last read stopped. A line is parsed only
    /// where it can change the listing, and a line longer than [`RECORD`] never is.
    fn read_on(&mut self, open: &File) -> Option<()> {
        if self.followed.restart(open).ok()? {
            self.head = Head::default();
        }
        let head = &mut self.head;
        self.followed
            .read_on(open, |line, start, _| {
                if line.len() > RECORD {
                    return true;
                }
                if start == 0 {
                    if let Ok(meta) = serde_json::from_slice::<Value>(line) {
                        head.metadata(&meta);
                    }
                } else if interesting(line, head.person)
                    && let Ok(record) = serde_json::from_slice::<Value>(line)
                {
                    head.record(&record);
                }
                true
            })
            .ok()
    }

    /// Reads a whole `.json` file again where it changed.
    fn read_whole(&mut self, open: &File) -> Option<()> {
        let meta = open.metadata().ok()?;
        let file = file_identity(&meta);
        if self.whole == Some(file) {
            return Some(());
        }
        if meta.len() > RECORD as u64 {
            return None;
        }
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut &*open, &mut bytes).ok()?;
        let document: Value = serde_json::from_slice(&bytes).ok()?;
        let mut head = Head::default();
        head.metadata(&document);
        for message in document["messages"].as_array().into_iter().flatten() {
            head.record(message);
        }
        self.head = head;
        self.whole = Some(file);
        Some(())
    }
}

impl Head {
    fn metadata(&mut self, meta: &Value) {
        let text = |key: &str| meta[key].as_str().filter(|text| !text.trim().is_empty());
        self.id = text("sessionId").map(str::to_owned);
        self.started = text("startTime").and_then(events::millis);
        self.updated = text("lastUpdated").and_then(events::millis);
        self.summary = text("summary").map(title);
    }

    fn record(&mut self, record: &Value) {
        if let Some(set) = record.get("$set") {
            if let Some(id) = set["sessionId"].as_str() {
                self.id = Some(id.to_owned());
            }
            if let Some(updated) = set["lastUpdated"].as_str().and_then(events::millis) {
                self.updated = Some(updated);
            }
            if let Some(summary) = set["summary"]
                .as_str()
                .filter(|text| !text.trim().is_empty())
            {
                self.summary = Some(title(summary));
            }
            return;
        }
        match record["type"].as_str() {
            Some("user") if !self.person => {
                if let Some((said, _)) = events::person(record) {
                    self.person = true;
                    self.prompt = said.lines().find(|line| !line.trim().is_empty()).map(title);
                }
            }
            Some("gemini") => {
                for call in record["toolCalls"].as_array().into_iter().flatten() {
                    if call["name"] != "invoke_agent" {
                        continue;
                    }
                    let (Some(agent), Some(id)) = (call["agentId"].as_str(), call["id"].as_str())
                    else {
                        continue;
                    };
                    self.started_agents.insert(
                        agent.to_owned(),
                        Started {
                            call: id.to_owned(),
                            name: call["args"]["agent_name"].as_str().map(str::to_owned),
                            task: call["args"]["prompt"]
                                .as_str()
                                .and_then(|prompt| {
                                    prompt.lines().find(|line| !line.trim().is_empty())
                                })
                                .map(title),
                        },
                    );
                }
            }
            _ => {}
        }
    }
}

/// A line that can change what the listing says: metadata, the first person prompt, or a
/// call that started a sub-agent. Others are not parsed.
fn interesting(line: &[u8], person: bool) -> bool {
    memmem::find(line, b"\"$set\"").is_some()
        || memmem::find(line, b"\"invoke_agent\"").is_some()
        || (!person && memmem::find(line, b"\"type\":\"user\"").is_some())
}

/// A chat file of a conversation, not of a sub-agent.
fn chat_file(path: &Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with(PREFIX) && (name.ends_with(".jsonl") || name.ends_with(".json"))
            })
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

fn file_identity(meta: &std::fs::Metadata) -> (u64, u64, u64) {
    use std::os::unix::fs::MetadataExt as _;
    (meta.dev(), meta.ino(), meta.len())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn not_found(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("Gemini CLI keeps no conversation `{id}` that a person typed into here"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use steery_sdk::wire::harness::{Event, Execution, Voice};

    /// The corpus's conversations, copied to a folder of the test's own, each file older
    /// than [`events::SETTLE`] as a finished run leaves it.
    fn root(name: &str) -> PathBuf {
        let to = std::env::temp_dir().join(format!("steery-gemini-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&to);
        let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/gemini/root/.gemini");
        let old = SystemTime::now() - std::time::Duration::from_secs(3600);
        copy(&from, &to, old);
        to
    }

    fn copy(from: &Path, to: &Path, at: SystemTime) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let target = to.join(entry.file_name());
            if entry.path().is_dir() {
                copy(&entry.path(), &target, at);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
                File::options()
                    .write(true)
                    .open(&target)
                    .unwrap()
                    .set_modified(at)
                    .unwrap();
            }
        }
    }

    fn read(store: &Store, id: &str, sub_agent: Option<&str>, cursor: Option<String>) -> Events {
        store
            .read(Read {
                external_id: id.into(),
                sub_agent: sub_agent.map(str::to_owned),
                cursor,
                tail: None,
            })
            .unwrap()
    }

    /// Each turn's end, and each rollback as `rollback <turns>`.
    fn turns(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::TurnEnded { state, .. } => Some(format!("{state:?}")),
                Event::Rollback { turns, .. } => Some(format!("rollback {turns}")),
                Event::Compacted { .. } => Some("compacted".into()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn conversations_list_and_read_as_gemini_wrote_them() {
        let root = root("read");
        let store = Store::new(&root);

        // Only conversations that a person typed into are listed; a headless or ACP run
        // leaves no prompt in `logs.json`.
        let listed = store.list(List::default());
        assert!(listed.complete);
        let mut ids: Vec<_> = listed
            .sessions
            .iter()
            .map(|s| s.external_id.as_str())
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            [
                "488d4dbe-87e4-4026-aa02-5905bc8790fb",
                "760907b6-38c2-467e-8585-e4c4458da883",
                "b02d7ce8-e463-40aa-b7f6-33498bbdcb74",
                "df636b9c-090c-436e-a92e-7e8798e4b943",
            ]
        );
        let lead = &listed
            .sessions
            .iter()
            .find(|s| s.external_id.starts_with("488d"))
            .unwrap();
        assert_eq!(lead.title.as_deref(), Some("use a sub-agent to list files"));
        assert!(
            lead.cwd
                .as_deref()
                .is_some_and(|cwd| cwd.ends_with("subagent/work"))
        );
        let [child] = lead.sub_agents.as_slice() else {
            panic!("one sub-agent: {:?}", lead.sub_agents);
        };
        assert_eq!(child.external_id, "7480c29e-d0d0-4421-bada-2db4dd0aa087");
        assert_eq!(child.name.as_deref(), Some("generalist"));
        assert_eq!(
            child.started_by.as_deref(),
            Some("invoke_agent__invoke_agent_1794311070223_0")
        );

        // `/compress` between two answered turns: the summary is said once, and Gemini's
        // acknowledgement of it is no answer of the agent.
        let compress = read(&store, "df636b9c-090c-436e-a92e-7e8798e4b943", None, None);
        assert!(compress.whole && !compress.more);
        assert_eq!(
            turns(&compress.events),
            ["Finished", "Finished", "compacted", "Finished"]
        );
        let agent = |events: &[Event]| {
            events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        Event::Words {
                            voice: Voice::Agent,
                            ..
                        }
                    )
                })
                .count()
        };
        assert_eq!(agent(&compress.events), 3);

        // `/rewind` to the second prompt takes its turn back.
        let rewind = read(&store, "760907b6-38c2-467e-8585-e4c4458da883", None, None);
        assert_eq!(
            turns(&rewind.events),
            ["Finished", "Interrupted", "rollback 1", "Finished"]
        );

        // The sub-agent's own chat ends at its `complete_task`; its lead says how it ended.
        let lead = read(&store, "488d4dbe-87e4-4026-aa02-5905bc8790fb", None, None);
        assert!(lead.events.iter().any(|event| matches!(
            event,
            Event::SubAgent { external_id, state: Some(Execution::Finished), .. }
                if external_id == "7480c29e-d0d0-4421-bada-2db4dd0aa087"
        )));
        let child = read(
            &store,
            "488d4dbe-87e4-4026-aa02-5905bc8790fb",
            Some("7480c29e-d0d0-4421-bada-2db4dd0aa087"),
            None,
        );
        assert_eq!(turns(&child.events), ["Finished"]);

        // A model call that failed (HTTP 429) rewinds its own prompt away.
        let failed = std::fs::read_dir(root.join("tmp/ratelimit/chats"))
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path();
        let failed = events::read(&failed, None, None).unwrap();
        assert_eq!(turns(&failed.events), ["Interrupted", "rollback 1"]);

        // A conversation that is not listed is not read.
        assert!(
            store
                .read(Read {
                    external_id: "2216106a-26ee-4868-8f78-dee3ab5ef4c1".into(),
                    sub_agent: None,
                    cursor: None,
                    tail: None,
                })
                .is_err()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A chat file read while Gemini writes it, a few records at a time, says the same as
    /// one read of the whole file; the last turn stays open while the file is fresh.
    #[test]
    fn a_chat_read_while_written_says_what_one_read_says() {
        let root = root("follow");
        let file = std::fs::read_dir(root.join("tmp/interactive/chats"))
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path();
        let whole = std::fs::read(&file).unwrap();
        let lines: Vec<&[u8]> = whole.split_inclusive(|byte| *byte == b'\n').collect();
        std::fs::write(&file, b"").unwrap();
        let mut said = Vec::new();
        let mut cursor = None;
        for chunk in lines.chunks(3) {
            let mut open = File::options().append(true).open(&file).unwrap();
            std::io::Write::write_all(&mut open, &chunk.concat()).unwrap();
            let events = events::read(&file, cursor.as_deref(), None).unwrap();
            assert!(!events.reset);
            said.extend(events.events);
            cursor = Some(events.cursor);
        }
        let once = events::read(&file, None, None).unwrap();
        assert_eq!(
            serde_json::to_value(&said).unwrap(),
            serde_json::to_value(&once.events).unwrap()
        );
        assert_eq!(
            turns(&once.events),
            ["Finished"],
            "the newest turn is still open"
        );

        // Written again from its start, the file is read again from its start.
        std::fs::write(&file, &whole[..lines[0].len()]).unwrap();
        assert!(events::read(&file, cursor.as_deref(), None).unwrap().reset);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
