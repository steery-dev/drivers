//! The events of a conversation's records, the `event` JSON of its `transcript_events` rows,
//! in the order of its active path.
//!
//! A `message` record is the person's (`user`), the agent's (`assistant`: thinking, text and
//! tool calls in `content`, how it ended in `stopReason`) or a call's result (`toolResult`).
//! OpenClaw also writes `user` messages that no person typed: a heartbeat's poll and the
//! prompts it sends to retry a turn, each with a `provenance`
//! (`src/auto-reply/heartbeat.ts:19`). A turn that ends with no reply gets a
//! `run-failed-before-reply` `custom_message` after the agent's last message. A spawned
//! sub-agent's first `user` message wraps its task in internal context blocks, which the
//! person's words leave out (`src/agents/internal-runtime-context.ts`).

use std::collections::HashMap;

use serde_json::Value;
use steery_sdk::wire::harness::{
    Call, Compactor, Count, Event, Execution, Failure, FailureKind, Group, TurnState, Voice,
};

/// How many of a result's last lines `call_ended` carries.
const TAIL_LINES: usize = 8;

/// The words of a heartbeat's poll (`src/auto-reply/heartbeat.ts:19`).
const HEARTBEAT: &str = "[OpenClaw heartbeat poll]";

/// The record that closes a turn that gave no reply.
const FAILED_BEFORE_REPLY: &str = "run-failed-before-reply";

/// The lines that open and close an internal context block
/// (`packages/llm-core/src/types.ts:392`).
const CONTEXT_BEGIN: &str = "<<<BEGIN_OPENCLAW_INTERNAL_CONTEXT>>>";
const CONTEXT_END: &str = "<<<END_OPENCLAW_INTERNAL_CONTEXT>>>";

/// Steery's MCP server, as its setup names it in `mcp.servers`.
const STEERY: &str = "steery";

/// One record of the active path.
pub struct Entry {
    /// The window (`session_windows.session_id`) whose row it is.
    pub window: String,
    pub seq: i64,
    pub record: Value,
    /// The length of its JSON.
    pub bytes: u64,
}

/// A sub-agent of the conversation, by its session key.
pub struct Child {
    pub external_id: String,
    pub state: Option<Execution>,
}

pub struct Fold<'a> {
    pub events: Vec<Event>,
    pub path: &'a [Entry],
    /// The MCP servers' names as they stand in their tools' names.
    pub servers: &'a [String],
    pub children: &'a HashMap<String, Child>,
    /// The model's context window in tokens, where OpenClaw resolved it.
    pub context: Option<u64>,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

/// A record's time: its message's `timestamp` in milliseconds where it has one, else the
/// record's own ISO `timestamp`.
pub fn at(record: &Value) -> u64 {
    record
        .get("message")
        .and_then(|message| message.get("timestamp"))
        .and_then(Value::as_u64)
        .or_else(|| {
            let iso = record.get("timestamp")?.as_str()?;
            let at = chrono::DateTime::parse_from_rfc3339(iso).ok()?;
            u64::try_from(at.timestamp_millis()).ok()
        })
        .unwrap_or(0)
}

/// The text of a message's `content`: a string, or the `text` of its `text` parts.
pub fn said(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Whether a `user` message is words that no person typed.
pub fn machine(message: &Value) -> bool {
    message.get("provenance").is_some_and(|p| !p.is_null())
        || said(message.get("content")).trim() == HEARTBEAT
}

/// The text outside internal context blocks, which nest; an unclosed block runs to the end.
fn outside_context(text: &str) -> String {
    let mut depth = 0usize;
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        match line.trim() {
            CONTEXT_BEGIN => depth += 1,
            CONTEXT_END if depth > 0 => depth -= 1,
            CONTEXT_END => {}
            _ if depth == 0 => kept.push(line),
            _ => {}
        }
    }
    let mut out = String::new();
    let mut blank = false;
    for line in kept.join("\n").trim().lines() {
        if line.trim().is_empty() {
            blank = true;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        out.push_str(line);
        blank = false;
    }
    out
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

impl Fold<'_> {
    pub fn entry(&mut self, index: usize) {
        let record = &self.path[index].record;
        let at = at(record);
        let id = text(record, "id").unwrap_or_else(|| {
            let entry = &self.path[index];
            format!("{}#{}", entry.window, entry.seq)
        });
        match record.get("type").and_then(Value::as_str) {
            Some("message") => {
                let Some(message) = record.get("message") else {
                    return;
                };
                match message.get("role").and_then(Value::as_str) {
                    Some("user") => self.asked(at, id, message),
                    Some("assistant") => self.answered(index, at, id, message),
                    Some("toolResult") => self.result(at, id, message),
                    _ => {}
                }
            }
            Some("custom_message")
                if record.get("customType").and_then(Value::as_str)
                    == Some(FAILED_BEFORE_REPLY) =>
            {
                let line = record
                    .get("details")
                    .and_then(|details| details.get("error"))
                    .and_then(Value::as_str)
                    .or_else(|| record.get("content").and_then(Value::as_str))
                    .and_then(first_line)
                    .unwrap_or("OpenClaw's agent gave no reply")
                    .to_owned();
                self.ended(at, TurnState::Failed, Some(failure(line, None, None)));
            }
            Some("compaction") => {
                let tokens = |key: &str| record.get(key).and_then(Value::as_u64);
                self.events.push(Event::Compacted {
                    at,
                    id: id.clone(),
                    by: Compactor::Harness { reason: None },
                    tokens_before: tokens("tokensBefore"),
                    tokens_after: tokens("tokensAfter"),
                });
                let summary = text(record, "summary").unwrap_or_default();
                self.words(at, format!("{id}:summary"), Voice::Summary, &summary, None);
            }
            Some("model_change") => {
                if let Some(model) = text(record, "modelId") {
                    self.events.push(Event::Model {
                        at,
                        model,
                        provider: text(record, "provider"),
                        thinking: None,
                    });
                }
            }
            Some("thinking_level_change") => {
                // The model it applies to is the last one before it on the path.
                let model = self.path[..index].iter().rev().find_map(|entry| {
                    let record = &entry.record;
                    (record.get("type").and_then(Value::as_str) == Some("model_change"))
                        .then(|| Some((text(record, "modelId")?, text(record, "provider"))))
                        .flatten()
                });
                if let (Some((model, provider)), Some(level)) =
                    (model, text(record, "thinkingLevel"))
                {
                    self.events.push(Event::Model {
                        at,
                        model,
                        provider,
                        thinking: Some(level),
                    });
                }
            }
            _ => {}
        }
    }

    fn words(&mut self, at: u64, id: String, voice: Voice, text: &str, call: Option<String>) {
        if text.trim().is_empty() {
            return;
        }
        self.events.push(Event::Words {
            at,
            id,
            voice,
            text: text.to_owned(),
            message: None,
            call,
            clips: Vec::new(),
        });
    }

    fn ended(&mut self, at: u64, state: TurnState, failure: Option<Failure>) {
        self.events.push(Event::TurnEnded {
            at,
            turn: None,
            state,
            failure,
        });
    }

    /// A `user` message begins a turn; only a person's says words as the person.
    fn asked(&mut self, at: u64, id: String, message: &Value) {
        self.events.push(Event::TurnBegan { at, turn: None });
        if !machine(message) {
            let text = outside_context(&said(message.get("content")));
            if !text.is_empty() {
                self.words(at, id, Voice::Person, &text, None);
            }
        }
    }

    fn answered(&mut self, index: usize, at: u64, id: String, message: &Value) {
        let parts = message
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let joined = |kind: &str, key: &str| {
            parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some(kind))
                .filter_map(|part| part.get(key).and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n")
        };
        let thought = joined("thinking", "thinking");
        self.words(at, format!("{id}:thought"), Voice::Thought, &thought, None);
        let answer = match message.get("content") {
            Some(Value::String(text)) => text.clone(),
            _ => joined("text", "text"),
        };
        self.words(at, id.clone(), Voice::Agent, &answer, None);
        for part in parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("toolCall"))
        {
            if let Some(call) = call(part, &id, self.servers) {
                self.events.push(Event::CallBegan { at, call });
            }
        }
        if let Some(usage) = message.get("usage") {
            self.tokens(at, &id, message, usage);
        }
        let model = text(message, "model");
        let provider = text(message, "provider");
        let end = match message.get("stopReason").and_then(Value::as_str) {
            Some("toolUse") | None => return,
            Some("error") => {
                let line = message
                    .get("errorMessage")
                    .and_then(Value::as_str)
                    .and_then(first_line)
                    .unwrap_or("the model's response failed")
                    .to_owned();
                (TurnState::Failed, Some(failure(line, provider, model)))
            }
            Some("aborted") => (TurnState::Interrupted, None),
            // `stop` and `length`.
            Some(_) => (TurnState::Finished, None),
        };
        // A turn that OpenClaw closes as failed ends there, not at its last message.
        if !self.fails_after(index) {
            self.ended(at, end.0, end.1);
        }
    }

    /// Whether a `run-failed-before-reply` follows the message at `index` before the next
    /// message.
    fn fails_after(&self, index: usize) -> bool {
        for entry in &self.path[index + 1..] {
            let record = &entry.record;
            match record.get("type").and_then(Value::as_str) {
                Some("message") => return false,
                Some("custom_message")
                    if record.get("customType").and_then(Value::as_str)
                        == Some(FAILED_BEFORE_REPLY) =>
                {
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    /// `usage` counts `input` without the cache: Steery's `input` counts all it read.
    fn tokens(&mut self, at: u64, id: &str, message: &Value, usage: &Value) {
        let (input, read, write, output) = (
            number(usage, "input"),
            number(usage, "cacheRead"),
            number(usage, "cacheWrite"),
            number(usage, "output"),
        );
        if input + read + write + output == 0 {
            return;
        }
        let total = usage
            .get("contextUsage")
            .and_then(|context| context.get("totalTokens"))
            .and_then(Value::as_u64)
            .unwrap_or(input + read + write + output);
        self.events.push(Event::Tokens {
            at,
            count: Count::Response,
            response: Some(id.to_owned()),
            model: text(message, "model"),
            input: input + read + write,
            cached: read,
            cache_write: Some(write),
            output,
            reasoning: 0,
            context_left: self
                .context
                .filter(|&window| window > 0)
                .map(|window| (1.0 - total as f64 / window as f64).clamp(0.0, 1.0)),
        });
    }

    fn result(&mut self, at: u64, id: String, message: &Value) {
        let Some(call) = text(message, "toolCallId") else {
            return;
        };
        let content = said(message.get("content"));
        let lines: Vec<&str> = content.lines().collect();
        let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
        self.events.push(Event::CallEnded {
            at,
            id: call.clone(),
            ok: message.get("isError").and_then(Value::as_bool) != Some(true),
            chars: content.chars().count() as u64,
            tail: (!tail.trim().is_empty()).then_some(tail),
            waited: Vec::new(),
        });
        self.words(at, id, Voice::Tool, &content, Some(call.clone()));
        // `sessions_spawn` answers with the key of the session it started
        // (`src/agents/tools/sessions-spawn-cloud.test.ts:139`); no live record shows one.
        if message.get("toolName").and_then(Value::as_str) == Some("sessions_spawn")
            && let Some(child) = message
                .get("details")
                .and_then(|details| details.get("childSessionKey"))
                .and_then(Value::as_str)
                .and_then(|key| self.children.get(key))
        {
            self.events.push(Event::SubAgent {
                at,
                external_id: child.external_id.clone(),
                call: Some(call),
                state: child.state,
                error: None,
            });
        }
    }
}

fn failure(line: String, provider: Option<String>, model: Option<String>) -> Failure {
    Failure {
        kind: FailureKind::Other,
        line,
        service: true,
        http_status: None,
        code: None,
        retry_at: None,
        provider,
        model,
        thinking: None,
    }
}

/// A server's name as OpenClaw puts it before its tools' names: each character outside
/// `[A-Za-z0-9_-]` made `-`, a name that does not start with a letter prefixed `mcp-`, cut to
/// 30 characters (`src/agents/agent-bundle-mcp-names.ts:35-46`). A collision's `-2` suffix
/// is not followed.
pub fn safe_server(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = if cleaned.is_empty() {
        "mcp".to_owned()
    } else {
        cleaned
    };
    let safe = if cleaned.starts_with(|c: char| c.is_ascii_alphabetic()) {
        cleaned
    } else {
        format!("mcp-{cleaned}")
    };
    safe.chars().take(30).collect()
}

fn group(tool: &str) -> Group {
    match tool {
        "exec" | "process" | "bash" => Group::Shell,
        "read" | "ls" => Group::FilesRead,
        "write" | "edit" | "apply_patch" => Group::FilesEdit,
        "grep" | "find" | "glob" => Group::Search,
        "web_search" | "web_fetch" => Group::Web,
        "browser" => Group::Browser,
        "sessions_spawn" | "sessions_send" | "sessions_yield" | "sessions_list"
        | "sessions_history" | "subagents" | "agents_list" => Group::Agents,
        "memory_search" | "memory_get" => Group::Memory,
        "image" | "image_generate" | "music_generate" | "video_generate" | "tts" => Group::Media,
        "openclaw" | "tool_search" | "tool_describe" | "dispatch_action" | "session_status"
        | "heartbeat_respond" | "cron" | "gateway" => Group::Harness,
        _ => Group::Custom,
    }
}

/// A `toolCall` part: `{id, name, arguments}`. OpenClaw names an MCP tool
/// `<server>__<tool>` (`src/agents/agent-bundle-mcp-names.ts:10,93`).
fn call(part: &Value, response: &str, servers: &[String]) -> Option<Call> {
    let id = part.get("id").and_then(Value::as_str)?;
    let tool = part.get("name").and_then(Value::as_str)?;
    let input = part.get("arguments").cloned().unwrap_or(Value::Null);
    let server = servers
        .iter()
        .filter(|server| {
            tool.strip_prefix(server.as_str())
                .and_then(|tail| tail.strip_prefix("__"))
                .is_some_and(|tail| !tail.is_empty())
        })
        .max_by_key(|server| server.len())
        .cloned();
    let steery = match &server {
        Some(name) if name == STEERY => tool.strip_prefix("steery__").map(str::to_owned),
        _ => None,
    };
    let group = match (&server, &steery) {
        (_, Some(_)) => Group::Steery,
        (Some(_), None) => Group::Mcp,
        (None, _) => group(tool),
    };
    Some(Call {
        id: id.to_owned(),
        tool: tool.to_owned(),
        input,
        group,
        server,
        steery,
        asks: None,
        parent: None,
        response: Some(response.to_owned()),
    })
}
