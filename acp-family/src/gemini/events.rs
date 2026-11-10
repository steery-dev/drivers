//! One Gemini chat file, `session-<time>-<id>.jsonl`, read as Steery's events.
//!
//! Gemini only appends to the file. Its first line is the conversation's metadata. Each
//! later line is one of these:
//! - a message `{id, timestamp, type: user|gemini|info|error|warning, content, …}`;
//! - `{$set}`, which changes the metadata;
//! - `{$patch: {updates, removeIds, orderIds}}`, which rewrites, removes or reorders
//!   messages;
//! - `{$rewindTo: id}`, which drops that message and every one after it.
//!
//! Gemini writes a message again, under the same id, as it learns more of it: a `gemini`
//! message first with its text, again with its tokens, and again with its tool calls once
//! they ended. Only the newest message is ever written again. So the cursor is the offset
//! after the last whole line, and a small state: the newest message and what was already
//! said of it. A message said again gives only what is new in it. Thus the read never holds
//! a line back, and each event is said once.
//!
//! The file does not say where a turn ends. A turn ends `finished` where its newest message
//! is an answer with text and no tool calls, and the file stood still for [`SETTLE`]. It
//! also ends where the next person prompt begins. Gemini writes an answer's text before its
//! tools run, so a tool that runs longer than [`SETTLE`] can end its turn early: the records
//! after it then open a turn of their own. A hook's `AfterAgent` says the turn's end at once.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufReader, Read as _, Seek as _, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_driver_common::lines::{self, BATCH, Line, RECORD};
use steery_sdk::wire::harness::{
    Call, Clip, Compactor, Count, Event, Events, Execution, Group, TurnState, Voice,
};
use steery_sdk::{Error, refusal};

/// How long the file stands still before its last answer is its turn's end.
pub const SETTLE: Duration = Duration::from_secs(60);

/// The text that opens Gemini's own context, its first `user` message.
const CONTEXT: &str = "<session_context>";
/// The text that opens the summary that replaces the context at a compression.
const SNAPSHOT: &str = "<state_snapshot>";
/// The text that a hook adds beside a person's prompt.
const HOOK_CONTEXT: &str = "<hook_context>";
/// The prompt that Gemini sends on its own to go on after a pause (`client.ts`).
const CONTINUE: &str = "Please continue.";
/// The tool whose success ends a sub-agent's run.
const COMPLETE_TASK: &str = "complete_task";
/// The tool that starts a sub-agent.
const INVOKE_AGENT: &str = "invoke_agent";
/// How many messages that say nothing a cursor keeps.
const QUIET: usize = 4;

/// Where a read stands: the file it was, by device and inode, the offset after the last
/// whole line read, and what the lines so far carry on.
#[derive(Serialize, Deserialize)]
struct Cursor {
    v: u8,
    dev: u64,
    ino: u64,
    off: u64,
    #[serde(default)]
    st: State,
}

impl Cursor {
    fn parse(cursor: &str) -> Result<Self, Error> {
        serde_json::from_str::<Self>(cursor)
            .ok()
            .filter(|cursor| cursor.v == 1)
            .ok_or_else(|| {
                Error::invalid(format!("this driver did not give the cursor `{cursor}`"))
            })
    }

    fn said(&self) -> String {
        serde_json::to_string(self).expect("a cursor is JSON")
    }
}

/// What the lines read so far carry on to the next.
#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    /// The person prompt whose turn began and did not end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    open: Option<String>,
    /// The person prompts on the current history, oldest first: a rewind counts those it
    /// drops.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    prompts: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// The newest message, and what was said of it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<Last>,
    /// A compression's summary was the newest record: a `gemini` message right after it
    /// without tokens or calls is Gemini's own acknowledgement of it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    snapshot: bool,
    /// The newest messages that say nothing (a summary and its acknowledgement), which
    /// Gemini may write again: at most [`QUIET`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    quiet: Vec<String>,
}

/// The newest message, and what was said of it.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Last {
    id: String,
    /// When the message was written.
    at: u64,
    /// The message is an answer with text and no tool calls: its turn may end with it.
    #[serde(default)]
    answer: bool,
    /// The message ended its conversation's run (a sub-agent's `complete_task`).
    #[serde(default)]
    done: bool,
    #[serde(default)]
    words: bool,
    #[serde(default)]
    thoughts: usize,
    /// The tokens last said of it: input, cached, output, reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tokens: Option<[u64; 4]>,
    /// The calls said of it.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    calls: BTreeSet<String>,
}

/// The events of `path` after `cursor`, or of its newest `tail` bytes.
pub fn read(path: &Path, cursor: Option<&str>, tail: Option<u64>) -> Result<Events, Error> {
    let unreadable = |e: std::io::Error| {
        Error::refused(
            refusal::REFUSED,
            format!("cannot read {}: {e}", path.display()),
        )
    };
    let open = File::open(path).map_err(unreadable)?;
    let meta = open.metadata().map_err(unreadable)?;
    let (dev, ino) = identity(&meta);
    let length = meta.len();
    let still = meta
        .modified()
        .ok()
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .is_some_and(|age| age >= SETTLE);
    let mut reset = false;
    let (start, state) = match (tail, cursor) {
        (Some(tail), _) => {
            let from = length.saturating_sub(tail.min(BATCH));
            (
                lines::first_whole(&open, from, length).map_err(unreadable)?,
                State::default(),
            )
        }
        (None, Some(cursor)) => {
            let cursor = Cursor::parse(cursor)?;
            if cursor.dev == dev && cursor.ino == ino && cursor.off <= length {
                (cursor.off, cursor.st)
            } else {
                reset = true;
                (0, State::default())
            }
        }
        (None, None) => (0, State::default()),
    };
    let mut fold = Fold {
        state,
        events: Vec::new(),
    };
    // An older release's chat file is one JSON document, which no release writes on.
    if path.extension().is_some_and(|extension| extension == "json") {
        let unread = tail.is_some() || start < length;
        if unread {
            if length > RECORD as u64 {
                return Err(Error::refused(
                    refusal::REFUSED,
                    format!("{} is longer than {RECORD} bytes", path.display()),
                ));
            }
            let bytes = lines::bytes(&open, 0, length).map_err(unreadable)?;
            let document: Value = serde_json::from_slice(&bytes).map_err(|e| {
                Error::refused(
                    refusal::REFUSED,
                    format!("cannot read {}: {e}", path.display()),
                )
            })?;
            fold.state = State::default();
            for message in document["messages"].as_array().into_iter().flatten() {
                fold.record(message);
            }
            fold.settle();
        }
        return Ok(Events {
            cursor: Cursor {
                v: 1,
                dev,
                ino,
                off: length,
                st: fold.state,
            }
            .said(),
            events: fold.events,
            more: false,
            reset,
            whole: unread,
            after: None,
        });
    }
    let mut at = start;
    let mut more = false;
    (&open).seek(SeekFrom::Start(start)).map_err(unreadable)?;
    let total = length - start;
    let mut reader = BufReader::new((&open).take(total));
    let mut line = Vec::new();
    loop {
        if at - start >= BATCH {
            more = true;
            break;
        }
        let kind = lines::capped(&mut reader, &mut line, RECORD).map_err(unreadable)?;
        let end = start + lines::consumed(&reader, total);
        match kind {
            Line::Partial => break,
            Line::Long => {}
            Line::Whole => {
                if let Ok(record) = serde_json::from_slice::<Value>(&line) {
                    fold.record(&record);
                }
            }
        }
        at = end;
    }
    if still && !more && at == length {
        fold.settle();
    }
    Ok(Events {
        cursor: Cursor {
            v: 1,
            dev,
            ino,
            off: at,
            st: fold.state,
        }
        .said(),
        events: fold.events,
        more,
        reset,
        whole: start == 0,
        after: None,
    })
}

#[cfg(unix)]
fn identity(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt as _;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn identity(_: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// The lines of one read, folded into events.
struct Fold {
    state: State,
    events: Vec<Event>,
}

impl Fold {
    fn record(&mut self, record: &Value) {
        if let Some(id) = record["$rewindTo"].as_str() {
            self.rewind(id);
            return;
        }
        if let Some(patch) = record.get("$patch") {
            self.patch(patch);
            return;
        }
        if record.get("$set").is_some() {
            return;
        }
        let (Some(id), Some(kind)) = (record["id"].as_str(), record["type"].as_str()) else {
            return;
        };
        let at = record["timestamp"].as_str().and_then(millis).unwrap_or(0);
        match kind {
            "user" => self.user(id, at, record),
            "gemini" => self.gemini(id, at, record),
            // `info`, `error` and `warning` are what the terminal showed the person; an
            // error that ends a turn also rewinds it, which says it.
            _ => {}
        }
    }

    /// A `user` message: a person's prompt, a compression's summary, or what Gemini sends
    /// on its own (its context, a tool's response, a prompt to go on).
    fn user(&mut self, id: &str, at: u64, record: &Value) {
        if self.state.quiet.iter().any(|quiet| quiet == id)
            || self.state.last.as_ref().is_some_and(|last| last.id == id)
        {
            return;
        }
        if let Some(summary) = text(&record["content"])
            .filter(|text| text.trim_start().starts_with(SNAPSHOT))
        {
            // A turn whose newest message is an answer is over before the compression; a
            // turn compressed in its middle goes on.
            if self.ended() == TurnState::Finished {
                self.end(TurnState::Finished);
            }
            self.quiet(id);
            self.state.snapshot = true;
            self.events.push(Event::Compacted {
                at,
                id: id.to_owned(),
                by: Compactor::Harness { reason: None },
                tokens_before: None,
                tokens_after: None,
            });
            self.events.push(Event::Words {
                at,
                id: id.to_owned(),
                voice: Voice::Summary,
                text: summary,
                message: None,
                call: None,
                clips: Vec::new(),
            });
            return;
        }
        let Some((said, clips)) = person(record) else {
            return;
        };
        self.state.snapshot = false;
        self.end(self.ended());
        self.state.open = Some(id.to_owned());
        self.state.prompts.push(id.to_owned());
        self.state.last = Some(Last {
            id: id.to_owned(),
            at,
            ..Last::default()
        });
        self.events.push(Event::TurnBegan {
            at,
            turn: Some(id.to_owned()),
        });
        self.events.push(Event::Words {
            at,
            id: id.to_owned(),
            voice: Voice::Person,
            text: said,
            message: None,
            call: None,
            clips,
        });
    }

    /// A `gemini` message, new or said again: only what is new in it is said.
    fn gemini(&mut self, id: &str, at: u64, record: &Value) {
        if self.state.quiet.iter().any(|quiet| quiet == id) {
            return;
        }
        // Gemini's acknowledgement of a compression's summary, right after it, is not the
        // agent's: it has no tokens and no calls.
        if std::mem::take(&mut self.state.snapshot)
            && record.get("tokens").is_none_or(Value::is_null)
            && record.get("toolCalls").is_none_or(Value::is_null)
        {
            self.quiet(id);
            return;
        }
        let mut last = match self.state.last.take() {
            Some(last) if last.id == id => last,
            _ => Last {
                id: id.to_owned(),
                at,
                ..Last::default()
            },
        };
        // A record of a turn whose end was already said opens a turn of its own.
        if self.state.open.is_none() {
            self.state.open = Some(id.to_owned());
            self.events.push(Event::TurnBegan {
                at,
                turn: Some(id.to_owned()),
            });
        }
        if let Some(model) = record["model"].as_str()
            && self.state.model.as_deref() != Some(model)
        {
            self.state.model = Some(model.to_owned());
            self.events.push(Event::Model {
                at,
                model: model.to_owned(),
                provider: None,
                thinking: None,
            });
        }
        let thoughts = record["thoughts"].as_array().map(Vec::as_slice).unwrap_or_default();
        for (index, thought) in thoughts.iter().enumerate().skip(last.thoughts) {
            let subject = thought["subject"].as_str().unwrap_or_default().trim();
            let description = thought["description"].as_str().unwrap_or_default().trim();
            let text = match (subject.is_empty(), description.is_empty()) {
                (true, true) => continue,
                (false, true) => format!("**{subject}**"),
                (true, false) => description.to_owned(),
                (false, false) => format!("**{subject}**\n\n{description}"),
            };
            self.events.push(Event::Words {
                at: thought["timestamp"].as_str().and_then(millis).unwrap_or(at),
                id: format!("{id}#thought-{index}"),
                voice: Voice::Thought,
                text,
                message: None,
                call: None,
                clips: Vec::new(),
            });
        }
        last.thoughts = last.thoughts.max(thoughts.len());
        let said = text(&record["content"]).unwrap_or_default();
        if !last.words && !said.trim().is_empty() {
            last.words = true;
            self.events.push(Event::Words {
                at,
                id: id.to_owned(),
                voice: Voice::Agent,
                text: said.clone(),
                message: None,
                call: None,
                clips: Vec::new(),
            });
        }
        if let Some(tokens) = tokens(&record["tokens"])
            && last.tokens != Some(tokens)
        {
            last.tokens = Some(tokens);
            let [input, cached, output, reasoning] = tokens;
            self.events.push(Event::Tokens {
                at,
                count: Count::Response,
                response: Some(id.to_owned()),
                model: record["model"].as_str().map(str::to_owned),
                input,
                cached,
                cache_write: None,
                output,
                reasoning,
                context_left: None,
            });
        }
        let calls = record["toolCalls"].as_array().map(Vec::as_slice).unwrap_or_default();
        for recorded in calls {
            let Some(call_id) = recorded["id"].as_str() else {
                continue;
            };
            if !last.calls.insert(call_id.to_owned()) {
                continue;
            }
            self.call(at, recorded);
            if recorded["name"] == COMPLETE_TASK && recorded["status"] == "success" {
                last.done = true;
            }
        }
        last.answer = calls.is_empty() && !said.trim().is_empty();
        self.state.last = Some(last);
        if self.state.last.as_ref().is_some_and(|last| last.done) {
            self.end(TurnState::Finished);
        }
    }

    /// A call that ended: Gemini writes a call once it is over.
    fn call(&mut self, at: u64, recorded: &Value) {
        let id = recorded["id"].as_str().unwrap_or_default();
        let name = recorded["name"].as_str().unwrap_or_default();
        let ended = recorded["timestamp"].as_str().and_then(millis).unwrap_or(at);
        let status = recorded["status"].as_str().unwrap_or_default();
        let mut began = call(
            id,
            name,
            recorded.get("args").cloned().unwrap_or(Value::Null),
            None,
        );
        began.id = id.to_owned();
        self.events.push(Event::CallBegan {
            at,
            call: began,
        });
        let output = output(recorded);
        self.events.push(Event::CallEnded {
            at: ended,
            id: id.to_owned(),
            ok: status == "success",
            chars: output.chars().count() as u64,
            tail: tail(&output),
            waited: Vec::new(),
        });
        if !output.trim().is_empty() {
            self.events.push(Event::Words {
                at: ended,
                id: format!("{id}#result"),
                voice: Voice::Tool,
                text: output.clone(),
                message: None,
                call: Some(id.to_owned()),
                clips: Vec::new(),
            });
        }
        if name == INVOKE_AGENT
            && let Some(agent) = recorded["agentId"].as_str()
        {
            let state = recorded["resultDisplay"]["state"]
                .as_str()
                .and_then(execution)
                .or_else(|| execution(status));
            self.events.push(Event::SubAgent {
                at: ended,
                external_id: agent.to_owned(),
                call: Some(id.to_owned()),
                state,
                error: (state == Some(Execution::Failed))
                    .then(|| output.lines().next().unwrap_or_default().to_owned())
                    .filter(|line| !line.is_empty()),
            });
        }
    }

    /// `$rewindTo`: the message and every one after it are gone. Where that drops person
    /// prompts, the open turn was cut short, and the conversation took those turns back.
    /// A rewind to an answer alone is a retry inside the turn.
    fn rewind(&mut self, id: &str) {
        let Some(from) = self.state.prompts.iter().position(|prompt| prompt == id) else {
            if self.state.last.as_ref().is_some_and(|last| last.id == id) {
                self.state.last = None;
            }
            return;
        };
        let at = self.state.last.as_ref().map_or(0, |last| last.at);
        let turns = self.state.prompts.len() - from;
        self.state.prompts.truncate(from);
        self.end(TurnState::Interrupted);
        self.state.last = None;
        self.events.push(Event::Rollback {
            at,
            turns: u32::try_from(turns).unwrap_or(u32::MAX),
        });
    }

    /// `$patch`: messages it removes leave the history; a rewrite of an older message
    /// changes nothing that was said.
    fn patch(&mut self, patch: &Value) {
        let removed: BTreeSet<&str> = patch["removeIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        self.state
            .prompts
            .retain(|prompt| !removed.contains(prompt.as_str()));
    }

    /// A message that says nothing, if it is written again.
    fn quiet(&mut self, id: &str) {
        let quiet = &mut self.state.quiet;
        if quiet.len() == QUIET {
            quiet.remove(0);
        }
        quiet.push(id.to_owned());
    }

    /// How the open turn ends where the person prompts again: finished on an answer.
    fn ended(&self) -> TurnState {
        if self.state.last.as_ref().is_some_and(|last| last.answer) {
            TurnState::Finished
        } else {
            TurnState::Interrupted
        }
    }

    /// The open turn ends, at its newest message.
    fn end(&mut self, state: TurnState) {
        if let Some(turn) = self.state.open.take() {
            self.events.push(Event::TurnEnded {
                at: self.state.last.as_ref().map_or(0, |last| last.at),
                turn: Some(turn),
                state,
                failure: None,
            });
        }
    }

    /// The file stood still: an open turn whose newest message is an answer ended there.
    fn settle(&mut self) {
        if self.state.last.as_ref().is_some_and(|last| last.answer) {
            self.end(TurnState::Finished);
        }
    }
}

/// What a person said in a `user` message, and what came with it; none where the message
/// is not a person's: Gemini's context, a tool's response, or its own prompt to go on.
pub(super) fn person(record: &Value) -> Option<(String, Vec<Clip>)> {
    let content = &record["content"];
    let parts = parts(content);
    if parts.iter().any(|part| part.get("functionResponse").is_some()) {
        return None;
    }
    let written = text(content).unwrap_or_default();
    let shown = record
        .get("displayContent")
        .filter(|shown| !shown.is_null())
        .and_then(text);
    // Gemini records its own "Please continue." with the person's first prompt shown.
    if written.trim() == CONTINUE && shown.as_deref().is_some_and(|shown| shown.trim() != CONTINUE)
    {
        return None;
    }
    let said = shown.unwrap_or(written);
    let said = said.trim();
    if said.starts_with(CONTEXT) || said.starts_with(SNAPSHOT) {
        return None;
    }
    let clips: Vec<Clip> = parts.iter().filter_map(|part| clip(part)).collect();
    (!said.is_empty() || !clips.is_empty()).then(|| (said.to_owned(), clips))
}

/// The parts of a message's content: a string is one text part.
fn parts(content: &Value) -> Vec<Value> {
    match content {
        Value::Array(parts) => parts.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

/// The text of a message's content, its parts joined; a thought or a hook's added context
/// is left out.
fn text(content: &Value) -> Option<String> {
    let parts = parts(content);
    let texts: Vec<&str> = parts
        .iter()
        .filter_map(|part| match part {
            Value::String(text) => Some(text.as_str()),
            Value::Object(_) if part["thought"] == true => None,
            Value::Object(_) => part["text"].as_str(),
            _ => None,
        })
        .filter(|text| !text.trim_start().starts_with(HOOK_CONTEXT))
        .collect();
    (!texts.is_empty()).then(|| texts.concat())
}

/// An image, audio or file that a person's message carried inline.
fn clip(part: &Value) -> Option<Clip> {
    let inline = part.get("inlineData")?;
    Some(Clip::Bytes {
        data: inline["data"].as_str()?.to_owned(),
        media_type: inline["mimeType"].as_str().map(str::to_owned),
        name: inline["displayName"].as_str().map(str::to_owned),
    })
}

/// A response's tokens as Steery counts them: input (with the cached part and the tools'
/// prompt), cached, output (with the thinking), and the thinking.
fn tokens(tokens: &Value) -> Option<[u64; 4]> {
    let count = |key: &str| tokens[key].as_u64().unwrap_or(0);
    tokens.as_object()?;
    Some([
        count("input") + count("tool"),
        count("cached"),
        count("output") + count("thoughts"),
        count("thoughts"),
    ])
}

/// The text of a call's result: what the model was given of it.
fn output(recorded: &Value) -> String {
    let responses: Vec<String> = parts(&recorded["result"])
        .iter()
        .filter_map(|part| {
            let response = &part["functionResponse"]["response"];
            response["output"]
                .as_str()
                .or_else(|| response["error"].as_str())
                .or_else(|| response["error"]["message"].as_str())
                .map(str::to_owned)
        })
        .collect();
    if !responses.is_empty() {
        return responses.join("\n");
    }
    recorded["resultDisplay"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The call named `name`, with its group. Gemini names an MCP tool `mcp_<server>_<tool>`;
/// where `server` is not given, the server is the part up to the next `_`.
pub(super) fn call(id: &str, name: &str, input: Value, server: Option<&str>) -> Call {
    let served = match server {
        Some(server) => name
            .strip_prefix("mcp_")
            .and_then(|rest| rest.strip_prefix(server))
            .and_then(|rest| rest.strip_prefix('_'))
            .map(|tool| (server, tool))
            .or(Some((server, name))),
        None => name.strip_prefix("mcp_").and_then(|rest| rest.split_once('_')),
    };
    let (tool, group, server, steery) = match served {
        Some(("steery", tool)) => (
            format!("mcp__steery__{tool}"),
            Group::Steery,
            None,
            Some(tool.to_owned()),
        ),
        Some((server, tool)) => (
            format!("mcp__{server}__{tool}"),
            Group::Mcp,
            Some(server.to_owned()),
            None,
        ),
        None => (name.to_owned(), group(name), None, None),
    };
    Call {
        id: id.to_owned(),
        tool,
        input,
        group,
        server,
        steery,
        asks: None,
        parent: None,
        response: None,
    }
}

/// The group of one of Gemini's own tools (`tool-names.ts`).
fn group(name: &str) -> Group {
    match name {
        "run_shell_command" => Group::Shell,
        "read_file" | "read_many_files" | "list_directory" | "glob" => Group::FilesRead,
        "write_file" | "replace" => Group::FilesEdit,
        "grep_search" | "search_file_content" => Group::Search,
        "google_web_search" | "web_fetch" => Group::Web,
        "invoke_agent" | COMPLETE_TASK => Group::Agents,
        "enter_plan_mode" | "exit_plan_mode" | "write_todos" | "update_topic" => Group::Plan,
        "ask_user" => Group::Ask,
        "save_memory" => Group::Memory,
        "take_snapshot" => Group::Browser,
        "activate_skill" | "get_internal_docs" | "list_mcp_resources" | "read_mcp_resource" => {
            Group::Harness
        }
        name if name.starts_with("tracker_") => Group::Plan,
        _ => Group::Custom,
    }
}

/// The last 8 lines of a result.
fn tail(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let kept = lines[lines.len().saturating_sub(8)..].join("\n");
    (!kept.trim().is_empty()).then_some(kept)
}

/// A sub-agent's state, by its `SubagentState` or its call's status.
fn execution(status: &str) -> Option<Execution> {
    match status {
        "completed" | "success" => Some(Execution::Finished),
        "error" => Some(Execution::Failed),
        "cancelled" => Some(Execution::Interrupted),
        "running" => Some(Execution::Running),
        _ => None,
    }
}

/// Milliseconds since 1970 of an RFC 3339 time.
pub fn millis(at: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}

/// Milliseconds since 1970 of a file's last change.
pub fn changed(path: &Path) -> Option<u64> {
    let at = std::fs::metadata(path).ok()?.modified().ok()?;
    u64::try_from(at.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}
