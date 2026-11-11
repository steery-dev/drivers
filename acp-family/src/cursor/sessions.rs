//! Cursor's conversations: where they live, and the events of each.
//!
//! Cursor keeps each chat in a folder of its own under its config folder:
//! `chats/<md5 of the folder it ran in>/<id>/` for a chat of its terminal app or of `-p`,
//! and `acp-sessions/<id>/` for a chat that an ACP client started. Each folder holds
//! `store.db` (see [`super::store`]) and `meta.json`, Cursor's note of its folder, title and
//! times. A chat that a person typed in also holds `prompt_history.json`, the words for the
//! up arrow; a `-p` run, which no person is at, never writes it.
//!
//! The events of a chat are read from its turns. Each turn is a person's words and the
//! steps that answered them. Cursor writes a step once it is whole: a call once it returned.
//! A turn's events only grow, so the cursor is a turn and how many of its events were read.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Event, Events, List, Listed, Locate, Located, Read, Session, TurnState, Voice,
};

use super::calls;
use super::store::{self, Chat, Root, Step, Turn, User};

/// The most events one read answers; more come with `more`.
const EVENTS_MAX: usize = 2000;

/// Where Cursor keeps its chats.
pub struct Store {
    root: PathBuf,
}

/// One chat's folder.
struct Folder {
    id: String,
    path: PathBuf,
    /// A person typed in it.
    person: bool,
}

impl Folder {
    fn store(&self) -> PathBuf {
        self.path.join("store.db")
    }

    /// Cursor has the chat open. It leaves the log and its index beside the file once it
    /// closes, so only a lock on the index says that a process still has it open.
    fn open(&self) -> bool {
        store::held(&self.store())
    }
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list(&self, asked: List) -> Listed {
        let mut listed = Listed {
            sessions: Vec::new(),
            excluded: Vec::new(),
            complete: true,
            cursor: None,
            after: None,
            gone: Vec::new(),
        };
        for folder in self.folders() {
            let Some(session) = session(&folder) else {
                continue;
            };
            if asked
                .seen_since
                .is_some_and(|since| session.updated_at < since)
            {
                continue;
            }
            if folder.person {
                listed.sessions.push(session);
            } else {
                listed.excluded.push(folder.id);
            }
        }
        listed
    }

    pub fn locate(&self, asked: Locate) -> Result<Located, Error> {
        self.find(&asked.external_id)
            .filter(|folder| folder.person)
            .and_then(|folder| session(&folder))
            .map(|session| Located { session })
            .ok_or_else(not_found)
    }

    pub fn read(&self, asked: Read) -> Result<Events, Error> {
        if asked.sub_agent.is_some() {
            return Err(not_found());
        }
        let folder = self.find(&asked.external_id).ok_or_else(not_found)?;
        let refused = |why: String| {
            Error::refused(
                "refused",
                format!("Cursor's store of this chat cannot be read: {why}."),
            )
        };
        // Asked before the store is opened: closing any file of the store drops every lock
        // this process holds on it, the driver's own connection's too.
        let attempt = |open: bool| {
            let chat = Chat::open(&folder.store())?;
            let root = chat.root()?;
            read(&chat, &root, open, &asked)
        };
        // A read that races Cursor closing the chat can fail; once nobody holds the chat,
        // it reads once more.
        attempt(folder.open())
            .or_else(|why| match folder.open() {
                true => Err(why),
                false => attempt(false),
            })
            .map_err(refused)
    }

    /// Every chat's folder that holds a store.
    fn folders(&self) -> Vec<Folder> {
        let mut folders = Vec::new();
        for project in children(&self.root.join("chats")) {
            for path in children(&project) {
                folders.extend(folder(path, false));
            }
        }
        for path in children(&self.root.join("acp-sessions")) {
            folders.extend(folder(path, true));
        }
        folders
    }

    /// The folder of chat `id`.
    fn find(&self, id: &str) -> Option<Folder> {
        if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
            return None;
        }
        folder(self.root.join("acp-sessions").join(id), true).or_else(|| {
            children(&self.root.join("chats"))
                .into_iter()
                .find_map(|project| folder(project.join(id), false))
        })
    }

    /// The store of chat `id`, a person's or a run's.
    pub fn transcript(&self, id: &str) -> Result<PathBuf, Error> {
        self.find(id)
            .map(|folder| folder.store())
            .ok_or_else(not_found)
    }

    /// The words of the person in each turn of chat `id`, oldest first.
    pub fn said(&self, id: &str) -> Result<Vec<String>, Error> {
        let folder = self.find(id).ok_or_else(not_found)?;
        let said = || -> Result<Vec<String>, String> {
            let chat = Chat::open(&folder.store())?;
            let root = chat.root()?;
            let mut said = Vec::new();
            for id in &root.turns {
                let Some(turn) = chat.blob(id)?.and_then(|bytes| Turn::read(&bytes)) else {
                    continue;
                };
                let user = chat
                    .blob(&turn.user)?
                    .map(|bytes| User::read(&bytes))
                    .unwrap_or_default();
                said.push(user.text);
            }
            Ok(said)
        };
        said().map_err(|why| {
            Error::refused(
                "refused",
                format!("Cursor's store of this chat cannot be read: {why}."),
            )
        })
    }
}

/// The chat in `path`, where it holds a store. A chat of an ACP client always has a person
/// or a program at it, which the folder does not say: it is read as a person's.
fn folder(path: PathBuf, acp: bool) -> Option<Folder> {
    if !path.join("store.db").is_file() {
        return None;
    }
    let id = path.file_name()?.to_str()?.to_owned();
    let person = acp || path.join("prompt_history.json").is_file();
    Some(Folder { id, path, person })
}

fn children(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

fn session(folder: &Folder) -> Option<Session> {
    let note = steery_driver_common::mcp::json(&folder.path.join("meta.json"));
    let note = note.as_ref();
    let text = |key| note.and_then(|note| note.get(key)).and_then(Value::as_str);
    let stamp = |key| note.and_then(|note| note.get(key)).and_then(Value::as_u64);
    let changed = ["store.db", "store.db-wal", "meta.json"]
        .iter()
        .filter_map(|name| modified(&folder.path.join(name)))
        .chain(stamp("updatedAtMs"))
        .max()?;
    Some(Session {
        external_id: folder.id.clone(),
        cwd: text("cwd").map(PathBuf::from),
        title: text("title").map(str::to_owned),
        created_at: stamp("createdAtMs"),
        updated_at: changed,
        transcripts: vec![folder.store()],
        parent: None,
        app: false,
        sub_agents: Vec::new(),
    })
}

fn modified(path: &Path) -> Option<u64> {
    let time = std::fs::metadata(path).ok()?.modified().ok()?;
    u64::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}

fn not_found() -> Error {
    Error::refused("not-found", "")
}

/// Where a read ended: the turn, how many of its events were read, and the blob of its
/// person's message, which says that the turn is still the one the cursor followed.
#[derive(Debug, PartialEq)]
struct Mark {
    turn: usize,
    read: usize,
    user: String,
}

impl Mark {
    fn parse(cursor: &str) -> Option<Self> {
        let mut parts = cursor.splitn(3, ':');
        Some(Self {
            turn: parts.next()?.parse().ok()?,
            read: parts.next()?.parse().ok()?,
            user: parts.next()?.to_owned(),
        })
    }

    fn text(&self) -> String {
        format!("{}:{}:{}", self.turn, self.read, self.user)
    }
}

fn read(chat: &Chat, root: &Root, open: bool, asked: &Read) -> Result<Events, String> {
    let turns: Vec<Option<Turn>> = root
        .turns
        .iter()
        .map(|id| Ok(chat.blob(id)?.and_then(|bytes| Turn::read(&bytes))))
        .collect::<Result<_, String>>()?;
    let user = |index: usize| {
        turns
            .get(index)
            .and_then(Option::as_ref)
            .map(|turn| store::hex(&turn.user))
            .unwrap_or_default()
    };
    let (mut start, mut skip, mut reset) = (0, 0, false);
    if asked.tail.is_some() {
        start = turns.len().saturating_sub(1);
    } else if let Some(mark) = asked.cursor.as_deref().map(Mark::parse) {
        match mark {
            // A cursor that read nothing of a chat with no turns yet reads on from its start.
            Some(mark)
                if mark.turn < turns.len().max(1)
                    && (mark.user == user(mark.turn) || mark.read == 0 && mark.user.is_empty()) =>
            {
                (start, skip) = (mark.turn, mark.read);
            }
            _ => reset = true,
        }
    }
    let whole = start == 0 && skip == 0;
    let mut messages = Messages::new(chat, root);
    let finished = root_finished(chat, root)?;
    let (mut events, mut more) = (Vec::new(), false);
    let mut mark = Mark {
        turn: start,
        read: skip,
        user: user(start),
    };
    for (index, turn) in turns.iter().enumerate().skip(start) {
        let Some(turn) = turn else {
            continue;
        };
        let ending = if index + 1 < turns.len() {
            Ending::Over
        } else if finished {
            Ending::Finished
        } else if !open {
            Ending::Gone
        } else {
            Ending::Open
        };
        let said = turn_events(chat, turn, ending, &mut messages)?;
        let from = if index == start { skip } else { 0 };
        let room = EVENTS_MAX - events.len();
        let said_len = said.len();
        let taken = said_len.saturating_sub(from).min(room);
        events.extend(said.into_iter().skip(from).take(taken));
        mark = Mark {
            turn: index,
            read: from + taken,
            user: user(index),
        };
        if events.len() >= EVENTS_MAX {
            more = index + 1 < turns.len() || from + taken < said_len;
            break;
        }
    }
    Ok(Events {
        cursor: mark.text(),
        events,
        more,
        reset,
        whole: whole || reset,
        after: None,
    })
}

/// The newest turn ended: no call waits to return, and the newest message that the model
/// reads is the agent's, with no call in it. While a call runs, or before the agent's
/// message that holds a call is written, the newest message is the person's or a result.
fn root_finished(chat: &Chat, root: &Root) -> Result<bool, String> {
    if root.pending {
        return Ok(false);
    }
    let Some(id) = root.messages.last() else {
        return Ok(false);
    };
    let Some(message) = chat.message(id)? else {
        return Ok(false);
    };
    let calls = message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|content| {
            content
                .iter()
                .any(|part| part.get("type").and_then(Value::as_str) == Some("tool-call"))
        });
    Ok(message.get("role").and_then(Value::as_str) == Some("assistant") && !calls)
}

/// How a turn stands.
#[derive(Clone, Copy, PartialEq)]
enum Ending {
    /// A newer turn follows it: it ended, finished where its last step is the agent's words.
    Over,
    /// The newest turn, and it ended.
    Finished,
    /// The newest turn, not ended, and no process has the chat open: Cursor stopped. It
    /// ends as [`Ending::Over`] does.
    Gone,
    /// The newest turn, still running.
    Open,
}

/// Every event of `turn` that stands now. While it runs, its events stop before the first
/// step that is not whole, so they only ever grow.
fn turn_events(
    chat: &Chat,
    turn: &Turn,
    ending: Ending,
    messages: &mut Messages,
) -> Result<Vec<Event>, String> {
    let user = chat
        .blob(&turn.user)?
        .map(|bytes| User::read(&bytes))
        .unwrap_or_default();
    let key = turn
        .request
        .clone()
        .or_else(|| user.id.clone())
        .unwrap_or_else(|| store::hex(&turn.user));
    let mut at = user.at.unwrap_or_default();
    let mut events = vec![Event::TurnBegan {
        at,
        turn: turn.request.clone(),
    }];
    if !user.text.trim().is_empty() {
        events.push(Event::Words {
            at,
            id: user.id.clone().unwrap_or_else(|| format!("{key}:user")),
            voice: Voice::Person,
            text: user.text,
            message: None,
            call: None,
            clips: Vec::new(),
        });
    }
    let running = ending == Ending::Open;
    let mut last = None;
    for (index, id) in turn.steps.iter().enumerate() {
        let Some(step) = chat.blob(id)?.and_then(|bytes| Step::read(&bytes)) else {
            if running {
                return Ok(events);
            }
            continue;
        };
        let (voice, text, began, ended) = match step {
            Step::Said { text, began, ended } => (Voice::Agent, text, began, ended),
            Step::Thought { text, began, ended } => (Voice::Thought, text, began, ended),
            Step::Call(step) => {
                let id = step.id.clone().unwrap_or_else(|| format!("{key}:{index}"));
                let written = messages.call(&id)?;
                let result = messages.result(&id)?;
                // A running call is told once its start and input are written; its end waits
                // for its result.
                if running && (step.began.is_none() || written.is_none()) {
                    return Ok(events);
                }
                at = step.began.or(step.ended).unwrap_or(at);
                let (name, args) = match &written {
                    Some((name, args)) => (Some(name.as_str()), Some(args)),
                    None => (None, None),
                };
                events.push(Event::CallBegan {
                    at,
                    call: calls::call(&id, name, args, step.mcp.as_ref()),
                });
                if running && (step.ended.is_none() || result.is_none()) {
                    return Ok(events);
                }
                if let Some(ended) = step.ended {
                    at = ended;
                    let text = result.map(said).unwrap_or_default();
                    events.push(Event::CallEnded {
                        at,
                        id: id.clone(),
                        ok: step.ok == Some(true),
                        chars: text.chars().count() as u64,
                        tail: tail(&text),
                        waited: Vec::new(),
                    });
                    if !text.trim().is_empty() {
                        events.push(words(
                            at,
                            format!("{id}:result"),
                            Voice::Tool,
                            text,
                            Some(id),
                        ));
                    }
                }
                last = Some(index);
                continue;
            }
        };
        // The newest words of a running turn can still grow: they wait for the next step.
        if running && index + 1 == turn.steps.len() {
            return Ok(events);
        }
        at = ended.or(began).unwrap_or(at);
        if !text.trim().is_empty() {
            events.push(words(at, format!("{key}:{index}"), voice, text, None));
        }
        last = Some(index);
    }
    let state = match ending {
        Ending::Open => return Ok(events),
        Ending::Finished => TurnState::Finished,
        // A turn that Cursor left reads as it will once a newer turn follows it, so its end
        // never changes. Its messages say how it ended; a turn with no message of its own
        // left, finished where its last step is the agent's words.
        Ending::Over | Ending::Gone => {
            let by_messages = match &turn.request {
                Some(request) => messages.finished(request)?,
                None => None,
            };
            let finished = by_messages.unwrap_or_else(|| {
                let said = last
                    .and_then(|index| chat.blob(&turn.steps[index]).ok().flatten())
                    .and_then(|bytes| Step::read(&bytes));
                matches!(said, Some(Step::Said { .. }))
            });
            if finished {
                TurnState::Finished
            } else {
                TurnState::Interrupted
            }
        }
    };
    events.push(Event::TurnEnded {
        at,
        turn: turn.request.clone(),
        state,
        failure: None,
    });
    Ok(events)
}

fn words(at: u64, id: String, voice: Voice, text: String, call: Option<String>) -> Event {
    Event::Words {
        at,
        id,
        voice,
        text,
        message: None,
        call,
        clips: Vec::new(),
    }
}

/// A result as text: a string as it is, anything else as its JSON.
fn said(result: Value) -> String {
    match result {
        Value::String(text) => text,
        other => other.to_string(),
    }
}

/// The last 8 lines of `text`; none where it is empty.
fn tail(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    Some(lines[lines.len().saturating_sub(8)..].join("\n"))
}

/// The JSON messages of a chat, read newest first and only as far as a call asks: each
/// call's tool and input, and each result.
struct Messages<'a> {
    chat: &'a Chat,
    /// Every message, oldest first: the ones that summaries replaced, then the ones the
    /// model reads now.
    ids: Vec<Vec<u8>>,
    /// How many of `ids` are not read yet.
    unread: usize,
    calls: HashMap<String, (String, Value)>,
    results: HashMap<String, Value>,
    /// Whether each person's message, by its request, began a turn that ended finished.
    ends: HashMap<String, bool>,
    /// Of the messages read since the newest person's message read: whether the newest of
    /// them is the agent's answer, with no call in it.
    newest: Option<bool>,
}

impl<'a> Messages<'a> {
    fn new(chat: &'a Chat, root: &Root) -> Self {
        let mut ids: Vec<Vec<u8>> = root
            .archives
            .iter()
            .filter_map(|id| chat.blob(id).ok().flatten())
            .flat_map(|bytes| store::archived(&bytes))
            .collect();
        ids.extend(root.messages.iter().cloned());
        Self {
            chat,
            unread: ids.len(),
            ids,
            calls: HashMap::new(),
            results: HashMap::new(),
            ends: HashMap::new(),
            newest: None,
        }
    }

    fn call(&mut self, id: &str) -> Result<Option<(String, Value)>, String> {
        while !self.calls.contains_key(id) && self.next()? {}
        Ok(self.calls.get(id).cloned())
    }

    fn result(&mut self, id: &str) -> Result<Option<Value>, String> {
        while !self.results.contains_key(id) && self.next()? {}
        Ok(self.results.get(id).cloned())
    }

    /// Whether the turn of the person's message `request` ended with the agent's answer:
    /// its newest message is the agent's, and holds no call. A turn that Cursor stopped
    /// ends on the person's message, or on a call and the result that Cursor wrote for it
    /// as the chat resumed ("interrupted before it completed"). None where no message is
    /// the person's of `request`.
    fn finished(&mut self, request: &str) -> Result<Option<bool>, String> {
        while !self.ends.contains_key(request) && self.next()? {}
        Ok(self.ends.get(request).copied())
    }

    /// Reads the newest unread message; false where none is left.
    fn next(&mut self) -> Result<bool, String> {
        let Some(index) = self.unread.checked_sub(1) else {
            return Ok(false);
        };
        self.unread = index;
        let Some(message) = self.chat.message(&self.ids[index])? else {
            return Ok(true);
        };
        let parts = message.get("content").and_then(Value::as_array);
        let mut called = false;
        for part in parts.into_iter().flatten() {
            let text = |key| part.get(key).and_then(Value::as_str);
            let Some(id) = text("toolCallId") else {
                continue;
            };
            match text("type") {
                Some("tool-call") => {
                    called = true;
                    let name = text("toolName").unwrap_or_default().to_owned();
                    let args = part.get("args").cloned().unwrap_or(Value::Null);
                    self.calls.entry(id.to_owned()).or_insert((name, args));
                }
                Some("tool-result") => {
                    let result = part.get("result").cloned().unwrap_or(Value::Null);
                    self.results.entry(id.to_owned()).or_insert(result);
                }
                _ => {}
            }
        }
        let role = message.get("role").and_then(Value::as_str);
        let request = message
            .pointer("/providerOptions/cursor/requestId")
            .and_then(Value::as_str);
        match (role, request) {
            (Some("user"), Some(request)) => {
                let finished = self.newest.take().unwrap_or(false);
                self.ends.entry(request.to_owned()).or_insert(finished);
            }
            _ => {
                self.newest
                    .get_or_insert(role == Some("assistant") && !called);
            }
        }
        Ok(true)
    }
}
