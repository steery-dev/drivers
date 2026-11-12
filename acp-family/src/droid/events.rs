//! One Droid session file, `<id>.jsonl`, read as Steery's events.
//!
//! Droid appends one JSON record per line:
//! - `session_start` first: `id`, `title`, `cwd`, and in a fork `parent` (the conversation
//!   it forks) and `forkedAtMessageId` (the newest record of the history it copied);
//! - `message` records, `{id, parentId, timestamp, message: {role, content, visibility}}`,
//!   whose `content` is `text`, `thinking`, `tool_use` and `tool_result` blocks;
//! - `agent_turn_outcome` after each turn, `{turnId, reason}`, where `turnId` is the id of the
//!   prompt that began the turn. It has no id and no parent.
//!
//! What is not the person's words: an `llm_only` message is context Droid gives the model (the
//! tool catalog, the folder's `ls` and `git status`, `Continue.` reminders); a `user_only`
//! message is a notice for the terminal: a hook that ran (it names `hookEventName`), or why a
//! turn failed ("Unable to reach …"). A person's prompt is the `user` message with text and
//! neither visibility.
//!
//! A turn whose outcome Droid never wrote was stopped: it ends `interrupted` where the next
//! prompt begins, or where a `SessionEnd` hook's notice says the process ended.

use serde_json::Value;
use steery_sdk::wire::harness::{Call, Event, Failure, FailureKind, Group, TurnState, Voice};

/// Steery's MCP server, by the name setup gives it.
pub(crate) const STEERY: &str = "steery";

/// How Droid names an MCP server's tool: `<server>___<tool>`.
const MCP_SEPARATOR: &str = "___";

/// How many of a result's last lines `call_ended` carries.
const TAIL_LINES: usize = 8;

/// What `<id>.settings.json` beside the file says of the model.
#[derive(Default)]
pub struct Facts {
    pub model: Option<String>,
    pub provider: Option<String>,
}

/// An event, and the bytes of the record it came from.
pub struct Said {
    pub start: u64,
    pub end: u64,
    pub event: Event,
}

/// The file as events, and what its end leaves open.
#[derive(Default)]
pub struct Parsed {
    pub said: Vec<Said>,
    /// The turn that has begun and not ended.
    pub open: Option<String>,
    /// The id of the newest record with one.
    pub newest: Option<String>,
    /// When the first message was written.
    pub first: Option<u64>,
    /// When the newest message was written.
    pub at: u64,
    /// The length of the whole records.
    pub len: u64,
}

/// Each whole record of `bytes` as events. A last line with no newline yet is not read.
pub fn parse(bytes: &[u8], facts: &Facts) -> Parsed {
    let mut reading = Reading {
        facts,
        parsed: Parsed::default(),
        notice: None,
        fork: None,
        forked: false,
        start: 0,
        end: 0,
    };
    let mut start = 0;
    for end in memchr::memchr_iter(b'\n', bytes) {
        let line = &bytes[start..end];
        reading.start = start as u64;
        reading.end = end as u64 + 1;
        if let Ok(record) = serde_json::from_slice::<Value>(line) {
            reading.record(&record);
        }
        start = end + 1;
    }
    reading.parsed.len = start as u64;
    if let Some((parent, _)) = reading.fork.take().filter(|_| reading.forked) {
        let at = reading.parsed.at;
        reading.push(Event::Fork { at, parent });
    }
    reading.parsed
}

struct Reading<'a> {
    facts: &'a Facts,
    parsed: Parsed,
    /// The newest `user_only` notice that is no hook's: why a turn failed.
    notice: Option<String>,
    /// The conversation this one forks, and the newest record it copied.
    fork: Option<(String, String)>,
    /// The copied history is read: the next message is this conversation's own.
    forked: bool,
    start: u64,
    end: u64,
}

impl Reading<'_> {
    fn push(&mut self, event: Event) {
        self.parsed.said.push(Said {
            start: self.start,
            end: self.end,
            event,
        });
    }

    fn record(&mut self, record: &Value) {
        match record["type"].as_str() {
            Some("session_start") => {
                if let (Some(parent), Some(at)) = (
                    record["parent"].as_str(),
                    record["forkedAtMessageId"].as_str(),
                ) {
                    self.fork = Some((parent.to_owned(), at.to_owned()));
                }
            }
            Some("message") => self.message(record),
            Some("agent_turn_outcome") => self.outcome(record),
            _ => {}
        }
    }

    fn message(&mut self, record: &Value) {
        let Some(id) = record["id"].as_str() else {
            return;
        };
        if let Some(at) = record["timestamp"].as_str().and_then(millis) {
            self.parsed.at = at;
            self.parsed.first.get_or_insert(at);
        }
        if self.forked
            && let Some((parent, _)) = self.fork.take()
        {
            let at = self.parsed.at;
            self.push(Event::Fork { at, parent });
        }
        self.parsed.newest = Some(id.to_owned());
        let message = &record["message"];
        let blocks = message["content"].as_array().map_or(&[][..], Vec::as_slice);
        match (message["role"].as_str(), message["visibility"].as_str()) {
            (_, Some("llm_only")) => {}
            (Some("user"), Some("user_only")) => self.notice(message, blocks),
            (Some("user"), None) => self.user(id, blocks),
            (Some("assistant"), None) => self.assistant(id, blocks),
            _ => {}
        }
        if self.fork.as_ref().is_some_and(|(_, at)| at == id) {
            self.forked = true;
        }
    }

    /// A notice for the terminal: the process ended, or why a turn failed.
    fn notice(&mut self, message: &Value, blocks: &[Value]) {
        match message["hookEventName"].as_str() {
            Some("SessionEnd") => {
                if let Some(turn) = self.parsed.open.take() {
                    self.ended(turn, TurnState::Interrupted, None);
                }
            }
            Some(_) => {}
            None => {
                let text = texts(blocks);
                if !text.is_empty() {
                    self.notice = Some(text);
                }
            }
        }
    }

    /// The person's prompt, or the results of the agent's calls.
    fn user(&mut self, id: &str, blocks: &[Value]) {
        let at = self.parsed.at;
        for block in blocks.iter().filter(|block| block["type"] == "tool_result") {
            let Some(call) = block["tool_use_id"].as_str() else {
                continue;
            };
            self.push(call_ended(
                at,
                call,
                block["is_error"] != true,
                &result_text(&block["content"]),
            ));
        }
        let text = texts(blocks);
        if text.is_empty() {
            return;
        }
        if let Some(turn) = self.parsed.open.take() {
            self.ended(turn, TurnState::Interrupted, None);
        }
        self.notice = None;
        self.push(Event::TurnBegan {
            at,
            turn: Some(id.to_owned()),
        });
        self.push(Event::Words {
            at,
            id: id.to_owned(),
            voice: Voice::Person,
            text,
            message: None,
            call: None,
            clips: Vec::new(),
        });
        self.parsed.open = Some(id.to_owned());
    }

    fn assistant(&mut self, id: &str, blocks: &[Value]) {
        let at = self.parsed.at;
        for (index, block) in blocks.iter().enumerate() {
            let words = |voice, text: &str| Event::Words {
                at,
                id: format!("{id}:{index}"),
                voice,
                text: text.to_owned(),
                message: Some(id.to_owned()),
                call: None,
                clips: Vec::new(),
            };
            let event = match block["type"].as_str() {
                Some("text") => block["text"]
                    .as_str()
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| words(Voice::Agent, text)),
                Some("thinking") => block["thinking"]
                    .as_str()
                    .filter(|text| !text.trim().is_empty())
                    .map(|text| words(Voice::Thought, text)),
                Some("tool_use") => match (block["id"].as_str(), block["name"].as_str()) {
                    (Some(call), Some(tool)) => Some(Event::CallBegan {
                        at,
                        call: self::call(call, tool, block["input"].clone(), Some(id)),
                    }),
                    _ => None,
                },
                _ => None,
            };
            if let Some(event) = event {
                self.push(event);
            }
        }
    }

    /// The end of the turn its prompt began, as Droid said it.
    fn outcome(&mut self, record: &Value) {
        let Some(turn) = record["turnId"].as_str() else {
            return;
        };
        if self.parsed.open.as_deref() != Some(turn) {
            return;
        }
        self.parsed.open = None;
        let reason = record["reason"].as_str().unwrap_or("");
        let (state, failure) = match reason {
            "completed" => (TurnState::Finished, None),
            // Its person said no to a call, and the turn stopped there.
            "permission_rejected" => (TurnState::Interrupted, None),
            _ => (
                TurnState::Failed,
                Some(Failure {
                    kind: FailureKind::Other,
                    line: self
                        .notice
                        .take()
                        .and_then(|notice| notice.lines().next().map(str::to_owned))
                        .unwrap_or_else(|| reason.to_owned()),
                    service: false,
                    http_status: None,
                    code: (!reason.is_empty()).then(|| reason.to_owned()),
                    retry_at: None,
                    provider: self.facts.provider.clone(),
                    model: self.facts.model.clone(),
                    thinking: None,
                }),
            ),
        };
        self.ended(turn.to_owned(), state, failure);
    }

    fn ended(&mut self, turn: String, state: TurnState, failure: Option<Failure>) {
        let at = self.parsed.at;
        self.push(Event::TurnEnded {
            at,
            turn: Some(turn),
            state,
            failure,
        });
    }
}

/// The end of call `id`, with what it gave back.
pub(crate) fn call_ended(at: u64, id: &str, ok: bool, text: &str) -> Event {
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    Event::CallEnded {
        at,
        id: id.to_owned(),
        ok,
        chars: text.chars().count() as u64,
        tail: (!tail.is_empty()).then_some(tail),
        waited: Vec::new(),
    }
}

/// A call to `tool`: an MCP server's tool is `<server>___<tool>`.
pub(crate) fn call(id: &str, tool: &str, input: Value, response: Option<&str>) -> Call {
    let server = tool
        .split_once(MCP_SEPARATOR)
        .filter(|(server, tool)| !server.is_empty() && !tool.is_empty())
        .map(|(server, _)| server.to_owned());
    let steery = match &server {
        Some(name) if name == STEERY => tool
            .strip_prefix(STEERY)
            .and_then(|tail| tail.strip_prefix(MCP_SEPARATOR))
            .map(str::to_owned),
        _ => None,
    };
    let group = match (&server, &steery) {
        (_, Some(_)) => Group::Steery,
        (Some(_), None) => Group::Mcp,
        (None, _) => group(tool),
    };
    Call {
        id: id.to_owned(),
        tool: tool.to_owned(),
        input,
        group,
        server,
        steery,
        asks: None,
        parent: None,
        response: response.map(str::to_owned),
    }
}

/// Droid's own tools, by the names its 1.9.0 executable holds.
fn group(tool: &str) -> Group {
    match tool {
        "Execute" | "WaitForScript" => Group::Shell,
        "Read" | "LS" => Group::FilesRead,
        "Edit" | "MultiEdit" | "Create" | "ApplyPatch" => Group::FilesEdit,
        "Grep" | "Glob" => Group::Search,
        "WebSearch" | "FetchUrl" => Group::Web,
        "Task" | "TaskOutput" | "TaskStop" | "GenerateDroid" => Group::Agents,
        "TodoWrite" | "ExitSpecMode" => Group::Plan,
        "AskUser" => Group::Ask,
        "ToolSearch" | "Skill" => Group::Harness,
        _ => Group::Custom,
    }
}

/// The text blocks of a message, joined.
fn texts(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// A call's result: text, or text blocks.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => texts(blocks),
        _ => String::new(),
    }
}

/// Milliseconds since 1970 of an RFC 3339 time.
pub(crate) fn millis(at: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}
