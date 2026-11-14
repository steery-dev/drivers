//! Kiro's conversations: each `~/.kiro/sessions/<bucket>/<id>/`, where `<bucket>` is the
//! first 16 hex digits of the SHA-256 of the conversation's folder. Inside, `session.json`
//! holds its title, folder, model and the conversation it forks, `messages.jsonl` its
//! records ([`super::events`]), and `sub-executions/<sub>.jsonl` each sub-agent's.
//!
//! Kiro writes `~/.kiro/session-leases/<id>.lease` while a process holds a conversation, and
//! leaves it behind when that process ends, so a lease holds only while its `pid` lives.
//!
//! A conversation with no prompt of a person is excluded.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use memchr::memmem;
use serde_json::Value;
use steery_driver_common::kernel;
use steery_sdk::wire::harness::{
    Event, Events, List, Listed, Locate, Located, Read, Session, SubAgent, TurnState,
};
use steery_sdk::{Error, refusal};

use super::events;

/// What marks a person's prompt in a record.
const PROMPT: &[u8] = b"\"type\":\"user\"";

/// The conversation file in a conversation's folder.
const MESSAGES: &str = "messages.jsonl";

/// Kiro's sub-agents' files in a conversation's folder.
const SUB_EXECUTIONS: &str = "sub-executions";

pub struct Store {
    root: PathBuf,
}

/// What `session.json` says of a conversation.
pub struct Head {
    pub id: String,
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    pub parent: Option<String>,
    /// The newest record of the parent's history that a fork copied.
    pub forked_at: Option<String>,
    pub created_at: Option<u64>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl Store {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    /// Every conversation's folder.
    fn folders(&self) -> Vec<PathBuf> {
        let entries = |folder: &Path| {
            std::fs::read_dir(folder)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect::<Vec<_>>()
        };
        let mut folders: Vec<PathBuf> = entries(&self.root.join("sessions"))
            .iter()
            .flat_map(|bucket| entries(bucket))
            .filter(|folder| folder.join(MESSAGES).is_file())
            .collect();
        folders.sort();
        folders
    }

    /// The folder of the conversation `id`, where a person typed into it.
    pub fn folder(&self, id: &str) -> Result<(PathBuf, Head), Error> {
        if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
            return Err(not_found(id));
        }
        self.folders()
            .into_iter()
            .filter(|folder| folder.file_name().is_some_and(|name| *name == *id))
            .find_map(|folder| {
                let head = head(&folder)?;
                person(&folder).then_some((folder, head))
            })
            .ok_or_else(|| not_found(id))
    }

    /// The folder the conversation `id` ran in.
    pub fn cwd(&self, id: &str) -> Result<Option<PathBuf>, Error> {
        Ok(self.folder(id)?.1.cwd)
    }

    /// `sessions/list`.
    pub fn list(&self, list: List) -> Listed {
        let mut sessions = Vec::new();
        let mut excluded = Vec::new();
        let mut seen = HashSet::new();
        let mut complete = true;
        for folder in self.folders() {
            let Some(head) = head(&folder) else {
                continue;
            };
            if !seen.insert(head.id.clone()) {
                complete = false;
            }
            if !person(&folder) {
                excluded.push(head.id);
                continue;
            }
            let session = session(&folder, head);
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

    /// `sessions/locate`.
    pub fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let (folder, head) = self.folder(&locate.external_id)?;
        Ok(Located {
            session: session(&folder, head),
        })
    }

    /// The pid of the live process whose lease holds the conversation `id`, and when it
    /// started. A pid that started after the lease was taken is another process.
    pub fn holder(&self, id: &str) -> Option<(u32, u64)> {
        let lease =
            std::fs::read(self.root.join("session-leases").join(format!("{id}.lease"))).ok()?;
        let lease: Value = serde_json::from_slice(&lease).ok()?;
        let pid = u32::try_from(lease["pid"].as_u64()?).ok()?;
        let started = kernel::started(pid)?;
        let acquired = lease["acquiredAt"].as_str().and_then(events::millis)?;
        (started <= acquired).then_some((pid, started))
    }

    /// `sessions/read`. The cursor is the length read, and the turn that the read ended as
    /// interrupted while no process held the conversation, so that its later end is not
    /// said again.
    pub fn read(&self, read: Read) -> Result<Events, Error> {
        let (folder, head) = self.folder(&read.external_id)?;
        let (file, fork) = match &read.sub_agent {
            Some(sub) => {
                let file = folder.join(SUB_EXECUTIONS).join(format!("{sub}.jsonl"));
                if sub.contains(['/', '\\']) || !file.is_file() {
                    return Err(Error::refused(
                        refusal::NOT_FOUND,
                        format!("Kiro keeps no sub-agent `{sub}` of `{}`", read.external_id),
                    ));
                }
                (file, None)
            }
            None => (
                folder.join(MESSAGES),
                head.parent.clone().zip(head.forked_at.clone()),
            ),
        };
        let bytes = std::fs::read(&file)
            .map_err(|e| Error::refused(refusal::NOT_FOUND, format!("{}: {e}", file.display())))?;
        let mut parsed = events::parse(&bytes, fork);
        let (mut from, settled) = match read.cursor.as_deref() {
            None => (0, None),
            Some(cursor) => {
                let (length, turn) = cursor.split_once('/').unwrap_or((cursor, ""));
                let length = length.parse::<u64>().map_err(|_| {
                    Error::invalid(format!("not a cursor of this driver: {cursor}"))
                })?;
                (length, (!turn.is_empty()).then(|| turn.to_owned()))
            }
        };
        let reset = from > parsed.len;
        if reset {
            from = 0;
        }
        let settled = settled.filter(|_| !reset);
        let mut kept = settled
            .clone()
            .filter(|turn| parsed.open.as_ref() == Some(turn));
        if let Some(turn) = parsed.open.clone()
            && kept.is_none()
            && self.holder(&read.external_id).is_none()
        {
            parsed.said.push(events::Said {
                start: parsed.len,
                end: parsed.len + 1,
                event: Event::TurnEnded {
                    at: parsed.at,
                    turn: Some(turn.clone()),
                    state: TurnState::Interrupted,
                    failure: None,
                },
            });
            kept = Some(turn);
        }
        let tail_from = read.tail.map(|tail| parsed.len.saturating_sub(tail));
        let mut said: Vec<Event> = parsed
            .said
            .into_iter()
            .filter(|said| match tail_from {
                Some(at) => said.start >= at,
                None => said.end > from,
            })
            .map(|said| said.event)
            .filter(|event| {
                !matches!(event, Event::TurnEnded { turn, .. }
                    if settled.is_some() && *turn == settled)
            })
            .collect();
        let whole = from == 0 && tail_from.is_none();
        if whole
            && read.sub_agent.is_none()
            && let Some(model) = &head.model
        {
            said.insert(
                0,
                Event::Model {
                    at: parsed.first.unwrap_or(parsed.at),
                    model: model.clone(),
                    provider: None,
                    thinking: head.effort.clone(),
                },
            );
        }
        Ok(Events {
            cursor: match &kept {
                Some(turn) => format!("{}/{turn}", parsed.len),
                None => parsed.len.to_string(),
            },
            events: said,
            more: false,
            reset,
            whole,
            after: parsed.newest.filter(|_| tail_from.is_none()),
        })
    }

    /// The person's words in the conversation `id` after the first `mark` records.
    pub fn said_after(&self, id: &str, mark: &str) -> Result<Vec<String>, Error> {
        let mark: usize = mark
            .parse()
            .map_err(|_| Error::invalid(format!("not a mark of this driver: {mark}")))?;
        let (folder, _) = self.folder(id)?;
        Ok(records(&folder.join(MESSAGES))
            .into_iter()
            .skip(mark)
            .filter(|record| record["payload"]["type"] == "user")
            .filter_map(|record| record["payload"]["content"].as_str().map(str::to_owned))
            .collect())
    }

    /// How many records the conversation `id` has.
    pub fn count(&self, id: &str) -> Result<usize, Error> {
        let (folder, _) = self.folder(id)?;
        Ok(records(&folder.join(MESSAGES)).len())
    }

    /// The records of the conversation `id`, its newest last.
    pub fn records(&self, id: &str) -> Result<Vec<Value>, Error> {
        let (folder, _) = self.folder(id)?;
        Ok(records(&folder.join(MESSAGES)))
    }
}

/// Every whole record of `file`.
fn records(file: &Path) -> Vec<Value> {
    let bytes = std::fs::read(file).unwrap_or_default();
    let whole = bytes
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(&[][..], |end| &bytes[..end]);
    whole
        .split(|&b| b == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect()
}

/// What `session.json` says of the conversation in `folder`.
fn head(folder: &Path) -> Option<Head> {
    let bytes = std::fs::read(folder.join("session.json")).ok()?;
    let json: Value = serde_json::from_slice(&bytes).ok()?;
    let text = |key: &str| {
        json[key]
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    Some(Head {
        id: text("id").or_else(|| Some(folder.file_name()?.to_str()?.to_owned()))?,
        title: text("title"),
        cwd: json["workspacePaths"][0].as_str().map(PathBuf::from),
        parent: text("parentSessionId"),
        forked_at: text("forkedAtMessageId"),
        created_at: json["createdAt"].as_str().and_then(events::millis),
        model: text("modelId"),
        effort: text("effortLevel"),
    })
}

/// Whether a person typed into the conversation in `folder`.
fn person(folder: &Path) -> bool {
    std::fs::read(folder.join(MESSAGES)).is_ok_and(|bytes| memmem::find(&bytes, PROMPT).is_some())
}

fn session(folder: &Path, head: Head) -> Session {
    let file = folder.join(MESSAGES);
    Session {
        external_id: head.id.clone(),
        cwd: head.cwd,
        title: head.title,
        created_at: head.created_at,
        updated_at: stamp(&file).unwrap_or(0),
        transcripts: vec![file.clone()],
        parent: head.parent,
        app: false,
        sub_agents: sub_agents(&file, folder, &head.id),
    }
}

/// The sub-agents the conversation started, each as its `sub_agent_start` names it.
fn sub_agents(file: &Path, folder: &Path, lead: &str) -> Vec<SubAgent> {
    let bytes = std::fs::read(file).unwrap_or_default();
    if memmem::find(&bytes, b"\"sub_agent_start\"").is_none() {
        return Vec::new();
    }
    records(file)
        .into_iter()
        .filter(|record| record["payload"]["type"] == "sub_agent_start")
        .filter_map(|record| {
            let payload = &record["payload"];
            let id = payload["subSessionId"].as_str()?.to_owned();
            let transcript = folder.join(SUB_EXECUTIONS).join(format!("{id}.jsonl"));
            Some(SubAgent {
                external_id: id,
                lead: Some(lead.to_owned()),
                name: payload["subAgentName"].as_str().map(str::to_owned),
                task: payload["prompt"].as_str().map(str::to_owned),
                started_at: record["timestamp"].as_str().and_then(events::millis),
                started_by: None,
                transcripts: transcript
                    .is_file()
                    .then_some(transcript)
                    .into_iter()
                    .collect(),
            })
        })
        .collect()
}

/// When a file last changed, in milliseconds since 1970.
fn stamp(file: &Path) -> Option<u64> {
    let at = std::fs::metadata(file)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?;
    u64::try_from(at.as_millis()).ok()
}

fn not_found(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("Kiro keeps no conversation `{id}` that a person typed into"),
    )
}
