//! What Claude Code's records say happened, as the protocol's events.
//!
//! Claude Code appends one record per line: `user` records carry what went into the model
//! (a person's words, a call's result, the harness's own notes marked `isMeta`, a
//! compaction's summary marked `isCompactSummary`), `assistant` records carry one content
//! block of a response each, every one repeating the response's `message.id` and its
//! `usage`, and `system` records carry the harness's bookkeeping. A sub-agent's records are
//! marked `isSidechain`: in its own transcript they are its own, and in its lead's (where
//! older releases inlined them) they are none of the lead's.
//!
//! A turn begins at what the agent is given to work on: a person's message, a command, a
//! notice from a background task. It ends at the agent's last answer of it, which stops for
//! any reason but a call (`end_turn`, `stop_sequence`, `max_tokens`, `refusal`), at the
//! `system` `turn_duration` record newer releases write once it is over, at Claude Code's
//! answer to a local command (`<local-command-stdout>`), at `[Request interrupted by
//! user…]`, or at the API error Claude Code writes once it gave up retrying
//! (`isApiErrorMessage`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;
use steery_sdk::wire::harness::{
    Clip, Count, Event, Execution, Failure, FailureKind, TurnState, Voice,
};

use crate::calls;

/// What Claude Code names what it writes itself — an API error, a filler reply — in place
/// of a model.
const SYNTHETIC: &str = "<synthetic>";

/// What reading one transcript needs besides its records.
pub struct Reading {
    /// It is a sub-agent's own transcript, whose `isSidechain` records are its own.
    pub own: bool,
    /// The folder the conversation's sub-agents are filed in, `<conversation>/subagents`.
    pub agents: PathBuf,
}

/// One whole record of a transcript, and where it starts: `None` where it is no JSON.
pub struct Record {
    pub start: u64,
    pub value: Option<Value>,
}

/// Where a turn stands between reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    /// Ended, or none began.
    Closed,
    Open,
    /// Read from the middle: whatever came before is not known.
    Unknown,
}

/// What a run of records came to.
pub struct Read {
    pub events: Vec<Event>,
    /// Where the turn stands after them.
    pub turn: Turn,
}

/// The events of `records`, the records of the conversation's path in order, read on from
/// where `turn` says the turn stands.
pub fn read(reading: &Reading, records: &[Record], turn: Turn) -> Read {
    let mut state = State {
        reading,
        at: 0,
        turn,
        model: None,
        tokens: None,
        running: HashSet::new(),
        started: None,
        out: Vec::new(),
    };
    for record in records {
        if let Some(value) = &record.value {
            state.record(record.start, value);
        }
    }
    Read {
        events: state.out,
        turn: state.turn,
    }
}

/// What a record off the conversation's path still spent: a response on a branch a rewind
/// left behind is no part of the conversation's history, and its tokens were spent all the
/// same. Named by its response's id, as on the path, so a response said twice counts once.
pub fn spent(record: &Value) -> Option<Event> {
    let message = record
        .get("message")
        .filter(|_| text(record, "type") == Some("assistant"))?;
    let model = text(message, "model").map(str::trim);
    if model == Some(SYNTHETIC) || flag(record, "isApiErrorMessage") {
        return None;
    }
    let at = text(record, "timestamp").and_then(millis).unwrap_or(0);
    Some(tokens(
        at,
        text(message, "id")?.to_owned(),
        model.filter(|model| !model.is_empty()),
        message.get("usage")?,
    ))
}

/// One response's `usage`: what it read fresh, wrote to a cache and read from one are all
/// of `input`; its thinking is of `output`.
fn tokens(at: u64, response: String, model: Option<&str>, usage: &Value) -> Event {
    let number = |value: Option<&Value>| value.and_then(Value::as_u64).unwrap_or(0);
    let cached = number(usage.get("cache_read_input_tokens"));
    let cache_write = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    Event::Tokens {
        at,
        count: Count::Response,
        response: Some(response),
        model: model.map(str::to_owned),
        input: number(usage.get("input_tokens")) + cache_write.unwrap_or(0) + cached,
        cached,
        cache_write,
        output: number(usage.get("output_tokens")),
        reasoning: number(usage.pointer("/output_tokens_details/thinking_tokens")),
        context_left: None,
    }
}

struct State<'a> {
    reading: &'a Reading,
    at: u64,
    turn: Turn,
    /// The model and effort last said.
    model: Option<(String, Option<String>)>,
    /// The response and `usage` last said, so a response its next block repeats unchanged
    /// is said once.
    tokens: Option<(String, Value)>,
    /// The sub-agents said running.
    running: HashSet<String>,
    /// The sub-agent each call started, by the `.meta.json` Claude Code files beside each:
    /// read once, where a call needs it.
    started: Option<HashMap<String, String>>,
    out: Vec<Event>,
}

impl State<'_> {
    fn record(&mut self, start: u64, record: &Value) {
        if let Some(at) = text(record, "timestamp").and_then(millis) {
            self.at = at;
        }
        if !self.reading.own && flag(record, "isSidechain") {
            return;
        }
        let id = text(record, "uuid").map_or_else(|| format!("@{start}"), str::to_owned);
        match text(record, "type") {
            Some("user") => self.user(id, record),
            Some("assistant") => self.assistant(id, record),
            Some("system") if text(record, "subtype") == Some("turn_duration") => {
                self.end(TurnState::Finished, None);
            }
            _ => {}
        }
    }

    fn user(&mut self, id: String, record: &Value) {
        let Some(content) = record.pointer("/message/content") else {
            return;
        };
        let results: Vec<&Value> = blocks(content)
            .iter()
            .filter(|block| text(block, "type") == Some("tool_result"))
            .collect();
        if !results.is_empty() {
            for block in results {
                self.result(&id, record, block);
            }
            return;
        }
        let said = texts(content);
        if interrupted(&said) {
            self.end(TurnState::Interrupted, None);
            return;
        }
        if flag(record, "isCompactSummary") {
            self.words(id, Voice::Summary, said.trim().to_owned(), None, Vec::new());
            return;
        }
        // The harness talking to itself, and the Claude Agent SDK echoing a message the
        // person already sent. Words another session sent through a live session's inbox
        // are marked `isMeta` too, with a `peer` origin, and are the agent's to work on.
        let peer = record.pointer("/origin/kind").and_then(Value::as_str) == Some("peer");
        if (flag(record, "isMeta") && !peer)
            || flag(record, "isSynthetic")
            || flag(record, "isReplay")
        {
            return;
        }
        let trimmed = said.trim_start();
        if trimmed.starts_with("<local-command-stdout>")
            || trimmed.starts_with("<local-command-stderr>")
        {
            self.end(TurnState::Finished, None);
            return;
        }
        self.notified(&said);
        self.begin();
        if let Some((said, clips)) = person(record) {
            self.words(id, Voice::Person, said, None, clips);
        }
    }

    fn assistant(&mut self, id: String, record: &Value) {
        let Some(message) = record.get("message") else {
            return;
        };
        if flag(record, "isApiErrorMessage") {
            self.end(TurnState::Failed, Some(failure(record, message)));
            return;
        }
        let model = text(message, "model").map(str::trim);
        if model == Some(SYNTHETIC) {
            return;
        }
        let model = model.filter(|model| !model.is_empty());
        let at = self.at;
        if let Some(model) = model {
            let said = (
                model.to_owned(),
                text(record, "effort")
                    .filter(|effort| !effort.is_empty())
                    .map(str::to_owned),
            );
            if self.model.as_ref() != Some(&said) {
                self.out.push(Event::Model {
                    at,
                    model: said.0.clone(),
                    provider: None,
                    thinking: said.1.clone(),
                });
                self.model = Some(said);
            }
        }
        let response = text(message, "id").map(str::to_owned);
        let content = blocks(message.get("content").unwrap_or(&Value::Null));
        let joined = |kind: &str, field: &str| -> String {
            content
                .iter()
                .filter(|block| text(block, "type") == Some(kind))
                .filter_map(|block| text(block, field))
                .filter(|said| !said.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let thought = joined("thinking", "thinking");
        if !thought.is_empty() {
            self.words(
                format!("{id}:thought"),
                Voice::Thought,
                thought,
                None,
                Vec::new(),
            );
        }
        let said = joined("text", "text");
        if !said.is_empty() {
            self.words(id.clone(), Voice::Agent, said, None, Vec::new());
        }
        for block in content
            .iter()
            .filter(|block| text(block, "type") == Some("tool_use"))
        {
            let (Some(call), Some(name)) = (text(block, "id"), text(block, "name")) else {
                continue;
            };
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            let carried = flattened(&input);
            let mut began = calls::call(call, name, input);
            began.response = response.clone();
            self.out.push(Event::CallBegan { at, call: began });
            if !carried.trim().is_empty() {
                self.words(
                    format!("{id}:{call}"),
                    Voice::Tool,
                    carried,
                    Some(call.to_owned()),
                    Vec::new(),
                );
            }
            if AGENTS.contains(&name)
                && let Some(agent) = self.started_by(call)
            {
                self.sub_agent(agent, Some(call.to_owned()), Execution::Running, None);
            }
        }
        if let (Some(response), Some(usage)) = (response, message.get("usage")) {
            self.tokens(response, model, usage);
        }
        if text(message, "stop_reason").is_some_and(|reason| ENDS.contains(&reason)) {
            self.end(TurnState::Finished, None);
        }
    }

    /// One call's result: its text, its last lines, and where the call started a
    /// sub-agent, how that sub-agent's run came out as Claude Code answered the call: its
    /// `toolUseResult` `completed`, `async_launched` for one that went on in the
    /// background, or an error, which `[Request interrupted…]` says the person caused.
    fn result(&mut self, id: &str, record: &Value, block: &Value) {
        let Some(call) = text(block, "tool_use_id") else {
            return;
        };
        let at = self.at;
        let said = without_reminders(&flatten(block.get("content")))
            .trim()
            .to_owned();
        let failed = flag(block, "is_error");
        let lines: Vec<&str> = said.lines().collect();
        let tail = lines[lines.len().saturating_sub(8)..].join("\n");
        self.out.push(Event::CallEnded {
            at,
            id: call.to_owned(),
            ok: !failed,
            chars: said.chars().count() as u64,
            tail: (!tail.trim().is_empty()).then_some(tail),
        });
        let result = record.get("toolUseResult");
        let answered = if failed {
            Some(if interrupted(&said) {
                (Execution::Interrupted, None)
            } else {
                (
                    Execution::Failed,
                    Some(one_line(&said)).filter(|why| !why.is_empty()),
                )
            })
        } else {
            match result.and_then(|result| text(result, "status")) {
                Some("completed") => Some((Execution::Finished, None)),
                Some("async_launched") => Some((Execution::Running, None)),
                _ => None,
            }
        };
        if let Some((state, error)) = answered {
            let agent = result
                .and_then(|result| text(result, "agentId"))
                .map(str::to_owned)
                .or_else(|| self.started_by(call));
            if let Some(agent) = agent
                && !(state == Execution::Running && self.running.contains(&agent))
            {
                self.sub_agent(agent, Some(call.to_owned()), state, error);
            }
        }
        if !said.is_empty() {
            self.words(
                format!("{id}:{call}"),
                Voice::Tool,
                said,
                Some(call.to_owned()),
                Vec::new(),
            );
        }
    }

    /// A `<task-notification>` Claude Code posts each time a sub-agent running in the
    /// background stops, naming it as `task-id` and the call that started it as
    /// `tool-use-id`: `completed`, `failed` with why in its summary, `killed` when its lead
    /// stopped it, `stopped` when Claude Code found no record of how it ended. A background
    /// shell's notice names no sub-agent filed here, and says nothing of one.
    fn notified(&mut self, said: &str) {
        let Some((_, notice)) = said.split_once("<task-notification>") else {
            return;
        };
        let state = match tagged(notice, "status") {
            Some("completed") => Execution::Finished,
            Some("failed") => Execution::Failed,
            Some("killed" | "stopped") => Execution::Interrupted,
            _ => return,
        };
        let Some(agent) = tagged(notice, "task-id").filter(|agent| self.filed(agent)) else {
            return;
        };
        let error = tagged(notice, "summary")
            .filter(|_| state == Execution::Failed)
            .map(|summary| {
                one_line(
                    summary
                        .split_once(" failed: ")
                        .map_or(summary, |(_, why)| why),
                )
            });
        let call = tagged(notice, "tool-use-id").map(str::to_owned);
        self.sub_agent(agent.to_owned(), call, state, error);
    }

    fn sub_agent(
        &mut self,
        external_id: String,
        call: Option<String>,
        state: Execution,
        error: Option<String>,
    ) {
        if state == Execution::Running {
            self.running.insert(external_id.clone());
        } else {
            self.running.remove(&external_id);
        }
        self.out.push(Event::SubAgent {
            at: self.at,
            external_id,
            call,
            state: Some(state),
            error,
        });
    }

    /// Whether a sub-agent of this id is filed with this conversation's.
    fn filed(&self, agent: &str) -> bool {
        agent_id(agent) && agent_file(&self.reading.agents, agent).is_file()
    }

    /// The sub-agent `call` started, as the `.meta.json` beside its transcript names the
    /// call (`toolUseId`).
    fn started_by(&mut self, call: &str) -> Option<String> {
        self.started
            .get_or_insert_with(|| started(&self.reading.agents))
            .get(call)
            .cloned()
    }

    /// One response's count, said once however many of its blocks repeat it unchanged.
    fn tokens(&mut self, response: String, model: Option<&str>, usage: &Value) {
        let said = (response, usage.clone());
        if self.tokens.as_ref() == Some(&said) {
            return;
        }
        self.out.push(tokens(self.at, said.0.clone(), model, usage));
        self.tokens = Some(said);
    }

    fn begin(&mut self) {
        if self.turn != Turn::Open {
            self.out.push(Event::TurnBegan {
                at: self.at,
                turn: None,
            });
            self.turn = Turn::Open;
        }
    }

    fn end(&mut self, state: TurnState, failure: Option<Failure>) {
        if self.turn != Turn::Closed {
            self.out.push(Event::TurnEnded {
                at: self.at,
                turn: None,
                state,
                failure,
            });
            self.turn = Turn::Closed;
        }
    }

    fn words(
        &mut self,
        id: String,
        voice: Voice,
        text: String,
        call: Option<String>,
        clips: Vec<Clip>,
    ) {
        if text.is_empty() && clips.is_empty() {
            return;
        }
        self.out.push(Event::Words {
            at: self.at,
            id,
            voice,
            text,
            call,
            clips,
        });
    }
}

/// The tools that start a sub-agent: `Task`, renamed `Agent` in later releases.
const AGENTS: [&str; 2] = ["Task", "Agent"];

/// The `stop_reason`s of a response that ends its turn: any but a call.
const ENDS: [&str; 4] = ["end_turn", "stop_sequence", "max_tokens", "refusal"];

/// Why a turn failed, as Claude Code recorded it: the API's status (`apiErrorStatus`) and
/// its own code (`error`), its words in the message. Never read from wording: a 401 or
/// its `authentication_failed` is a sign-in it could not make; a 429, 503 or 529 or its
/// `rate_limit` a limit. Every such failure is the model service's.
fn failure(record: &Value, message: &Value) -> Failure {
    let http_status = record
        .get("apiErrorStatus")
        .and_then(Value::as_u64)
        .filter(|status| (100..=599).contains(status))
        .and_then(|status| u16::try_from(status).ok());
    let code = text(record, "error")
        .filter(|code| !code.is_empty())
        .map(str::to_owned);
    let signed_out = match http_status {
        Some(status) => status == 401,
        None => code.as_deref() == Some("authentication_failed"),
    };
    let limited = http_status.is_some_and(|status| matches!(status, 429 | 503 | 529))
        || code.as_deref() == Some("rate_limit");
    let kind = if signed_out {
        FailureKind::SignIn
    } else if limited {
        FailureKind::RateLimit
    } else {
        FailureKind::Other
    };
    let line = blocks(message.get("content").unwrap_or(&Value::Null))
        .iter()
        .find_map(|block| text(block, "text"))
        .map(one_line)
        .unwrap_or_default();
    Failure {
        kind,
        line,
        service: true,
        http_status,
        code,
        retry_at: None,
        provider: None,
        model: text(message, "model")
            .filter(|model| *model != SYNTHETIC && !model.is_empty())
            .map(str::to_owned),
        thinking: None,
    }
}

/// What a person put in a record, and what it carried: only a message Claude Code marks
/// as theirs (`origin.kind` `human`), never the harness's own notes inside it
/// (`<system-reminder>`), and never a message that is one element and nothing else, which
/// is how a harness posts in a person's place.
fn person(record: &Value) -> Option<(String, Vec<Clip>)> {
    if record.pointer("/origin/kind").and_then(Value::as_str) != Some("human") {
        return None;
    }
    let content = record.pointer("/message/content")?;
    let said = without_reminders(&texts(content)).trim().to_owned();
    if enveloped(&said) {
        return None;
    }
    let clips: Vec<Clip> = blocks(content).iter().filter_map(carried).collect();
    (!said.is_empty() || !clips.is_empty()).then_some((said, clips))
}

/// The words a user record gives the agent to work on: what it says, the harness's notes
/// taken out, where it is no call's result, no note of the harness's own and no summary.
/// A conversation's title and a sub-agent's task are read from its first.
pub fn instruction(record: &Value) -> Option<String> {
    if text(record, "type") != Some("user")
        || ["isMeta", "isCompactSummary", "isSynthetic", "isReplay"]
            .iter()
            .any(|name| flag(record, name))
    {
        return None;
    }
    let content = record.pointer("/message/content")?;
    if blocks(content)
        .iter()
        .any(|block| text(block, "type") == Some("tool_result"))
    {
        return None;
    }
    let said = without_reminders(&texts(content)).trim().to_owned();
    (!said.is_empty()).then_some(said)
}

/// A picture or a document a person's message carried: its bytes in the record, or an
/// address Claude Code passed on. A note where the bytes were (`[base64 omitted: …]`) is
/// bytes Claude Code did not keep, and no clip.
fn carried(block: &Value) -> Option<Clip> {
    if !matches!(text(block, "type"), Some("image" | "document")) {
        return None;
    }
    let source = block.get("source")?;
    match text(source, "type")? {
        "base64" => {
            let data = text(source, "data")?;
            (!data.is_empty() && !data.starts_with("[base64 omitted")).then(|| Clip::Bytes {
                data: data.to_owned(),
                media_type: text(source, "media_type").map(str::to_owned),
                name: None,
            })
        }
        "url" => Some(Clip::Link {
            url: text(source, "url")?.to_owned(),
        }),
        _ => None,
    }
}

/// The sub-agent each call started: every `agent-<id>.meta.json` in `agents` naming its
/// call as `toolUseId`.
fn started(agents: &Path) -> HashMap<String, String> {
    let Ok(entries) = std::fs::read_dir(agents) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let agent = name
                .to_str()?
                .strip_prefix("agent-")?
                .strip_suffix(".meta.json")?;
            if !agent_id(agent) || !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                return None;
            }
            let meta: Value = serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
            Some((text(&meta, "toolUseId")?.to_owned(), agent.to_owned()))
        })
        .collect()
}

/// Whether a sub-agent's id can be one component of a path: letters, digits, `-` and `_`.
pub fn agent_id(agent: &str) -> bool {
    !agent.is_empty()
        && agent
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Where Claude Code files sub-agent `agent`'s transcript: `<agents>/agent-<id>.jsonl`.
pub fn agent_file(agents: &Path, agent: &str) -> PathBuf {
    agents.join(format!("agent-{agent}.jsonl"))
}

/// Claude Code's own words where a person stopped the turn: `[Request interrupted by
/// user]`, and `… for tool use]` where they stopped it inside a call.
fn interrupted(said: &str) -> bool {
    said.trim_start()
        .starts_with("[Request interrupted by user")
}

/// The content blocks of a message: none where it is a string.
fn blocks(content: &Value) -> &[Value] {
    content.as_array().map_or(&[], Vec::as_slice)
}

/// The words of a content: itself where it is a string, else its text blocks joined.
fn texts(content: &Value) -> String {
    match content {
        Value::String(said) => said.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| text(block, "type") == Some("text"))
            .filter_map(|block| text(block, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The words inside a call's result, whatever shape Claude Code wrote it in: a string, or
/// content blocks, of which the text ones are read.
fn flatten(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(said)) => said.clone(),
        Some(content @ Value::Array(_)) => texts(content),
        _ => String::new(),
    }
}

/// Every string a call's input carries, in order, one to a line.
fn flattened(input: &Value) -> String {
    fn walk<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
        match value {
            Value::String(said) if !said.is_empty() => out.push(said),
            Value::Array(items) => items.iter().for_each(|item| walk(item, out)),
            Value::Object(fields) => fields.values().for_each(|field| walk(field, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(input, &mut out);
    out.join("\n")
}

/// Text with every `<system-reminder>…</system-reminder>` taken out: Claude Code's own
/// notes, written into a person's message ahead of what they typed and into results.
fn without_reminders(said: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(said.len());
    let mut rest = said;
    while let Some(start) = rest.find(OPEN) {
        let Some(end) = rest[start..].find(CLOSE) else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + end + CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

/// Whether the text is one element, `<name …>…</name>`, and nothing else: how a harness
/// wraps what it posts as the person, never how a person types.
fn enveloped(said: &str) -> bool {
    let said = said.trim();
    let Some(rest) = said.strip_prefix('<') else {
        return false;
    };
    let name = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')))
        .map_or(rest, |end| &rest[..end]);
    !name.is_empty()
        && rest[name.len()..].starts_with(|c: char| c == '>' || c.is_whitespace())
        && said
            .strip_suffix('>')
            .and_then(|said| said.strip_suffix(name))
            .is_some_and(|said| said.ends_with("</"))
}

/// What the first `<tag>…</tag>` in `said` holds, trimmed.
fn tagged<'a>(said: &'a str, tag: &str) -> Option<&'a str> {
    let rest = said.split_once(&format!("<{tag}>"))?.1;
    Some(rest.split_once(&format!("</{tag}>"))?.0.trim())
}

/// Words as one line: every run of whitespace one space.
pub fn one_line(said: &str) -> String {
    said.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn flag(value: &Value, name: &str) -> bool {
    value.get(name).and_then(Value::as_bool) == Some(true)
}

pub fn text<'a>(value: &'a Value, name: &str) -> Option<&'a str> {
    value.get(name).and_then(Value::as_str)
}

/// An RFC 3339 time in milliseconds since 1970.
pub fn millis(at: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn reading(own: bool) -> Reading {
        Reading {
            own,
            agents: PathBuf::from("/nowhere/c/subagents"),
        }
    }

    fn records(lines: &[Value]) -> Vec<Record> {
        lines
            .iter()
            .enumerate()
            .map(|(at, value)| Record {
                start: at as u64 * 10,
                value: Some(value.clone()),
            })
            .collect()
    }

    fn events(lines: &[Value]) -> Vec<Event> {
        read(&reading(false), &records(lines), Turn::Closed).events
    }

    fn said(events: &[Event]) -> Vec<(Voice, &str)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Words { voice, text, .. } => Some((*voice, text.as_str())),
                _ => None,
            })
            .collect()
    }

    fn turns(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::TurnBegan { .. } => Some("began".to_owned()),
                Event::TurnEnded { state, .. } => Some(format!("{state:?}")),
                _ => None,
            })
            .collect()
    }

    /// A sub-agent's records inlined into its lead's file, and the harness talking to
    /// itself, are nobody's words in the conversation; reasoning is the agent's thought,
    /// a call's input and result the tool's (the runtime's
    /// `claude_code_skips_sidechains_thinking_and_tool_arguments`).
    #[test]
    fn sidechains_and_the_harness_s_own_notes_are_nobody_s_words() {
        let read = events(&[
            json!({"type":"custom-title","customTitle":"What it is called"}),
            json!({"uuid":"a","type":"user","isMeta":true,"origin":{"kind":"human"},"message":{"role":"user","content":"harness talking to itself"}}),
            json!({"uuid":"b","type":"user","isSidechain":true,"origin":{"kind":"human"},"message":{"role":"user","content":"a sub-agent's instruction"}}),
            json!({"uuid":"c","type":"user","origin":{"kind":"human"},"message":{"role":"user","content":"the real question"}}),
            json!({"uuid":"d","type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"thinking","thinking":"private"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"rm -rf /"}},{"type":"text","text":"the answer"}]}}),
            json!({"uuid":"e","type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"what the tool said"}]},{"type":"image","source":{"type":"base64","data":"AAAA"}}]}}),
        ]);
        assert_eq!(
            said(&read),
            [
                (Voice::Person, "the real question"),
                (Voice::Thought, "private"),
                (Voice::Agent, "the answer"),
                (Voice::Tool, "rm -rf /"),
                (Voice::Tool, "what the tool said"),
            ]
        );
        assert!(read.iter().any(|event| matches!(
            event,
            Event::CallBegan { call, .. } if call.id == "t1" && call.response.as_deref() == Some("m1")
        )));
        assert!(read.iter().any(|event| matches!(
            event,
            Event::CallEnded { id, ok: true, chars: 18, .. } if id == "t1"
        )));
    }

    /// The person's words are only what Claude Code marks as theirs, its notes in front of
    /// them taken out; a task's notice and a command are not theirs (the runtime's
    /// `a_tail_reads_whole_records_one_at_a_time` and typed inputs).
    #[test]
    fn a_person_is_only_who_claude_code_marks_human() {
        let read = events(&[
            json!({"type": "user", "origin": {"kind": "human"}, "message": {"role": "user", "content":
                "<system-reminder>\nIn a worktree.\n</system-reminder>\nship the chips"}}),
            json!({"type": "user", "origin": {"kind": "task-notification"},
                "message": {"role": "user", "content": "<task-notification>done</task-notification>"}}),
            json!({"type": "user", "message": {"role": "user", "content": "<command-name>/compact</command-name>"}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Shipped."}]}}),
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]}}),
            json!({"type": "user", "origin": {"kind": "human"}, "message": {"role": "user", "content": "  and the retries\n\ttoo  "}}),
        ]);
        assert_eq!(
            said(&read),
            [
                (Voice::Person, "ship the chips"),
                (Voice::Agent, "Shipped."),
                (Voice::Tool, "ok"),
                (Voice::Person, "and the retries\n\ttoo"),
            ]
        );
    }

    /// A picture a person pasted is a clip of their words, its bytes as the record holds
    /// them; one Claude Code did not keep is none.
    #[test]
    fn a_pasted_picture_is_a_clip() {
        let read = events(&[
            json!({"uuid": "p", "type": "user", "origin": {"kind": "human"},
            "message": {"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}},
                {"type": "image", "source": {"type": "base64", "data": "[base64 omitted: image/png]"}},
                {"type": "text", "text": "this one"}]}}),
        ]);
        assert!(read.contains(&Event::Words {
            at: 0,
            id: "p".into(),
            voice: Voice::Person,
            text: "this one".into(),
            call: None,
            clips: vec![Clip::Bytes {
                data: "iVBORw0KGgo=".into(),
                media_type: Some("image/png".into()),
                name: None,
            }],
        }));
    }

    /// A turn ends where the agent stops for anything but a call, once however many of
    /// the response's blocks say so; the person interrupting it ends it interrupted; a
    /// local command's answer ends the turn its command began.
    #[test]
    fn a_turn_ends_once_and_says_how() {
        let asked =
            json!({"type": "user", "origin": {"kind": "human"}, "message": {"content": "go"}});
        let answered = |reason: &str| json!({"type": "assistant", "message": {"id": "m", "stop_reason": reason, "content": []}});
        assert_eq!(
            turns(&events(&[
                asked.clone(),
                answered("tool_use"),
                answered("end_turn"),
                answered("end_turn"),
                json!({"type": "system", "subtype": "turn_duration"}),
            ])),
            ["began", "Finished"]
        );
        assert_eq!(
            turns(&events(&[
                asked.clone(),
                answered("tool_use"),
                json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t", "is_error": true, "content": "[Request interrupted by user for tool use]"}]}}),
                json!({"type": "user", "message": {"content": [{"type": "text", "text": "[Request interrupted by user for tool use]"}]}}),
            ])),
            ["began", "Interrupted"]
        );
        assert_eq!(
            turns(&events(&[
                json!({"type": "user", "message": {"content": "<command-name>/model</command-name>\n<command-message>model</command-message>"}}),
                json!({"type": "user", "message": {"content": "<local-command-stdout>Set model to opus</local-command-stdout>"}}),
            ])),
            ["began", "Finished"]
        );
        // A meta note asks the agent nothing.
        assert!(
            turns(&events(&[
                json!({"type": "user", "isMeta": true, "message": {"content": "Caveat"}})
            ]))
            .is_empty()
        );
        // Read from the middle of a turn, its end is still said.
        let middle = read(
            &reading(false),
            &records(&[answered("end_turn")]),
            Turn::Unknown,
        );
        assert_eq!(turns(&middle.events), ["Finished"]);
        assert_eq!(middle.turn, Turn::Closed);
    }

    /// The API error Claude Code writes once it gave up is the turn's end, failed, with the
    /// service's status and Claude Code's own code, never its wording (the runtime's
    /// `claude_api_error_is_current_until_the_model_answers_or_the_person_types` and
    /// `claude_captured_failure_retains_exact_native_record`).
    #[test]
    fn an_api_error_fails_its_turn_by_status_and_code() {
        let failed = |status: Option<u64>, code: &str| {
            let mut record = json!({"type":"assistant","uuid":"483f5161","timestamp":"2026-09-03T13:26:16.609Z",
                "isApiErrorMessage":true,"error":code,
                "message":{"model":"<synthetic>","content":[{"type":"text","text":"API Error: 500\n Internal server error"}]}});
            if let Some(status) = status {
                record["apiErrorStatus"] = json!(status);
            }
            let read = events(&[
                json!({"type": "user", "origin": {"kind": "human"}, "message": {"content": "go"}}),
                record,
            ]);
            match read.last() {
                Some(Event::TurnEnded {
                    state: TurnState::Failed,
                    failure: Some(failure),
                    at,
                    ..
                }) => {
                    assert_eq!(*at, 1_788_441_976_609);
                    failure.clone()
                }
                other => panic!("{other:?}"),
            }
        };
        let server = failed(Some(500), "server_error");
        assert_eq!(server.kind, FailureKind::Other);
        assert_eq!(server.line, "API Error: 500 Internal server error");
        assert_eq!(
            (server.http_status, server.code.as_deref()),
            (Some(500), Some("server_error"))
        );
        assert!(server.service);
        assert_eq!(
            server.model, None,
            "Claude Code's own record names no model"
        );
        assert_eq!(
            failed(Some(529), "overloaded_error").kind,
            FailureKind::RateLimit
        );
        assert_eq!(failed(None, "rate_limit").kind, FailureKind::RateLimit);
        assert_eq!(
            failed(Some(401), "authentication_failed").kind,
            FailureKind::SignIn
        );
        assert_eq!(
            failed(None, "authentication_failed").kind,
            FailureKind::SignIn
        );
        assert_eq!(
            failed(Some(403), "authentication_failed").kind,
            FailureKind::Other,
            "a service that answered other than 401 took the sign-in"
        );
        assert_eq!(failed(None, "invalid_request").kind, FailureKind::Other);
    }

    /// Claude Code names the model on every assistant record and the effort beside it,
    /// never on what it writes itself (the runtime's `claudes_own_messages_name_no_model`
    /// and `claude_says_what_it_runs_on_and_which_call_is_open`).
    #[test]
    fn the_model_is_said_when_it_changes_and_never_claude_code_s_own() {
        let read = events(&[
            json!({"timestamp":"2026-09-19T10:00:00.000Z","type":"assistant","effort":"xhigh","message":{"model":"claude-opus-5","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo build","description":"Build"}}]}}),
            json!({"timestamp":"2026-09-19T10:00:05.000Z","type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}),
            json!({"timestamp":"2026-09-19T10:00:06.000Z","type":"assistant","effort":"xhigh","message":{"model":"claude-opus-5","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/src/lib.rs"}}]}}),
            json!({"timestamp":"2026-09-19T10:00:07.000Z","type":"assistant","message":{"model":"claude-sonnet-5","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}}),
            json!({"timestamp":"2026-09-19T10:00:08.000Z","type":"assistant","isApiErrorMessage":true,"message":{"model":"<synthetic>","content":[{"type":"text","text":"Autocompact is thrashing"}]}}),
            json!({"timestamp":"2026-09-19T10:00:09.000Z","type":"assistant","message":{"model":"<synthetic>","content":[{"type":"text","text":"No response requested."}]}}),
        ]);
        let models: Vec<(&str, Option<&str>)> = read
            .iter()
            .filter_map(|event| match event {
                Event::Model {
                    model, thinking, ..
                } => Some((model.as_str(), thinking.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(
            models,
            [("claude-opus-5", Some("xhigh")), ("claude-sonnet-5", None)]
        );
        let ended: Vec<(&str, u64)> = read
            .iter()
            .filter_map(|event| match event {
                Event::CallEnded { id, at, .. } => Some((id.as_str(), *at)),
                _ => None,
            })
            .collect();
        assert_eq!(
            ended,
            [("t1", 1_789_812_005_000)],
            "t1 came back when its result was written"
        );
        assert!(!said(&read).contains(&(Voice::Agent, "No response requested.")));
    }

    /// Claude Code writes each response's count once per content block: each is that
    /// response's, under its id, a block repeating it unchanged said once (the runtime's
    /// `a_claude_count_is_each_message_once`).
    #[test]
    fn a_response_s_count_is_its_own() {
        let counted = |id: &str,
                       input: u64,
                       created: u64,
                       read: u64,
                       output: u64,
                       thinking: u64| {
            json!({"type": "assistant", "message": {"id": id, "model": "claude-opus-4", "usage": {
                "input_tokens": input, "cache_creation_input_tokens": created,
                "cache_read_input_tokens": read, "output_tokens": output,
                "output_tokens_details": {"thinking_tokens": thinking}}}})
        };
        let read = events(&[
            counted("m1", 2, 1000, 0, 0, 0),
            counted("m1", 2, 1000, 0, 382, 40),
            counted("m1", 2, 1000, 0, 382, 40),
            json!({"type": "user", "origin": {"kind": "human"}, "message": {"role": "user", "content": "usage please"}}),
            counted("m2", 3, 0, 1000, 20, 0),
        ]);
        let tokens: Vec<_> = read
            .iter()
            .filter_map(|event| match event {
                Event::Tokens {
                    response,
                    input,
                    cached,
                    cache_write,
                    output,
                    reasoning,
                    count: Count::Response,
                    ..
                } => Some((
                    response.as_deref()?,
                    *input,
                    *cached,
                    *cache_write,
                    *output,
                    *reasoning,
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            tokens,
            [
                ("m1", 1002, 0, Some(1000), 0, 0),
                ("m1", 1002, 0, Some(1000), 382, 40),
                ("m2", 1003, 1000, Some(0), 20, 0),
            ]
        );
    }

    /// A sub-agent its lead started in the background, and the notice Claude Code posted
    /// once it failed: running from the call, failed with the reason it was told (the
    /// runtime's `a_sub_agent_reads_failed_with_its_harness_reason`, records as Claude Code
    /// 2.1 wrote them for a background `Task` that hit a rate limit, 2026-09-05).
    #[test]
    fn a_sub_agent_runs_from_its_call_and_ends_as_its_lead_was_told() {
        let home = std::env::temp_dir().join(format!("steery-cc-events-{}", std::process::id()));
        let agents = home.join("lead/subagents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("agent-f00d.jsonl"), "{}\n").unwrap();
        std::fs::write(
            agents.join("agent-f00d.meta.json"),
            r#"{"agentType":"general-purpose","description":"Slice two","toolUseId":"toolu_9"}"#,
        )
        .unwrap();
        let notice = "<task-notification>\n<task-id>f00d</task-id>\n<tool-use-id>toolu_9</tool-use-id>\n<status>failed</status>\n<summary>Agent \"Slice two\" failed: Agent terminated early due to an API error: 429 Too Many Requests</summary>\n</task-notification>";
        let shell = "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>";
        let reading = Reading { own: false, agents };
        let read = read(
            &reading,
            &records(&[
                json!({"type":"user","origin":{"kind":"human"},"message":{"role":"user","content":"Build slice two"}}),
                json!({"type":"assistant","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_9","name":"Task","input":{"description":"Slice two","prompt":"Build it","run_in_background":true}}]}}),
                json!({"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_9","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully."}]}]},"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"f00d"}}),
                json!({"type":"user","origin":{"kind":"task-notification"},"message":{"role":"user","content":notice}}),
                json!({"type":"user","origin":{"kind":"task-notification"},"message":{"role":"user","content":shell}}),
            ]),
            Turn::Closed,
        );
        std::fs::remove_dir_all(&home).ok();
        let agents: Vec<_> = read
            .events
            .iter()
            .filter_map(|event| match event {
                Event::SubAgent {
                    external_id,
                    call,
                    state,
                    error,
                    ..
                } => Some((
                    external_id.as_str(),
                    call.as_deref(),
                    *state,
                    error.as_deref(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            agents,
            [
                ("f00d", Some("toolu_9"), Some(Execution::Running), None),
                (
                    "f00d",
                    Some("toolu_9"),
                    Some(Execution::Failed),
                    Some("Agent terminated early due to an API error: 429 Too Many Requests")
                ),
            ],
            "a background shell's notice names no sub-agent"
        );
    }

    /// A sub-agent's own transcript is all `isSidechain`, and all its own: its instruction
    /// begins its turn, and `end_turn` ends it.
    #[test]
    fn a_sub_agent_s_own_records_are_its_own() {
        let read = read(
            &reading(true),
            &records(&[
                json!({"type":"user","isSidechain":true,"agentId":"f00d","message":{"role":"user","content":"Build it"}}),
                json!({"type":"assistant","isSidechain":true,"agentId":"f00d","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Half done"}]}}),
            ]),
            Turn::Closed,
        );
        assert_eq!(turns(&read.events), ["began", "Finished"]);
        assert_eq!(said(&read.events), [(Voice::Agent, "Half done")]);
    }

    /// Words another session sent through a live session's inbox are `isMeta` with a
    /// `peer` origin, and begin a turn all the same; they are no person's words. Records as
    /// Claude Code 2.1.295 wrote them, signed out, 2026-10-09.
    #[test]
    fn a_peer_message_begins_a_turn() {
        let failed = |uuid: &str, parent: &str, at: &str| {
            json!({"parentUuid":parent,"type":"assistant","uuid":uuid,"timestamp":at,"isApiErrorMessage":true,"error":"authentication_failed",
                "message":{"id":uuid,"model":"<synthetic>","role":"assistant","stop_reason":"stop_sequence",
                    "usage":{"input_tokens":0,"output_tokens":0},"content":[{"type":"text","text":"Not logged in · Please run /login"}]}})
        };
        let read = events(&[
            json!({"parentUuid":null,"type":"user","message":{"role":"user","content":"say hi\n"},"uuid":"7a41","timestamp":"2026-10-09T09:50:13.972Z","origin":{"kind":"human"},"entrypoint":"cli"}),
            failed("da7c", "7a41", "2026-10-09T09:50:14.121Z"),
            json!({"parentUuid":"da7c","type":"system","subtype":"turn_duration","timestamp":"2026-10-09T09:50:14.147Z","uuid":"bb62","isMeta":false}),
            json!({"parentUuid":"bb62","type":"user","message":{"role":"user","content":"Another Claude session sent a message:\nsteery-delivery:d9 hello"},"isMeta":true,"uuid":"45f6","timestamp":"2026-10-09T09:50:31.351Z","origin":{"kind":"peer","from":"unknown"}}),
            failed("7620", "45f6", "2026-10-09T09:50:31.376Z"),
            json!({"parentUuid":"7620","type":"system","subtype":"turn_duration","timestamp":"2026-10-09T09:50:31.402Z","uuid":"49be","isMeta":false}),
        ]);
        assert_eq!(turns(&read), ["began", "Failed", "began", "Failed"]);
        assert_eq!(said(&read), [(Voice::Person, "say hi")]);
        assert!(
            read.iter()
                .all(|event| !matches!(event, Event::Tokens { .. })),
            "Claude Code's own record spent nothing"
        );
        assert!(read.iter().any(|event| matches!(
            event,
            Event::TurnEnded {
                failure: Some(Failure {
                    kind: FailureKind::SignIn,
                    http_status: None,
                    ..
                }),
                ..
            }
        )));
    }

    /// A compaction's summary is the harness's summary of earlier history.
    #[test]
    fn a_compaction_is_a_summary() {
        let read = events(&[
            json!({"type": "user", "isCompactSummary": true, "isVisibleInTranscriptOnly": true,
            "message": {"role": "user", "content": "This session is being continued. Summary: built the parser."}}),
        ]);
        assert_eq!(
            said(&read),
            [(
                Voice::Summary,
                "This session is being continued. Summary: built the parser."
            )]
        );
    }
}
