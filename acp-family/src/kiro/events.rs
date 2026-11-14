//! One Kiro conversation file, `messages.jsonl`, read as Steery's events.
//!
//! Kiro 3.3.0 appends one JSON record per line, `{id, timestamp, payload}`, where
//! `payload.type` is one of:
//! - `user`: the person's prompt, `content` its text;
//! - `turn_start` and `turn_end` around each turn, both naming it by `turnId`; `turn_end`
//!   says why it ended (`stopReason`);
//! - `assistant`: the agent's words, `operationType` `Say`, or `Reasoning` for its thinking;
//! - `tool_call` (`toolCallId`, `toolName`, `args`, ACP `kind`) and `tool_result`
//!   (`toolCallId`, `content`, `success`);
//! - `sub_agent_start` and `sub_agent_complete`, naming a sub-agent by `subSessionId`, whose
//!   own records are `sub-executions/<subSessionId>.jsonl` beside the file;
//! - what is not the person's or the agent's words: `session_start` (the system prompt),
//!   `agent_note` (context for the model), `session_metadata`, `usage_summary`,
//!   `session_event`, `pending_interaction`, `interaction_resolved`, and the hooks that ran
//!   (`ContextualHookInvoked`).
//!
//! A turn that began and never ended was stopped: it ends `interrupted` where the next
//! prompt begins.
use serde_json::Value;
use steery_sdk::wire::harness::{
    Call, Event, Execution, Failure, FailureKind, Group, TurnState, Voice,
};

/// Steery's MCP server, by the name setup gives it.
pub(crate) const STEERY: &str = "steery";

/// How Kiro names an MCP server's tool: `<server>/<tool>`.
const MCP_SEPARATOR: char = '/';

/// How many of a result's last lines `call_ended` carries.
const TAIL_LINES: usize = 8;

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
    /// The id of the newest record.
    pub newest: Option<String>,
    /// When the first record was written.
    pub first: Option<u64>,
    /// When the newest record was written.
    pub at: u64,
    /// The length of the whole records.
    pub len: u64,
}

/// Each whole record of `bytes` as events; `fork` is the conversation this one forks and
/// the newest record of the history it copied (`session.json`'s `parentSessionId` and
/// `forkedAtMessageId`). A last line with no newline yet is not read.
pub fn parse(bytes: &[u8], fork: Option<(String, String)>) -> Parsed {
    let mut reading = Reading {
        parsed: Parsed::default(),
        fork,
        forked: false,
        start: 0,
        end: 0,
    };
    let mut start = 0;
    for end in memchr::memchr_iter(b'\n', bytes) {
        reading.start = start as u64;
        reading.end = end as u64 + 1;
        if let Ok(record) = serde_json::from_slice::<Value>(&bytes[start..end]) {
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

struct Reading {
    parsed: Parsed,
    fork: Option<(String, String)>,
    /// The copied history is read: the next record is this conversation's own.
    forked: bool,
    start: u64,
    end: u64,
}

impl Reading {
    fn push(&mut self, event: Event) {
        self.parsed.said.push(Said {
            start: self.start,
            end: self.end,
            event,
        });
    }

    fn record(&mut self, record: &Value) {
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
        let at = self.parsed.at;
        let payload = &record["payload"];
        let text = |key: &str| payload[key].as_str().unwrap_or_default();
        match payload["type"].as_str() {
            Some("user") => {
                if let Some(turn) = self.parsed.open.take() {
                    self.ended(turn, TurnState::Interrupted, None);
                }
                let words = text("content").trim();
                if !words.is_empty() {
                    self.push(words_of(at, id, Voice::Person, words));
                }
            }
            Some("turn_start") => {
                if let Some(turn) = self.parsed.open.take() {
                    self.ended(turn, TurnState::Interrupted, None);
                }
                let turn = text("turnId").to_owned();
                self.push(Event::TurnBegan {
                    at,
                    turn: Some(turn.clone()),
                });
                self.parsed.open = Some(turn);
            }
            Some("turn_end") => {
                let turn = text("turnId").to_owned();
                if self.parsed.open.as_deref() == Some(turn.as_str()) {
                    self.parsed.open = None;
                }
                let (state, failure) = outcome(payload["stopReason"].as_str());
                self.ended(turn, state, failure);
            }
            Some("assistant") => {
                let voice = match text("operationType") {
                    "Reasoning" => Voice::Thought,
                    _ => Voice::Agent,
                };
                let words = text("content").trim();
                if !words.is_empty() {
                    self.push(words_of(at, id, voice, words));
                }
            }
            Some("tool_call") => {
                let call = call(
                    text("toolCallId"),
                    text("toolName"),
                    payload["args"].clone(),
                    payload["kind"].as_str(),
                );
                self.push(Event::CallBegan { at, call });
            }
            Some("tool_result") => {
                let content = match &payload["content"] {
                    Value::String(text) => text.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                };
                self.push(call_ended(
                    at,
                    text("toolCallId"),
                    payload["success"] != false,
                    &content,
                ));
            }
            Some(kind @ ("sub_agent_start" | "sub_agent_complete")) => {
                let (state, error) = match (kind, text("status")) {
                    ("sub_agent_start", _) => (Execution::Running, None),
                    (_, "success") => (Execution::Finished, None),
                    (_, status) => (Execution::Failed, Some(status.to_owned())),
                };
                self.push(Event::SubAgent {
                    at,
                    external_id: text("subSessionId").to_owned(),
                    call: None,
                    state: Some(state),
                    error: error.filter(|error| !error.is_empty()),
                });
            }
            _ => {}
        }
        if self.fork.as_ref().is_some_and(|(_, at)| at == id) {
            self.forked = true;
        }
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

/// How a turn ended, by its ACP `stopReason`.
fn outcome(reason: Option<&str>) -> (TurnState, Option<Failure>) {
    match reason {
        Some("end_turn" | "max_tokens" | "max_turn_requests") | None => (TurnState::Finished, None),
        Some("cancelled") => (TurnState::Interrupted, None),
        Some(other) => (
            TurnState::Failed,
            Some(Failure {
                kind: FailureKind::Other,
                line: format!("Kiro ended the turn: {other}"),
                service: false,
                http_status: None,
                code: Some(other.to_owned()),
                retry_at: None,
                provider: None,
                model: None,
                thinking: None,
            }),
        ),
    }
}

fn words_of(at: u64, id: &str, voice: Voice, text: &str) -> Event {
    Event::Words {
        at,
        id: id.to_owned(),
        voice,
        text: text.to_owned(),
        message: None,
        call: None,
        clips: Vec::new(),
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

/// A call to `tool`, of ACP `kind` where Kiro names one. An MCP server's tool is
/// `<server>/<tool>`.
pub(crate) fn call(id: &str, tool: &str, input: Value, kind: Option<&str>) -> Call {
    let server = tool
        .split_once(MCP_SEPARATOR)
        .filter(|(server, tool)| !server.is_empty() && !tool.is_empty())
        .map(|(server, _)| server.to_owned());
    let steery = match &server {
        Some(name) if name == STEERY => tool
            .split_once(MCP_SEPARATOR)
            .map(|(_, tool)| tool.to_owned()),
        _ => None,
    };
    let group = match (&server, &steery) {
        (_, Some(_)) => Group::Steery,
        (Some(_), None) => Group::Mcp,
        (None, _) => group(tool, kind),
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
        response: None,
    }
}

/// The group of one of Kiro's own tools, by its name, else by its ACP kind.
fn group(tool: &str, kind: Option<&str>) -> Group {
    match tool {
        "execute_bash" | "run_command" => return Group::Shell,
        "read_file" | "fs_read" | "list_directory" => return Group::FilesRead,
        "fs_write" | "write_file" | "str_replace" | "edit_file" => return Group::FilesEdit,
        "use_subagent" => return Group::Agents,
        _ => {}
    }
    match kind {
        Some("execute") => Group::Shell,
        Some("read") => Group::FilesRead,
        Some("edit" | "delete" | "move") => Group::FilesEdit,
        Some("search") => Group::Search,
        Some("fetch") => Group::Web,
        Some("think") => Group::Plan,
        _ => Group::Custom,
    }
}

/// Milliseconds since 1970 of an RFC 3339 time.
pub(crate) fn millis(at: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}
