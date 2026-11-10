//! Grok's conversations: a folder for each under `<root>/sessions/<encoded folder>/<id>/`,
//! with its `summary.json` and its transcript `updates.jsonl`. A sub-agent's run is a
//! conversation folder of its own, whose `session_kind` is `subagent…`; its lead keeps
//! `subagents/<id>/meta.json` about it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session, SubAgent};
use steery_sdk::{Error, refusal};

use super::events;

const SUMMARY: &str = "summary.json";
const UPDATES: &str = "updates.jsonl";

/// Grok's conversations under its root.
pub struct Store {
    sessions: PathBuf,
}

/// One conversation folder, as its summary says.
struct Folder {
    dir: PathBuf,
    id: String,
    cwd: Option<PathBuf>,
    title: Option<String>,
    created_at: Option<u64>,
    updated_at: u64,
    kind: Option<String>,
    parent: Option<String>,
}

impl Folder {
    /// The folder at `dir`, or `None` where its summary does not read.
    fn read(dir: PathBuf) -> Option<Self> {
        let summary: Value =
            serde_json::from_slice(&std::fs::read(dir.join(SUMMARY)).ok()?).ok()?;
        let text = |key: &str| {
            summary[key]
                .as_str()
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned)
        };
        let id = summary["info"]["id"].as_str()?.to_owned();
        let updated_at = [
            text("updated_at").as_deref().and_then(events::millis),
            events::changed(&dir.join(UPDATES)),
        ]
        .into_iter()
        .flatten()
        .max()?;
        Some(Self {
            cwd: summary["info"]["cwd"].as_str().map(PathBuf::from),
            title: text("generated_title").or_else(|| text("session_summary")),
            created_at: text("created_at").as_deref().and_then(events::millis),
            updated_at,
            kind: text("session_kind"),
            parent: text("parent_session_id"),
            id,
            dir,
        })
    }

    /// A run with no person at it.
    fn headless(&self) -> bool {
        self.kind.as_deref() == Some("headless")
    }

    fn sub_agent(&self) -> bool {
        self.kind
            .as_deref()
            .is_some_and(|kind| kind.starts_with("subagent"))
    }

    fn transcript(&self) -> PathBuf {
        self.dir.join(UPDATES)
    }
}

/// Every conversation folder by its id, and whether each read whole and alone.
struct Index {
    by_id: BTreeMap<String, Folder>,
    complete: bool,
}

impl Store {
    pub fn new(root: &Path) -> Self {
        Self {
            sessions: root.join("sessions"),
        }
    }

    fn index(&self) -> Index {
        let mut by_id: BTreeMap<String, Folder> = BTreeMap::new();
        let mut complete = true;
        let places = std::fs::read_dir(&self.sessions)
            .into_iter()
            .flatten()
            .flatten();
        for place in places.filter(|entry| entry.path().is_dir()) {
            let Ok(folders) = std::fs::read_dir(place.path()) else {
                complete = false;
                continue;
            };
            for dir in folders.flatten().map(|entry| entry.path()) {
                if !dir.join(UPDATES).is_file() && !dir.join(SUMMARY).is_file() {
                    continue;
                }
                let Some(folder) = Folder::read(dir) else {
                    complete = false;
                    continue;
                };
                // Two folders that name one id: the newest is it, and the listing is unsure.
                if let Some(kept) = by_id.get(&folder.id) {
                    complete = false;
                    if kept.updated_at >= folder.updated_at {
                        continue;
                    }
                }
                by_id.insert(folder.id.clone(), folder);
            }
        }
        Index { by_id, complete }
    }

    /// `sessions/list`.
    pub fn list(&self, list: List) -> Listed {
        let index = self.index();
        let mut excluded = Vec::new();
        let mut sessions = Vec::new();
        for folder in index.by_id.values() {
            if folder.headless() || folder.sub_agent() {
                excluded.push(folder.id.clone());
                continue;
            }
            sessions.push(session(&index, folder));
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
        let folder = conversation(&index, &locate.external_id)?;
        Ok(Located {
            session: session(&index, folder),
        })
    }

    /// `sessions/read`.
    pub fn read(&self, read: Read) -> Result<Events, Error> {
        let index = self.index();
        let lead = conversation(&index, &read.external_id)?;
        let folder = match &read.sub_agent {
            None => lead,
            Some(child) => index
                .by_id
                .get(child)
                .filter(|folder| {
                    folder.parent.as_deref() == Some(lead.id.as_str())
                        || lead.dir.join("subagents").join(child).is_dir()
                })
                .ok_or_else(|| not_found(child))?,
        };
        // A sub-agent's parent is its lead, not a fork: only a fork's parent copied history.
        let parent = (!folder.sub_agent())
            .then_some(folder.parent.as_deref())
            .flatten();
        events::read(
            &folder.transcript(),
            read.cursor.as_deref(),
            read.tail,
            parent,
        )
    }
}

/// The conversation `id` names: one with a person at it.
fn conversation<'a>(index: &'a Index, id: &str) -> Result<&'a Folder, Error> {
    index
        .by_id
        .get(id)
        .filter(|folder| !folder.headless() && !folder.sub_agent())
        .ok_or_else(|| not_found(id))
}

fn not_found(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("Grok keeps no conversation `{id}` here"),
    )
}

fn session(index: &Index, folder: &Folder) -> Session {
    Session {
        external_id: folder.id.clone(),
        cwd: folder.cwd.clone(),
        title: folder.title.clone(),
        created_at: folder.created_at,
        updated_at: folder.updated_at,
        transcripts: vec![folder.transcript()],
        parent: folder.parent.clone(),
        app: false,
        sub_agents: sub_agents(index, folder),
    }
}

/// The sub-agents of a conversation: those its `subagents/<id>/meta.json` names, and the
/// sub-agent folders that name it as their parent.
fn sub_agents(index: &Index, lead: &Folder) -> Vec<SubAgent> {
    let mut found: BTreeMap<String, SubAgent> = BTreeMap::new();
    let metas = std::fs::read_dir(lead.dir.join("subagents"))
        .into_iter()
        .flatten()
        .flatten();
    for entry in metas {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let meta: Value = std::fs::read(entry.path().join("meta.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null);
        let text = |key: &str| {
            meta[key]
                .as_str()
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned)
        };
        let id = text("child_session_id")
            .or_else(|| text("subagent_id"))
            .unwrap_or(name);
        let asked = text("prompt").or_else(|| text("description"));
        found.insert(
            id.clone(),
            SubAgent {
                name: text("subagent_type"),
                task: asked.and_then(|asked| asked.lines().next().map(str::to_owned)),
                started_at: text("started_at").as_deref().and_then(events::millis),
                transcripts: transcripts(index, &id),
                external_id: id,
                lead: None,
                started_by: None,
            },
        );
    }
    for child in index.by_id.values() {
        if child.sub_agent() && child.parent.as_deref() == Some(lead.id.as_str()) {
            found.entry(child.id.clone()).or_insert_with(|| SubAgent {
                external_id: child.id.clone(),
                lead: None,
                name: None,
                task: None,
                started_at: child.created_at,
                started_by: None,
                transcripts: vec![child.transcript()],
            });
        }
    }
    let mut found: Vec<SubAgent> = found.into_values().collect();
    found.sort_by(|a, b| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.external_id.cmp(&b.external_id))
    });
    found
}

fn transcripts(index: &Index, id: &str) -> Vec<PathBuf> {
    index
        .by_id
        .get(id)
        .map(Folder::transcript)
        .into_iter()
        .collect()
}
