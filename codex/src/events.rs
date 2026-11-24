//! A Codex rollout's records as protocol events.
//!
//! A rollout is one JSON record per line, `{timestamp, type, payload}`: `session_meta`
//! opens it; `response_item` records carry what the model was sent and sent back
//! (messages, reasoning, calls and their output); `event_msg` records say what happened
//! (`task_started`, `task_complete`, `turn_aborted`, `token_count`, `thread_rolled_back`,
//! `thread_settings_applied`, `error`) and repeat the items as the terminal drew them;
//! `turn_context` says what a turn runs on; `compacted` is a compaction, whose summary is
//! of earlier history.
//! Only the items are read for words, so nothing is said twice.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_sdk::wire::harness::{
    Clip, Compactor, Count, Event, Execution, Failure, FailureKind, TurnState, Voice,
};

use crate::{calls, words};

/// The longest failure line said.
const LINE: usize = 600;

/// The most background sessions, and calls naming one, a read keeps.
const SESSIONS: usize = 64;

/// What a read carries from one record to the next, and from one read to the next in its
/// cursor: what the conversation runs on, the turn running and a failure it has not got
/// past, and the calls that start, wait on or close sub-agents.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    /// The model and effort last said as a `model` event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    said: Option<(String, Option<String>)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn: Option<String>,
    /// A failure the turn running has not got past.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failing: Option<Value>,
    /// When the newest quota snapshot's spent windows all reset, in milliseconds: Codex
    /// writes the refusing quota's snapshot (`token_count` `rate_limits`) before its error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    spent_until: Option<u64>,
    /// Calls on sub-agents not yet answered: their tool and the agent they name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    agents: BTreeMap<String, (String, Option<String>)>,
    /// Every turn Codex counts when it rolls turns back, oldest first: `p` where it
    /// opened with a person's words said, `x` where with none said (one agent's message
    /// to another, a harness's envelope).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    opened: String,
    /// Background sessions of unified exec, `(session id, the call that started it)`,
    /// oldest first and at most [`SESSIONS`]: a result that says `Process running with
    /// session ID 9055` left one, and later calls name it as `session_id`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sessions: Vec<(String, String)>,
    /// Calls begun naming a background session, until answered: `(call, starter)`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    waiting: Vec<(String, String)>,
    /// MCP calls that their tool refused or failed, as Codex's own event says, until their
    /// output is read: Codex does not keep a call's `success` in its output. The tool's
    /// words, empty where the event has none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    refused: BTreeMap<String, String>,
    /// The calls last ended as succeeded, with the length of their result, at most
    /// [`SESSIONS`]: an event that comes after the output corrects them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ended: Vec<(String, u64)>,
    /// Code-mode scripts not yet answered, oldest first and at most [`SESSIONS`]: the
    /// `exec` calls that run tools from a script. Codex files each tool the script calls as
    /// its own completed item, and the newest script open is the call it runs inside.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scripts: Vec<String>,
    /// A compaction read, held until Codex counts the context it left: the `token_count`
    /// it sends once the history is replaced, after the `world_state`, `turn_context` and
    /// `thread_settings_applied` it writes with it. Said then, at any other record, or at
    /// the read's end ([`State::settle`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compacting: Option<Event>,
}

/// What a rollback that does not say how much went is refused with: reading on would
/// give work somebody took back as theirs.
pub const UNCOUNTED: &str =
    "that rollout records a rollback but does not say how many turns the rollback removed";

/// Where a record is, for the id of the words it carries where Codex gives none.
pub struct Place<'a> {
    pub segment: usize,
    pub offset: u64,
    pub at: u64,
    /// History a fork took from its parent: what was compacted in it was the parent's.
    pub copied: bool,
    pub record: &'a Value,
}

impl State {
    /// The events one record says, in order; an error where it cannot be read on from.
    pub fn read(&mut self, place: &Place<'_>, out: &mut Vec<Event>) -> Result<(), &'static str> {
        let record = place.record;
        let at = place.at;
        let Some(payload) = record.get("payload") else {
            return Ok(());
        };
        let kind = text(payload, "type");
        let kept = matches!(
            (text(record, "type"), kind),
            (Some("world_state" | "turn_context"), _)
                | (
                    Some("event_msg"),
                    Some("thread_settings_applied" | "token_count")
                )
        );
        if !kept {
            self.settle(out);
        }
        match text(record, "type") {
            Some("session_meta") => {
                if let Some(provider) = text(payload, "model_provider") {
                    self.provider = Some(provider.to_owned());
                }
            }
            Some("turn_context") => {
                self.settings(
                    text(payload, "model"),
                    text(payload, "effort").or_else(|| text(payload, "reasoning_effort")),
                    at,
                    out,
                );
            }
            Some("compacted") => {
                if let Some(summary) = text(payload, "message").filter(|s| !s.trim().is_empty()) {
                    out.push(Event::Words {
                        at,
                        id: id(place, payload, 0),
                        voice: Voice::Summary,
                        text: summary.to_owned(),
                        message: None,
                        call: None,
                        clips: Vec::new(),
                    });
                }
                if !place.copied {
                    self.compacting = Some(compacted(place, payload));
                }
            }
            // One model response's own count, by the response it names: Codex writes one
            // for each response it saw complete, cache writes included where its service
            // says them.
            Some("token_usage_record") => {
                let (Some(response), Some(usage)) = (
                    text(payload, "response_id").filter(|id| !id.is_empty()),
                    payload.get("usage").filter(|usage| usage.is_object()),
                ) else {
                    return Ok(());
                };
                out.push(Event::Tokens {
                    at,
                    count: Count::Response,
                    response: Some(response.to_owned()),
                    model: self.model.clone(),
                    input: number(usage, "input_tokens"),
                    cached: number(usage, "cached_input_tokens"),
                    cache_write: usage
                        .get("cache_write_input_tokens")
                        .and_then(Value::as_u64),
                    output: number(usage, "output_tokens"),
                    reasoning: number(usage, "reasoning_output_tokens"),
                    context_left: None,
                });
            }
            Some("event_msg") => return self.event(kind, payload, at, out),
            Some("response_item") => self.item(kind, payload, place, out),
            _ => {}
        }
        Ok(())
    }

    fn settings(
        &mut self,
        model: Option<&str>,
        effort: Option<&str>,
        at: u64,
        out: &mut Vec<Event>,
    ) {
        if let Some(model) = model.filter(|m| !m.is_empty()) {
            self.model = Some(model.to_owned());
        }
        if let Some(effort) = effort.filter(|e| !e.is_empty()) {
            self.effort = Some(effort.to_owned());
        }
        let Some(model) = self.model.clone() else {
            return;
        };
        let now = (model.clone(), self.effort.clone());
        if self.said.as_ref() != Some(&now) {
            self.said = Some(now);
            out.push(Event::Model {
                at,
                model,
                provider: self.provider.clone(),
                thinking: self.effort.clone(),
            });
        }
    }

    /// Says the compaction held for the count after it, as it stands.
    pub fn settle(&mut self, out: &mut Vec<Event>) {
        out.extend(self.compacting.take());
    }

    fn event(
        &mut self,
        kind: Option<&str>,
        payload: &Value,
        at: u64,
        out: &mut Vec<Event>,
    ) -> Result<(), &'static str> {
        match kind {
            Some("task_started") => {
                self.turn = text(payload, "turn_id").map(str::to_owned);
                self.failing = None;
                out.push(Event::TurnBegan {
                    at,
                    turn: self.turn.clone(),
                });
            }
            Some("task_complete") => {
                let error = payload
                    .get("error")
                    .filter(|error| error.is_object())
                    .cloned()
                    .or_else(|| self.failing.take());
                let turn = text(payload, "turn_id")
                    .map(str::to_owned)
                    .or_else(|| self.turn.clone());
                let failure = error.as_ref().map(|error| self.failure(error, at));
                out.push(Event::TurnEnded {
                    at,
                    turn,
                    state: if failure.is_some() {
                        TurnState::Failed
                    } else {
                        TurnState::Finished
                    },
                    failure,
                });
                self.turn = None;
                self.failing = None;
            }
            Some("turn_aborted") => {
                out.push(Event::TurnEnded {
                    at,
                    turn: text(payload, "turn_id")
                        .map(str::to_owned)
                        .or_else(|| self.turn.take()),
                    state: TurnState::Interrupted,
                    failure: None,
                });
                self.turn = None;
                self.failing = None;
            }
            // A failure the turn may yet get past: it stands only if the turn ends on it.
            Some("error") => {
                self.failing = payload
                    .get("error")
                    .filter(|error| error.is_object())
                    .cloned()
                    .or_else(|| Some(payload.clone()));
            }
            Some("thread_settings_applied") => {
                let settings = payload.get("thread_settings").unwrap_or(&Value::Null);
                if let Some(provider) = text(settings, "model_provider_id") {
                    self.provider = Some(provider.to_owned());
                }
                let effort = text(settings, "reasoning_effort").or_else(|| {
                    settings
                        .pointer("/collaboration_mode/settings/reasoning_effort")
                        .and_then(Value::as_str)
                });
                self.settings(text(settings, "model"), effort, at, out);
            }
            Some("token_count") => {
                if let Some(limits) = payload.get("rate_limits").filter(|l| l.is_object()) {
                    self.spent_until = spent_until(limits);
                }
                let info = payload.get("info").filter(|info| info.is_object());
                let used = info
                    .and_then(|info| info.pointer("/last_token_usage/total_tokens"))
                    .and_then(Value::as_u64);
                // The count Codex makes of the history a compaction left, summary included.
                if let Some(Event::Compacted { tokens_after, .. }) = &mut self.compacting {
                    *tokens_after = used;
                }
                self.settle(out);
                let Some(info) = info else {
                    return Ok(());
                };
                let Some(total) = info.get("total_token_usage") else {
                    return Ok(());
                };
                let context_left = info
                    .get("model_context_window")
                    .and_then(Value::as_u64)
                    .filter(|window| *window > 0)
                    .zip(used)
                    .map(|(window, used)| (1.0 - used as f64 / window as f64).clamp(0.0, 1.0));
                out.push(Event::Tokens {
                    at,
                    count: Count::Total,
                    response: None,
                    model: self.model.clone(),
                    input: number(total, "input_tokens"),
                    cached: number(total, "cached_input_tokens"),
                    cache_write: total
                        .get("cache_write_input_tokens")
                        .and_then(Value::as_u64),
                    output: number(total, "output_tokens"),
                    reasoning: number(total, "reasoning_output_tokens"),
                    context_left,
                });
            }
            // Codex undoes turns in place: the items stay and this says how many of the
            // turns it counts went. A person's turn among them is one rolled back;
            // rolling back more than were read keeps what came before any.
            Some("thread_rolled_back") => {
                let gone = payload
                    .get("num_turns")
                    .and_then(Value::as_u64)
                    .ok_or(UNCOUNTED)?;
                let gone = usize::try_from(gone)
                    .unwrap_or(usize::MAX)
                    .min(self.opened.len());
                let kept = self.opened.len() - gone;
                let turns = self.opened[kept..].matches('p').count();
                self.opened.truncate(kept);
                if turns > 0 {
                    out.push(Event::Rollback {
                        at,
                        turns: u32::try_from(turns).unwrap_or(u32::MAX),
                    });
                }
            }
            // How an MCP call ended, in a legacy rollout (`result` is `{"Ok": CallToolResult}`
            // or `{"Err": message}`) and in a paginated one (the completed item).
            Some("mcp_tool_call_end") => {
                if let Some(call) = text(payload, "call_id")
                    && let Some(error) = payload.get("result").and_then(mcp_failed)
                {
                    self.mcp_failed(call, error, at, out);
                }
            }
            Some("item_completed") => {
                if let Some(item) = payload.get("item")
                    && matches!(text(item, "type"), Some("McpToolCall" | "mcpToolCall"))
                    && let Some(call) = text(item, "id")
                {
                    let script = self
                        .scripts
                        .last()
                        .filter(|_| call.starts_with(SCRIPTED))
                        .cloned();
                    match script {
                        Some(script) => scripted(payload, item, call, &script, at, out),
                        None => {
                            if let Some(error) = mcp_item_failed(item) {
                                self.mcp_failed(call, error, at, out);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// An MCP call that its tool refused or failed, with the tool's words if it said any:
    /// the call fails where its output was read already, else where it is.
    fn mcp_failed(&mut self, call: &str, error: Option<String>, at: u64, out: &mut Vec<Event>) {
        match self.ended.iter().position(|(id, _)| id == call) {
            Some(index) => {
                let (_, chars) = self.ended.remove(index);
                out.push(Event::CallEnded {
                    at,
                    id: call.to_owned(),
                    ok: false,
                    chars,
                    tail: None,
                    waited: Vec::new(),
                    error,
                });
            }
            None => {
                if self.refused.len() >= SESSIONS
                    && let Some(oldest) = self.refused.keys().next().cloned()
                {
                    self.refused.remove(&oldest);
                }
                self.refused
                    .insert(call.to_owned(), error.unwrap_or_default());
            }
        }
    }

    fn item(
        &mut self,
        kind: Option<&str>,
        payload: &Value,
        place: &Place<'_>,
        out: &mut Vec<Event>,
    ) {
        let at = place.at;
        match kind {
            Some("message") => {
                let person = match text(payload, "role") {
                    Some("user") => true,
                    Some("assistant") => false,
                    _ => return,
                };
                let blocks = payload
                    .get("content")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let opens = words::opens(person, blocks);
                if person {
                    let said = person_words(blocks);
                    if opens {
                        self.opened.push(if said.is_some() { 'p' } else { 'x' });
                    }
                    if let Some((said, clips)) = said {
                        out.push(Event::Words {
                            at,
                            id: id(place, payload, 0),
                            voice: Voice::Person,
                            text: said,
                            message: None,
                            call: None,
                            clips,
                        });
                    }
                    return;
                }
                if opens {
                    // One agent's message to another: a turn Codex counts, no words.
                    self.opened.push('x');
                    return;
                }
                let said: Vec<&str> = blocks
                    .iter()
                    .filter_map(|block| text(block, "text"))
                    .filter(|said| !said.trim().is_empty())
                    .collect();
                if !said.is_empty() {
                    self.failing = None;
                    out.push(Event::Words {
                        at,
                        id: id(place, payload, 0),
                        voice: Voice::Agent,
                        text: said.join("\n\n"),
                        message: None,
                        call: None,
                        clips: Vec::new(),
                    });
                }
            }
            Some("reasoning") => {
                let parts = payload
                    .get("summary")
                    .and_then(Value::as_array)
                    .filter(|parts| !parts.is_empty())
                    .or_else(|| payload.get("content").and_then(Value::as_array));
                let thought: Vec<&str> = parts
                    .into_iter()
                    .flatten()
                    .filter_map(|part| text(part, "text").or_else(|| text(part, "thinking")))
                    .filter(|said| !said.trim().is_empty())
                    .collect();
                if !thought.is_empty() {
                    out.push(Event::Words {
                        at,
                        id: id(place, payload, 0),
                        voice: Voice::Thought,
                        text: thought.join("\n\n"),
                        message: None,
                        call: None,
                        clips: Vec::new(),
                    });
                }
            }
            Some("function_call" | "custom_tool_call" | "local_shell_call") => {
                let Some(call_id) = text(payload, "call_id").or_else(|| text(payload, "id")) else {
                    return;
                };
                self.failing = None;
                let name = text(payload, "name").unwrap_or(match kind {
                    Some("local_shell_call") => "local_shell",
                    _ => "a tool",
                });
                let tool = calls::spelled(name, text(payload, "namespace"));
                let input = payload
                    .get("arguments")
                    .or_else(|| payload.get("input"))
                    .or_else(|| payload.get("action"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let call = calls::call(call_id, &tool, input);
                if let Some(session) = addressed(&call.input)
                    && let Some((_, starter)) = self.sessions.iter().find(|(s, _)| *s == session)
                {
                    let starter = starter.clone();
                    keep(&mut self.waiting, call_id.to_owned(), starter);
                }
                if matches!(
                    call.tool.as_str(),
                    "spawn_agent" | "wait_agent" | "close_agent"
                ) {
                    let target = ["target", "id", "agent_id"]
                        .into_iter()
                        .find_map(|key| text(&call.input, key).map(str::to_owned));
                    self.agents
                        .insert(call_id.to_owned(), (call.tool.clone(), target));
                }
                if kind == Some("custom_tool_call") && call.tool == SCRIPT {
                    if self.scripts.len() >= SESSIONS {
                        self.scripts.remove(0);
                    }
                    self.scripts.push(call_id.to_owned());
                }
                out.push(Event::CallBegan { at, call });
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                let Some(call_id) = text(payload, "call_id") else {
                    return;
                };
                self.scripts.retain(|script| script != call_id);
                let output = output_text(payload.get("output"));
                if let Some((tool, target)) = self.agents.remove(call_id) {
                    self.agents_said(&tool, target, call_id, &output, at, out);
                }
                let waited: Vec<String> = self
                    .waiting
                    .iter()
                    .position(|(call, _)| call == call_id)
                    .map(|at| self.waiting.remove(at).1)
                    .into_iter()
                    .collect();
                match background_session(&output) {
                    Some(session) if waited.is_empty() => {
                        self.sessions.retain(|(s, _)| *s != session);
                        keep(&mut self.sessions, session, call_id.to_owned());
                    }
                    // Still running: the session stays its starter's.
                    Some(_) => {}
                    None => self
                        .sessions
                        .retain(|(_, starter)| !waited.contains(starter)),
                }
                let refusal = self.refused.remove(call_id);
                let ok = refusal.is_none() && succeeded(payload.get("output"), &output);
                let chars = output.chars().count() as u64;
                if ok {
                    if self.ended.len() >= SESSIONS {
                        self.ended.remove(0);
                    }
                    self.ended.push((call_id.to_owned(), chars));
                }
                out.push(Event::CallEnded {
                    at,
                    id: call_id.to_owned(),
                    ok,
                    chars,
                    tail: tail(&output),
                    waited,
                    error: refusal.filter(|error| !error.is_empty()),
                });
                if !output.trim().is_empty() {
                    out.push(Event::Words {
                        at,
                        id: format!("{call_id}:out"),
                        voice: Voice::Tool,
                        text: output,
                        message: None,
                        call: Some(call_id.to_owned()),
                        clips: Vec::new(),
                    });
                }
            }
            // A web search is written once it has come back: begun and ended at once.
            Some("web_search_call") => {
                let Some(call_id) = text(payload, "id") else {
                    return;
                };
                let input = payload.get("action").cloned().unwrap_or(Value::Null);
                out.push(Event::CallBegan {
                    at,
                    call: calls::call(call_id, "web_search", input),
                });
                out.push(Event::CallEnded {
                    at,
                    id: call_id.to_owned(),
                    ok: text(payload, "status") != Some("failed"),
                    chars: 0,
                    tail: None,
                    waited: Vec::new(),
                    error: None,
                });
            }
            _ => {}
        }
    }

    /// What a lead's call said of its sub-agents: `spawn_agent` the one it started,
    /// `wait_agent` each one's `status` by its id (`{"completed": …}`, `{"errored": why}`,
    /// `"shutdown"`), `close_agent` what its target was doing as it was closed.
    fn agents_said(
        &self,
        tool: &str,
        target: Option<String>,
        call_id: &str,
        output: &str,
        at: u64,
        out: &mut Vec<Event>,
    ) {
        let Ok(said) = serde_json::from_str::<Value>(output) else {
            return;
        };
        match tool {
            "spawn_agent" => {
                if let Some(agent) = text(&said, "agent_id") {
                    out.push(Event::SubAgent {
                        at,
                        external_id: agent.to_owned(),
                        call: Some(call_id.to_owned()),
                        state: Some(Execution::Running),
                        error: None,
                    });
                }
            }
            "wait_agent" => {
                for (agent, status) in said
                    .get("status")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    if let Some((state, error)) = status_of(status) {
                        out.push(Event::SubAgent {
                            at,
                            external_id: agent.clone(),
                            call: None,
                            state: Some(state),
                            error,
                        });
                    }
                }
            }
            "close_agent" => {
                let Some(target) = target else { return };
                let closed = match said.get("previous_status") {
                    Some(Value::String(running))
                        if matches!(running.as_str(), "running" | "pending_init") =>
                    {
                        Some((Execution::Interrupted, None))
                    }
                    Some(status) => status_of(status),
                    None => None,
                };
                if let Some((state, error)) = closed {
                    out.push(Event::SubAgent {
                        at,
                        external_id: target,
                        call: None,
                        state: Some(state),
                        error,
                    });
                }
            }
            _ => {}
        }
    }

    /// A failure as Codex wrote it: the rollout's snake_case (`codex_error_info`,
    /// `http_status_code`) or the app server's camelCase (`codexErrorInfo`,
    /// `httpStatusCode`), with what the turn ran on; a limit retries once its spent
    /// quota resets, if that is still ahead.
    fn failure(&self, error: &Value, at: u64) -> Failure {
        let mut failure = failure_of(error);
        failure.provider = self.provider.clone();
        failure.model = self.model.clone();
        failure.thinking = self.effort.clone();
        if failure.kind == FailureKind::RateLimit {
            failure.retry_at = self.spent_until.filter(|reset| *reset > at);
        }
        failure
    }
}

/// When every window of a quota snapshot that is spent (`used_percent` at 100) resets,
/// in milliseconds: none spent, or one spent with no reset, says nothing. The rollout
/// spells it in snake_case, the app server in camelCase (`usedPercent`, `resetsAt`).
pub fn spent_until(limits: &Value) -> Option<u64> {
    let mut until = None;
    for window in ["primary", "secondary"] {
        let Some(window) = limits.get(window).filter(|w| w.is_object()) else {
            continue;
        };
        if window
            .get("used_percent")
            .or_else(|| window.get("usedPercent"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            < 100.0
        {
            continue;
        }
        let reset = window
            .get("resets_at")
            .or_else(|| window.get("resetsAt"))
            .and_then(Value::as_u64)?
            .checked_mul(1000)?;
        until = until.max(Some(reset));
    }
    until
}

/// A failure from one error object, without what the turn ran on. Never read from
/// wording: by its status, else Codex's own code for it.
pub fn failure_of(error: &Value) -> Failure {
    let info = error
        .get("codex_error_info")
        .or_else(|| error.get("codexErrorInfo"));
    let code = info.and_then(Value::as_str).map(str::to_owned).or_else(|| {
        info.and_then(Value::as_object)
            .and_then(|map| map.keys().next().cloned())
    });
    let http_status = info.and_then(Value::as_object).and_then(|map| {
        map.values().find_map(|value| {
            value
                .get("http_status_code")
                .or_else(|| value.get("httpStatusCode"))
                .and_then(Value::as_u64)
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| (100..=599).contains(n))
        })
    });
    let code_is = |codes: &[&str]| code.as_deref().is_some_and(|code| codes.contains(&code));
    let limited = http_status.is_some_and(|status| matches!(status, 429 | 503 | 529))
        || code_is(&[
            "rateLimitExceeded",
            "rate_limit_exceeded",
            "usageLimitExceeded",
            "usage_limit_exceeded",
        ]);
    let signed_out = match http_status {
        Some(status) => status == 401,
        None => code_is(&["unauthorized"]),
    };
    let context_full = code_is(&["contextWindowExceeded", "context_window_exceeded"]);
    let service = http_status.is_some()
        || limited
        || code_is(&[
            "responseTooManyFailedAttempts",
            "response_too_many_failed_attempts",
            "responseStreamDisconnected",
            "response_stream_disconnected",
            "responseStreamConnectionFailed",
            "response_stream_connection_failed",
            "httpConnectionFailed",
            "http_connection_failed",
            "contextWindowExceeded",
            "context_window_exceeded",
        ]);
    Failure {
        kind: if signed_out {
            FailureKind::SignIn
        } else if limited {
            FailureKind::RateLimit
        } else if context_full {
            FailureKind::ContextFull
        } else {
            FailureKind::Other
        },
        line: one_line(text(error, "message").unwrap_or("Codex failed the turn")),
        service,
        http_status,
        code,
        retry_at: None,
        provider: None,
        model: None,
        thinking: None,
    }
}

/// A line as a person reads it: its whitespace one space, at most [`LINE`] characters.
pub fn one_line(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(LINE)
        .collect()
}

/// One sub-agent's status as Codex spells it in a lead's transcript.
fn status_of(status: &Value) -> Option<(Execution, Option<String>)> {
    match status {
        Value::Object(status) if status.contains_key("completed") => {
            Some((Execution::Finished, None))
        }
        Value::Object(status) => status.get("errored").map(|why| {
            (
                Execution::Failed,
                why.as_str().map(one_line).filter(|why| !why.is_empty()),
            )
        }),
        Value::String(word) if matches!(word.as_str(), "shutdown" | "interrupted") => {
            Some((Execution::Interrupted, None))
        }
        _ => None,
    }
}

/// What a person typed in one message, and what it carried: Codex's own fragments, its
/// wrapping of a picture, and a harness's envelope left out; the files it lists above
/// their words taken as clips.
pub fn person_words(blocks: &[Value]) -> Option<(String, Vec<Clip>)> {
    let mut pictures = words::Pictures::default();
    let mut clips = Vec::new();
    let mut said = Vec::new();
    for (at, block) in blocks.iter().enumerate() {
        words::carried(block, &mut pictures, &mut clips);
        if text(block, "type") != Some("input_text") {
            continue;
        }
        let Some(typed) = text(block, "text") else {
            continue;
        };
        if words::contextual(typed) || words::wraps_picture(blocks, at, typed) {
            continue;
        }
        if words::enveloped(typed) {
            return None;
        }
        let typed = words::requested(typed).trim();
        if !typed.is_empty() {
            said.push(typed.to_owned());
        }
    }
    let said = said.join("\n\n");
    (!said.is_empty() || !clips.is_empty()).then_some((said, clips))
}

/// A `compacted` record as Codex writes it: named by the context window it opens
/// (`window_id`), else where it is; who made it and why unrecorded.
/// `latest_token_usage_record` is the last response's before it, whose `total_tokens` Codex
/// counts as the context's; the context after it is the `token_count` Codex sends next
/// ([`State::settle`]).
fn compacted(place: &Place<'_>, payload: &Value) -> Event {
    Event::Compacted {
        at: place.at,
        id: text(payload, "window_id")
            .filter(|id| !id.is_empty())
            .map_or_else(
                || format!("{}:{}", place.segment, place.offset),
                str::to_owned,
            ),
        by: Compactor::Harness { reason: None },
        tokens_before: payload
            .pointer("/latest_token_usage_record/usage/total_tokens")
            .and_then(Value::as_u64),
        tokens_after: None,
    }
}

/// The id of the words a record carries: Codex's own where it gives one, else where the
/// record is.
fn id(place: &Place<'_>, payload: &Value, n: usize) -> String {
    match text(payload, "id") {
        Some(id) if !id.is_empty() => id.to_owned(),
        _ => format!("{}:{}:{n}", place.segment, place.offset),
    }
}

/// A call's output as text: a string, or the content items Codex writes it as.
pub fn output_text(output: Option<&Value>) -> String {
    match output {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| {
                matches!(
                    text(item, "type"),
                    Some("input_text" | "text" | "output_text")
                )
            })
            .filter_map(|item| text(item, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Object(fields)) => fields
            .get("content")
            .map(|content| output_text(Some(content)))
            .unwrap_or_else(|| Value::Object(fields.clone()).to_string()),
        _ => String::new(),
    }
}

/// Whether a call succeeded, as its output says: a command's exit code in the lines Codex
/// opens its output with, or an output object's own `success`.
fn succeeded(output: Option<&Value>, text: &str) -> bool {
    if let Some(success) = output
        .and_then(|output| output.get("success"))
        .and_then(Value::as_bool)
    {
        return success;
    }
    for line in text.lines().take(8) {
        let code = line
            .strip_prefix("Process exited with code ")
            .or_else(|| line.strip_prefix("Exit code: "));
        if let Some(code) = code.and_then(|code| code.trim().parse::<i64>().ok()) {
            return code == 0;
        }
    }
    true
}

/// The background session a result says it left running: `Process running with session
/// ID 9055`, among its last lines.
fn background_session(output: &str) -> Option<String> {
    output.lines().rev().take(8).find_map(|line| {
        let id = line
            .trim()
            .strip_prefix("Process running with session ID ")?;
        let id = id.trim();
        (!id.is_empty()).then(|| id.to_owned())
    })
}

/// The session a call names: `session_id` in its input, a number or text.
fn addressed(input: &Value) -> Option<String> {
    match input.as_object()?.get("session_id")? {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// `value` under `key` at the end of `list`, the oldest dropped past [`SESSIONS`].
fn keep(list: &mut Vec<(String, String)>, key: String, value: String) {
    if list.len() >= SESSIONS {
        list.remove(0);
    }
    list.push((key, value));
}

/// Codex's own words before the tool's, in the failure of an MCP call that did not get to its
/// server.
const TOOL_CALL_ERROR: &str = "tool call error: ";

/// The failure of an MCP call as its tool said it: `message`, or the text of the result's
/// `content` where there is none; none where it says nothing.
pub fn mcp_error(message: Option<&str>, content: Option<&Value>) -> Option<String> {
    // Codex can write the tool's whole `CallToolResult` as the message: its words are its
    // content's.
    let said = match message {
        Some(message) => serde_json::from_str::<Value>(message)
            .ok()
            .and_then(|result| {
                result
                    .get("content")
                    .map(|content| output_text(Some(content)))
            })
            .unwrap_or_else(|| message.to_owned()),
        None => output_text(content),
    };
    let said = said.trim();
    let said = said.strip_prefix(TOOL_CALL_ERROR).unwrap_or(said).trim();
    (!said.is_empty()).then(|| said.to_owned())
}

/// Whether a `CallToolResult` says it is an error (`isError`; `is_error` in older rollouts).
fn flagged(result: &Value) -> bool {
    result
        .get("isError")
        .or_else(|| result.get("is_error"))
        .and_then(Value::as_bool)
        == Some(true)
}

/// How an MCP call ended as `mcp_tool_call_end` says, by its `result`: none where it
/// succeeded; else its failure's words, if it has any.
fn mcp_failed(result: &Value) -> Option<Option<String>> {
    if let Some(message) = result.get("Err") {
        return Some(mcp_error(message.as_str(), None));
    }
    let result = result.get("Ok")?;
    flagged(result).then(|| mcp_error(None, result.get("content")))
}

/// The tool of Codex's code mode that runs a script, which calls tools from it.
const SCRIPT: &str = "exec";

/// What opens the id Codex gives a tool called from a code-mode script (`exec-<uuid>`).
const SCRIPTED: &str = "exec-";

/// An MCP tool a code-mode script called, as its completed item says: a call of its own
/// inside the script's (`parent`), begun and ended when the record says (`started_at_ms`,
/// `completed_at_ms` beside the item), failed with the tool's words where the tool refused
/// or failed. The script's own outcome is Codex's.
fn scripted(record: &Value, item: &Value, id: &str, script: &str, at: u64, out: &mut Vec<Event>) {
    let server = text(item, "server").unwrap_or_default();
    let tool = text(item, "tool").unwrap_or_default();
    let input = item.get("arguments").cloned().unwrap_or(Value::Null);
    let mut call = calls::call(id, &format!("mcp__{server}__{tool}"), input);
    call.parent = Some(script.to_owned());
    let began = record
        .get("started_at_ms")
        .and_then(Value::as_u64)
        .unwrap_or(at);
    let ended = record
        .get("completed_at_ms")
        .and_then(Value::as_u64)
        .unwrap_or(at);
    out.push(Event::CallBegan { at: began, call });
    let failed = mcp_item_failed(item);
    let result = item.get("result").filter(|result| !result.is_null());
    let said = match &failed {
        Some(Some(error)) => error.clone(),
        _ => output_text(result.and_then(|result| result.get("content"))),
    };
    out.push(Event::CallEnded {
        at: ended,
        id: id.to_owned(),
        ok: failed.is_none(),
        chars: said.chars().count() as u64,
        tail: tail(&said),
        waited: Vec::new(),
        error: failed.flatten(),
    });
    if !said.trim().is_empty() {
        out.push(Event::Words {
            at: ended,
            id: format!("{id}:out"),
            voice: Voice::Tool,
            text: said,
            message: None,
            call: Some(id.to_owned()),
            clips: Vec::new(),
        });
    }
}

/// How an MCP call ended as its completed item says (`status`, `result`, `error`): none where
/// it succeeded; else its failure's words, if it has any.
pub fn mcp_item_failed(item: &Value) -> Option<Option<String>> {
    let message = item.pointer("/error/message").and_then(Value::as_str);
    let result = item.get("result").filter(|result| !result.is_null());
    let failed =
        message.is_some() || result.is_some_and(flagged) || text(item, "status") == Some("failed");
    failed.then(|| mcp_error(message, result.and_then(|result| result.get("content"))))
}

/// Events as a protocol older than `call_ended`'s `error` reads them: no failure's words.
pub fn without_error(events: &mut [Event]) {
    for event in events {
        if let Event::CallEnded { error, .. } = event {
            *error = None;
        }
    }
}

/// Events as a protocol older than `waited` reads them: no call waited on any.
pub fn without_waited(events: &mut [Event]) {
    for event in events {
        if let Event::CallEnded { waited, .. } = event {
            waited.clear();
        }
    }
}

/// The last 8 lines of a result.
pub fn tail(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let kept = lines[lines.len().saturating_sub(8)..].join("\n");
    (!kept.trim().is_empty()).then_some(kept)
}

pub fn number(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

pub fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// A record's time, in milliseconds since 1970.
pub fn stamped(record: &Value) -> Option<u64> {
    text(record, "timestamp").and_then(millis)
}

/// An RFC 3339 time in milliseconds since 1970.
pub fn millis(at: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(records: &[Value]) -> Vec<Event> {
        let mut state = State::default();
        let mut out = Vec::new();
        for (n, record) in records.iter().enumerate() {
            let _ = state.read(
                &Place {
                    segment: 0,
                    offset: n as u64,
                    at: stamped(record).unwrap_or(0),
                    copied: false,
                    record,
                },
                &mut out,
            );
        }
        state.settle(&mut out);
        out
    }

    /// Each response Codex saw complete is counted on its own, by its id, cache writes
    /// included; a compaction says the context before it and the count Codex makes of
    /// what it left, past the records Codex writes with it.
    #[test]
    fn responses_count_on_their_own_and_compactions_say_what_they_left() {
        let events = read(&[
            json!({"timestamp":"2026-10-09T09:40:00Z","type":"turn_context","payload":{"turn_id":"t","model":"gpt-6.1-sol"}}),
            json!({"timestamp":"2026-10-09T09:40:01Z","type":"token_usage_record","payload":{"thread_id":"x","turn_id":"t","session_id":"x","root_turn_id":"t","response_id":"resp_1","usage":{"input_tokens":100,"cached_input_tokens":40,"cache_write_input_tokens":60,"output_tokens":10,"reasoning_output_tokens":5,"total_tokens":110},"turn_token_usage":{},"thread_token_usage":{}}}),
            json!({"timestamp":"2026-10-09T09:40:01Z","type":"token_usage_record","payload":{"response_id":"resp_0","usage":{"input_tokens":7,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":8}}}),
            json!({"timestamp":"2026-10-09T09:40:02Z","type":"compacted","payload":{"message":"so far","window_id":"w2","latest_token_usage_record":{"usage":{"total_tokens":9000}}}}),
            json!({"timestamp":"2026-10-09T09:40:02Z","type":"world_state","payload":{}}),
            json!({"timestamp":"2026-10-09T09:40:02Z","type":"turn_context","payload":{"turn_id":"t","model":"gpt-6.1-sol"}}),
            json!({"timestamp":"2026-10-09T09:40:02Z","type":"event_msg","payload":{"type":"thread_settings_applied"}}),
            json!({"timestamp":"2026-10-09T09:40:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":107,"cached_input_tokens":40,"cache_write_input_tokens":60,"output_tokens":11,"reasoning_output_tokens":5,"total_tokens":118},"last_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":0,"reasoning_output_tokens":0,"total_tokens":1200},"model_context_window":10000}}}),
            json!({"timestamp":"2026-10-09T09:40:04Z","type":"compacted","payload":{"message":"again","window_id":"w3"}}),
        ]);
        let counts: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::Tokens {
                    count,
                    response,
                    input,
                    cached,
                    cache_write,
                    output,
                    reasoning,
                    ..
                } => Some((
                    *count,
                    response.as_deref(),
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
            counts,
            [
                (Count::Response, Some("resp_1"), 100, 40, Some(60), 10, 5),
                (Count::Response, Some("resp_0"), 7, 0, None, 1, 0),
                (Count::Total, None, 107, 40, Some(60), 11, 5),
            ]
        );
        let compactions: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::Compacted {
                    id,
                    tokens_before,
                    tokens_after,
                    ..
                } => Some((id.as_str(), *tokens_before, *tokens_after)),
                _ => None,
            })
            .collect();
        assert_eq!(
            compactions,
            [("w2", Some(9000), Some(1200)), ("w3", None, None)]
        );
        let total = events.iter().position(|event| {
            matches!(
                event,
                Event::Tokens {
                    count: Count::Total,
                    ..
                }
            )
        });
        let first = events
            .iter()
            .position(|event| matches!(event, Event::Compacted { .. }));
        assert_eq!(first.zip(total).map(|(c, t)| c + 1 == t), Some(true));
    }

    fn failure(events: &[Event]) -> Option<&Failure> {
        events.iter().find_map(|event| match event {
            Event::TurnEnded { failure, .. } => failure.as_ref(),
            _ => None,
        })
    }

    /// Codex's own rollout keeps a spent quota snake_case: it reads as a limit, with the
    /// session's provider and the failed turn's settings (`01a0edc5…`, 2026-09-29).
    #[test]
    fn codex_rollout_quota_is_limited_with_its_provider_and_settings() {
        let records: Vec<Value> = include_str!("../fixtures/usage-limit.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let events = read(&records);
        let Some(Event::TurnEnded {
            turn, state, at, ..
        }) = events.last()
        else {
            panic!("{events:?}")
        };
        assert_eq!(*state, TurnState::Failed);
        assert_eq!(
            turn.as_deref(),
            Some("01a0ee5e-3ea0-7793-b8c3-f85d273db192")
        );
        assert_eq!(*at, millis("2026-09-29T19:12:59.591Z").unwrap());
        let failure = failure(&events).unwrap();
        assert_eq!(failure.code.as_deref(), Some("usage_limit_exceeded"));
        assert_eq!(failure.kind, FailureKind::RateLimit);
        assert!(failure.service);
        assert_eq!(failure.provider.as_deref(), Some("cpa-gui"));
        assert_eq!(failure.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(failure.thinking.as_deref(), Some("low"));
        // Trimmed of its quota snapshot, the reset is said only in words: never read.
        assert_eq!(failure.retry_at, None);

        // Codex 0.160 writes the refusing quota's snapshot before its error: the limit
        // retries once every spent window resets, a window not spent never holds it back,
        // and a reset already past, or the snapshot of another turn's success, says none.
        let quota = |primary: f64, primary_reset: u64, secondary: f64| {
            json!({"timestamp":"2026-09-29T19:12:58Z","type":"event_msg","payload":{"type":"token_count","info":null,
            "rate_limits":{"limit_id":"codex","primary":{"used_percent":primary,"window_minutes":300,"resets_at":primary_reset},
            "secondary":{"used_percent":secondary,"window_minutes":10080,"resets_at":1_791_500_000}}}})
        };
        let ending = records.last().unwrap().clone();
        let limited = |snapshot: Value| {
            let mut records = records[..2].to_vec();
            records.extend([snapshot, ending.clone()]);
            self::failure(&read(&records)).unwrap().retry_at
        };
        assert_eq!(
            limited(quota(100.0, 1_791_000_000, 40.0)),
            Some(1_791_000_000_000)
        );
        assert_eq!(
            limited(quota(100.0, 1_791_000_000, 100.0)),
            Some(1_791_500_000_000)
        );
        assert_eq!(limited(quota(100.0, 1_000_000_000, 40.0)), None);
        assert_eq!(limited(quota(64.0, 1_791_000_000, 10.0)), None);
    }

    /// A rollout nests the status snake_case too (`01a0d273…`, 2026-09-29 10:24:14).
    #[test]
    fn codex_rollout_retries_given_up_keep_their_status() {
        let events = read(&[
            json!({"timestamp":"2026-09-29T10:24:14.424Z","type":"event_msg","payload":{
            "type":"task_complete","turn_id":"t","last_agent_message":null,
            "error":{"message":"exceeded retry limit, last status: 429 Too Many Requests",
            "codex_error_info":{"response_too_many_failed_attempts":{"http_status_code":429}}}}}),
        ]);
        let failure = failure(&events).unwrap();
        assert_eq!(
            failure.code.as_deref(),
            Some("response_too_many_failed_attempts")
        );
        assert_eq!(failure.http_status, Some(429));
        assert_eq!(failure.kind, FailureKind::RateLimit);
        assert!(failure.service);
    }

    /// The app server's failed turn, camelCase, from a turn Steery ran; and a 401 is a
    /// harness that cannot sign in, as Codex 0.162 says it with no credentials.
    #[test]
    fn app_server_failures_keep_their_status_and_a_401_is_a_sign_in() {
        let limited = failure_of(
            &json!({"message":"exceeded retry limit, last status: 429 Too Many Requests",
            "codexErrorInfo":{"responseTooManyFailedAttempts":{"httpStatusCode":429}}}),
        );
        assert_eq!(limited.kind, FailureKind::RateLimit);
        assert_eq!(
            limited.code.as_deref(),
            Some("responseTooManyFailedAttempts")
        );
        let signed_out = failure_of(
            &json!({"message":"unexpected status 401 Unauthorized: Missing bearer",
            "codex_error_info":{"http_connection_failed":{"http_status_code":401}}}),
        );
        assert_eq!(signed_out.kind, FailureKind::SignIn);
        assert_eq!(signed_out.http_status, Some(401));
        assert_eq!(
            failure_of(&json!({"message":"x","codexErrorInfo":"unauthorized"})).kind,
            FailureKind::SignIn
        );
        assert_eq!(
            failure_of(&json!({"message":"x","codex_error_info":"context_window_exceeded"})).kind,
            FailureKind::ContextFull
        );
    }

    /// A failed turn is failed by its final outcome; one that carried on past an error
    /// finished. The next turn opens with `task_started`.
    #[test]
    fn a_failure_the_turn_got_past_is_no_failure() {
        let events = read(&[
            json!({"timestamp":"2026-09-22T23:18:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"a"}}),
            json!({"timestamp":"2026-09-22T23:18:01Z","type":"event_msg","payload":{"type":"error","message":"stream disconnected"}}),
            json!({"timestamp":"2026-09-22T23:18:02Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}),
            json!({"timestamp":"2026-09-22T23:18:03Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"a","last_agent_message":"done"}}),
            json!({"timestamp":"2026-09-22T23:19:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"b"}}),
            json!({"timestamp":"2026-09-22T23:19:01Z","type":"event_msg","payload":{"type":"turn_aborted","turn_id":"b","reason":"interrupted"}}),
        ]);
        let ended: Vec<TurnState> = events
            .iter()
            .filter_map(|event| match event {
                Event::TurnEnded { state, .. } => Some(*state),
                _ => None,
            })
            .collect();
        assert_eq!(ended, [TurnState::Finished, TurnState::Interrupted]);
    }

    /// A command that returns while it runs says `Process running with session ID …`; the
    /// `write_stdin` calls that name that `session_id` waited on it, until it exits.
    #[test]
    fn a_poll_of_a_background_session_waited_on_the_call_that_started_it() {
        let call = |id: &str, name: &str, arguments: &str| json!({"timestamp":"2026-10-09T09:40:02Z","type":"response_item","payload":{"type":"function_call","name":name,"call_id":id,"arguments":arguments}});
        let output = |id: &str, text: &str| json!({"timestamp":"2026-10-09T09:40:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":id,"output":text}});
        let events = read(&[
            call("c1", "exec_command", r#"{"cmd":"deploy/ship.sh"}"#),
            output(
                "c1",
                "Wall time: 1.0 seconds\nProcess running with session ID 9055\nOutput:\nbuilding",
            ),
            call(
                "c2",
                "write_stdin",
                r#"{"session_id":9055,"chars":"","yield_time_ms":30000}"#,
            ),
            output(
                "c2",
                "Wall time: 30.0 seconds\nProcess running with session ID 9055\nOutput:\nstill",
            ),
            call("c3", "write_stdin", r#"{"session_id":9055,"chars":""}"#),
            output(
                "c3",
                "Wall time: 5.0 seconds\nProcess exited with code 0\nOutput:\ndone",
            ),
            call("c4", "write_stdin", r#"{"session_id":9055,"chars":""}"#),
            output("c4", "write_stdin failed: Unknown process id 9055"),
        ]);
        let waited: Vec<(&str, &[String])> = events
            .iter()
            .filter_map(|event| match event {
                Event::CallEnded { id, waited, .. } => Some((id.as_str(), waited.as_slice())),
                _ => None,
            })
            .collect();
        let starter = ["c1".to_owned()];
        assert_eq!(
            waited,
            [
                ("c1", &[][..]),
                ("c2", &starter[..]),
                ("c3", &starter[..]),
                ("c4", &[][..])
            ]
        );
    }

    /// Words are the person's, the agent's and its reasoning; Codex's own fragments are
    /// nobody's; calls begin and end with their result, and a running total is counted.
    #[test]
    fn a_rollout_reads_as_its_words_calls_and_counts() {
        let events = read(&[
            json!({"timestamp":"2026-10-09T09:39:59Z","type":"session_meta","payload":{"id":"x","model_provider":"openai"}}),
            json!({"timestamp":"2026-10-09T09:39:59Z","type":"turn_context","payload":{"turn_id":"t","model":"gpt-6.1-sol","effort":"high"}}),
            json!({"timestamp":"2026-10-09T09:39:59Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n<cwd>/w</cwd>\n</environment_context>"}]}}),
            json!({"timestamp":"2026-10-09T09:39:59Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]}}),
            json!({"timestamp":"2026-10-09T09:40:00Z","type":"response_item","payload":{"type":"message","id":"msg_1","role":"user","content":[{"type":"input_text","text":"say hi"}]}}),
            json!({"timestamp":"2026-10-09T09:40:01Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"thinking it over"}]}}),
            json!({"timestamp":"2026-10-09T09:40:02Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","call_id":"c1","arguments":"{\"cmd\":\"false\"}"}}),
            json!({"timestamp":"2026-10-09T09:40:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"Process exited with code 1\nOutput:\nnope"}}),
            json!({"timestamp":"2026-10-09T09:40:04Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}),
            json!({"timestamp":"2026-10-09T09:40:05Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":300,"cached_input_tokens":200,"output_tokens":30,"reasoning_output_tokens":12},"last_token_usage":{"total_tokens":1000},"model_context_window":10000}}}),
        ]);
        let said: Vec<(Voice, &str)> = events
            .iter()
            .filter_map(|event| match event {
                Event::Words { voice, text, .. } => Some((*voice, text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            said,
            [
                (Voice::Person, "say hi"),
                (Voice::Thought, "thinking it over"),
                (Voice::Tool, "Process exited with code 1\nOutput:\nnope"),
                (Voice::Agent, "hi"),
            ]
        );
        assert!(events.iter().any(|event| matches!(event,
            Event::Model { model, thinking: Some(effort), provider: Some(provider), .. }
                if model == "gpt-6.1-sol" && effort == "high" && provider == "openai")));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::CallEnded { id, ok: false, .. } if id == "c1"))
        );
        let Some(Event::Tokens {
            count,
            input,
            cached,
            context_left,
            ..
        }) = events.last()
        else {
            panic!()
        };
        assert_eq!((*count, *input, *cached), (Count::Total, 300, 200));
        assert_eq!(*context_left, Some(0.9));
    }

    /// A lead says its sub-agents: the one `spawn_agent` started, and how `wait_agent`
    /// and `close_agent` found them (Codex 0.155, 2026-09-27).
    #[test]
    fn a_lead_says_what_its_sub_agents_did() {
        let pascal = "88888888-8888-4888-8888-888888888888";
        let events = read(&[
            json!({"timestamp":"2026-09-27T13:00:00Z","type":"response_item","payload":{"type":"function_call","name":"spawn_agent","namespace":"multi_agent_v1","call_id":"s1","arguments":"{\"message\":\"Compare\"}"}}),
            json!({"timestamp":"2026-09-27T13:00:01Z","type":"response_item","payload":{"type":"function_call_output","call_id":"s1","output":format!("{{\"agent_id\":\"{pascal}\",\"nickname\":\"Pascal\"}}")}}),
            json!({"timestamp":"2026-09-27T13:00:02Z","type":"response_item","payload":{"type":"function_call","name":"wait_agent","namespace":"multi_agent_v1","call_id":"w1","arguments":format!("{{\"targets\":[\"{pascal}\"]}}")}}),
            json!({"timestamp":"2026-09-27T13:00:09Z","type":"response_item","payload":{"type":"function_call_output","call_id":"w1","output":format!("{{\"status\":{{\"{pascal}\":{{\"errored\":\"boom\"}}}}}}")}}),
        ]);
        let said: Vec<(Option<&str>, Option<Execution>)> = events
            .iter()
            .filter_map(|event| match event {
                Event::SubAgent {
                    external_id,
                    call,
                    state,
                    ..
                } if external_id == pascal => Some((call.as_deref(), *state)),
                _ => None,
            })
            .collect();
        assert_eq!(
            said,
            [
                (Some("s1"), Some(Execution::Running)),
                (None, Some(Execution::Failed))
            ]
        );
    }

    fn user(text: &str) -> Value {
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}})
    }

    fn agent(text: &str) -> Value {
        json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}})
    }

    fn rolled_back(turns: u64) -> Value {
        json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":turns}})
    }

    /// What a rollback took back, in the person's instructions the runtime counts.
    fn taken_back(records: &[Value]) -> Vec<u32> {
        read(records)
            .iter()
            .filter_map(|event| match event {
                Event::Rollback { turns, .. } => Some(*turns),
                _ => None,
            })
            .collect()
    }

    /// Codex undoes turns in place: the items stay in the rollout and it appends
    /// `thread_rolled_back` saying how many turns went, which reads as the person's
    /// instructions taken back.
    #[test]
    fn codex_forgets_the_turns_a_person_rolled_back() {
        let records = [
            user("<environment_context>/tmp</environment_context>"),
            user("the question that stood"),
            agent("the answer that stood"),
            user("the question taken back"),
            json!({"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"1"}}),
            agent("the answer taken back"),
            rolled_back(1),
            user("what was asked instead"),
        ];
        assert_eq!(taken_back(&records), [1]);
    }

    /// Rolling back more turns than a rollout holds takes back what is there, and no
    /// more.
    #[test]
    fn codex_rolls_back_further_than_it_can_and_survives() {
        assert_eq!(
            taken_back(&[
                user("the only question"),
                agent("the only answer"),
                rolled_back(9)
            ]),
            [1]
        );
    }

    /// The notice Codex injects when somebody interrupts a turn on purpose is posted as
    /// the person, and opens no turn: it does not absorb the rollback meant for the
    /// question they withdrew.
    #[test]
    fn an_interrupted_turn_notice_does_not_absorb_the_rollback() {
        let records = [
            user("the question that stood"),
            agent("the answer that stood"),
            user("the question they withdrew"),
            agent("the answer they withdrew"),
            user(
                "<TURN_ABORTED>\nThe user interrupted the previous turn on purpose.\n</turn_aborted>",
            ),
            rolled_back(1),
        ];
        assert_eq!(taken_back(&records), [1]);
    }

    /// One agent's message to another is a turn Codex counts; an agent merely answering in
    /// that record's shape is not, and cannot absorb the rollback of a private question.
    #[test]
    fn invalid_agent_records_cannot_absorb_a_private_turns_rollback() {
        let valid = json!({"author": "/root", "recipient": "/root/worker", "content": "an example", "trigger_turn": false});
        assert_eq!(
            taken_back(&[
                user("a question"),
                agent(&valid.to_string()),
                rolled_back(1)
            ]),
            Vec::<u32>::new(),
            "the agents' turn went, no instruction of the person's"
        );
        let mut wrong_path = valid.clone();
        wrong_path["author"] = json!("/root/root");
        let mut wrong_metadata = valid.clone();
        wrong_metadata["internal_chat_message_metadata_passthrough"] = json!({"turn_id": 42});
        let mut null_recipients = valid;
        null_recipients["other_recipients"] = Value::Null;
        for example in [wrong_path, wrong_metadata, null_recipients] {
            let records = [
                user("private question withdrawn"),
                agent(&example.to_string()),
                rolled_back(1),
                user("the replacement question"),
            ];
            assert_eq!(taken_back(&records), [1], "{example}");
        }
    }

    #[test]
    fn xml_lookalikes_remain_real_user_turns() {
        let records = [
            user("the question that stood"),
            agent("the answer that stood"),
            user("<hook_prompt_example/>"),
            agent("an explanation of the XML example"),
            rolled_back(1),
        ];
        assert_eq!(taken_back(&records), [1]);
        let said: Vec<String> = read(&records[..3])
            .into_iter()
            .filter_map(|event| match event {
                Event::Words {
                    voice: Voice::Person,
                    text,
                    ..
                } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(said, ["the question that stood", "<hook_prompt_example/>"]);
    }

    #[test]
    fn zero_and_boundaryless_rollbacks_take_nothing_back() {
        let records = [
            agent("retained initial context"),
            rolled_back(1),
            user("the current question"),
            rolled_back(0),
        ];
        assert_eq!(taken_back(&records), Vec::<u32>::new());
    }

    /// A rollback that does not say how much went cannot be read past.
    #[test]
    fn a_rollback_without_a_count_is_refused() {
        let mut state = State::default();
        let record = json!({"type":"event_msg","payload":{"type":"thread_rolled_back"}});
        let place = Place {
            segment: 0,
            offset: 0,
            at: 0,
            copied: false,
            record: &record,
        };
        assert_eq!(state.read(&place, &mut Vec::new()), Err(UNCOUNTED));
        let record =
            json!({"type":"event_msg","payload":{"type":"thread_rolled_back","num_turns":"two"}});
        let place = Place {
            segment: 0,
            offset: 0,
            at: 0,
            copied: false,
            record: &record,
        };
        assert_eq!(state.read(&place, &mut Vec::new()), Err(UNCOUNTED));
    }

    /// A call's end, as the events of the records say it: `(ok, error)` of each.
    fn ends(events: &[Event]) -> Vec<(String, bool, Option<String>)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::CallEnded { id, ok, error, .. } => Some((id.clone(), *ok, error.clone())),
                _ => None,
            })
            .collect()
    }

    fn mcp_call() -> [Value; 1] {
        [
            json!({"timestamp":"2026-10-09T09:40:02Z","type":"response_item","payload":{"type":"function_call","name":"sessions_describe","namespace":"mcp__steery__","call_id":"m1","arguments":"{}"}}),
        ]
    }

    fn output(text: &str) -> Value {
        json!({"timestamp":"2026-10-09T09:40:03Z","type":"response_item","payload":{"type":"function_call_output","call_id":"m1","output":text}})
    }

    fn ended(payload: Value) -> Value {
        json!({"timestamp":"2026-10-09T09:40:03Z","type":"event_msg","payload":payload})
    }

    /// Codex does not write a call's `success`, so what refused an MCP call is its own event:
    /// a legacy rollout's `mcp_tool_call_end`, in either order with the output.
    #[test]
    fn a_refused_mcp_call_fails_with_its_tools_words_in_a_legacy_rollout() {
        let refused = ended(
            json!({"type":"mcp_tool_call_end","call_id":"m1","invocation":{"server":"steery","tool":"sessions_describe"},
            "result":{"Ok":{"content":[{"type":"text","text":"HTTP 400: value is longer than 120 characters (invalid)"}],"isError":true}}}),
        );
        let mut before = mcp_call().to_vec();
        before.extend([
            refused.clone(),
            output("HTTP 400: value is longer than 120 characters (invalid)"),
        ]);
        assert_eq!(
            ends(&read(&before)),
            [(
                "m1".to_owned(),
                false,
                Some("HTTP 400: value is longer than 120 characters (invalid)".to_owned())
            )]
        );
        // The output first: the call ended as it read, and the event corrects it.
        let mut after = mcp_call().to_vec();
        after.extend([
            output("HTTP 400: value is longer than 120 characters (invalid)"),
            refused,
        ]);
        assert_eq!(
            ends(&read(&after)),
            [
                ("m1".to_owned(), true, None),
                (
                    "m1".to_owned(),
                    false,
                    Some("HTTP 400: value is longer than 120 characters (invalid)".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn an_mcp_call_that_did_not_reach_its_server_says_the_error_without_codexs_prefix() {
        let mut records = mcp_call().to_vec();
        records.extend([
            ended(json!({"type":"mcp_tool_call_end","call_id":"m1","result":{"Err":"tool call error: transport closed"}})),
            output("tool call error: transport closed"),
        ]);
        assert_eq!(
            ends(&read(&records)),
            [("m1".to_owned(), false, Some("transport closed".to_owned()))]
        );
        let mut fine = mcp_call().to_vec();
        fine.extend([
            ended(json!({"type":"mcp_tool_call_end","call_id":"m1","result":{"Ok":{"content":[{"type":"text","text":"done"}]}}})),
            output("done"),
        ]);
        assert_eq!(ends(&read(&fine)), [("m1".to_owned(), true, None)]);
    }

    /// Codex 0.171's code mode calls MCP tools from an `exec` script: the rollout files each
    /// as an item of its own (`exec-<uuid>`) between the script and its output, its times
    /// beside it on the record, as real runs of 2026-11-24 wrote them.
    #[test]
    fn a_tool_a_script_called_is_a_call_inside_it_with_its_own_failure() {
        let script = json!({"timestamp":"2026-11-24T02:18:59Z","type":"response_item","payload":{"type":"custom_tool_call","name":"exec","call_id":"call_s","input":"const r = await tools.mcp__steery__sessions_query({});"}});
        let item = |id: &str, status: &str, error: Value, result: Value| {
            ended(
                json!({"type":"item_completed","thread_id":"x","turn_id":"t",
                "started_at_ms":1_779_000_000_000_u64,"completed_at_ms":1_779_000_000_250_u64,
                "item":{"type":"McpToolCall","id":id,"server":"steery","tool":"sessions_query","arguments":{"steps":[]},
                "status":status,"result":result,"error":error,"duration":{"secs":0,"nanos":250_000_000}}}),
            )
        };
        let answered = json!({"timestamp":"2026-11-24T02:19:00Z","type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_s",
            "output":[{"type":"input_text","text":"Script completed\nWall time 0.1 seconds\nOutput:\n"}]}});
        let events = read(&[
            script.clone(),
            item(
                "exec-1",
                "failed",
                json!({"message":"MCP tool call requires approval, but approval policy is never"}),
                Value::Null,
            ),
            item(
                "exec-2",
                "completed",
                Value::Null,
                json!({"content":[{"type":"text","text":"[]"}],"isError":false}),
            ),
            // Codex can write the tool's whole result as its error message.
            item(
                "exec-w",
                "failed",
                json!({"message": r#"{"content":[{"type":"text","text":"{\"error\":\"workspace_required\"}"}],"isError":true}"#}),
                json!({"content":[{"type":"text","text":"{\"error\":\"workspace_required\"}"}],"isError":true}),
            ),
            answered.clone(),
        ]);
        assert_eq!(
            ends(&events),
            [
                (
                    "exec-1".to_owned(),
                    false,
                    Some(
                        "MCP tool call requires approval, but approval policy is never".to_owned()
                    )
                ),
                ("exec-2".to_owned(), true, None),
                (
                    "exec-w".to_owned(),
                    false,
                    Some(r#"{"error":"workspace_required"}"#.to_owned())
                ),
                ("call_s".to_owned(), true, None),
            ],
            "each tool the script called ends on its own; the script ends as Codex said"
        );
        let nested: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::CallBegan { at, call } if call.id.starts_with("exec-") => {
                    Some((*at, call.tool.clone(), call.parent.clone(), call.group))
                }
                _ => None,
            })
            .collect();
        assert_eq!(nested.len(), 3);
        assert_eq!(nested[0].0, 1_779_000_000_000);
        let ended_at = events.iter().find_map(|event| match event {
            Event::CallEnded { at, id, .. } if id == "exec-1" => Some(*at),
            _ => None,
        });
        assert_eq!(
            ended_at,
            Some(1_779_000_000_250),
            "it ends when the record says"
        );
        assert_eq!(nested[0].1, "mcp__steery__sessions_query");
        assert_eq!(nested[0].2.as_deref(), Some("call_s"));
        assert_eq!(nested[0].3, steery_sdk::wire::harness::Group::Steery);
        // After its script answered, an item is a call of its own as before.
        let events = read(&[
            script,
            answered,
            item("exec-3", "failed", json!({"message":"no"}), Value::Null),
        ]);
        assert!(
            events.iter().all(
                |event| !matches!(event, Event::CallBegan { call, .. } if call.id == "exec-3")
            )
        );
    }

    /// A paginated rollout keeps the call as an `item_completed` of an MCP tool call.
    #[test]
    fn a_failed_mcp_item_fails_its_call_in_a_paginated_rollout() {
        let item = |status: &str, error: Value, result: Value| {
            ended(
                json!({"type":"item_completed","thread_id":"x","turn_id":"t","item":{"type":"McpToolCall","id":"m1",
                "server":"steery","tool":"sessions_describe","arguments":{},"status":status,"result":result,"error":error}}),
            )
        };
        let mut failed = mcp_call().to_vec();
        failed.extend([
            item(
                "failed",
                json!({"message":"refused: not allowed"}),
                Value::Null,
            ),
            output("refused: not allowed"),
        ]);
        assert_eq!(
            ends(&read(&failed)),
            [(
                "m1".to_owned(),
                false,
                Some("refused: not allowed".to_owned())
            )]
        );
        let mut flagged = mcp_call().to_vec();
        flagged.extend([
            item(
                "completed",
                Value::Null,
                json!({"content":[{"type":"text","text":"bad input"}],"isError":true}),
            ),
            output("bad input"),
        ]);
        assert_eq!(
            ends(&read(&flagged)),
            [("m1".to_owned(), false, Some("bad input".to_owned()))]
        );
        let mut fine = mcp_call().to_vec();
        fine.extend([
            item("completed", Value::Null, json!({"content":[]})),
            output("ok"),
        ]);
        assert_eq!(ends(&read(&fine)), [("m1".to_owned(), true, None)]);
    }

    #[test]
    fn an_older_protocol_is_told_no_error() {
        let mut events = read(&[
            mcp_call()[0].clone(),
            ended(json!({"type":"mcp_tool_call_end","call_id":"m1","result":{"Err":"no"}})),
            output("no"),
        ]);
        without_error(&mut events);
        assert_eq!(ends(&events), [("m1".to_owned(), false, None)]);
    }
}
