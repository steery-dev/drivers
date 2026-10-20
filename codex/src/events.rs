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
}

/// What a rollback that does not say how much went is refused with: reading on would
/// give work somebody took back as theirs.
pub const UNCOUNTED: &str = "that rollout records a rollback without saying how much went";

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
                        call: None,
                        clips: Vec::new(),
                    });
                }
                if !place.copied {
                    out.push(compacted(place, payload));
                }
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
                let Some(info) = payload.get("info").filter(|info| info.is_object()) else {
                    return Ok(());
                };
                let Some(total) = info.get("total_token_usage") else {
                    return Ok(());
                };
                let context_left = info
                    .get("model_context_window")
                    .and_then(Value::as_u64)
                    .filter(|window| *window > 0)
                    .zip(
                        info.pointer("/last_token_usage/total_tokens")
                            .and_then(Value::as_u64),
                    )
                    .map(|(window, used)| (1.0 - used as f64 / window as f64).clamp(0.0, 1.0));
                out.push(Event::Tokens {
                    at,
                    count: Count::Total,
                    response: None,
                    model: self.model.clone(),
                    input: number(total, "input_tokens"),
                    cached: number(total, "cached_input_tokens"),
                    cache_write: None,
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
            _ => {}
        }
        Ok(())
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
                out.push(Event::CallBegan { at, call });
            }
            Some("function_call_output" | "custom_tool_call_output") => {
                let Some(call_id) = text(payload, "call_id") else {
                    return;
                };
                let output = output_text(payload.get("output"));
                if let Some((tool, target)) = self.agents.remove(call_id) {
                    self.agents_said(&tool, target, call_id, &output, at, out);
                }
                out.push(Event::CallEnded {
                    at,
                    id: call_id.to_owned(),
                    ok: succeeded(payload.get("output"), &output),
                    chars: output.chars().count() as u64,
                    tail: tail(&output),
                });
                if !output.trim().is_empty() {
                    out.push(Event::Words {
                        at,
                        id: format!("{call_id}:out"),
                        voice: Voice::Tool,
                        text: output,
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
/// (`window_id`), else where it is; who made it unrecorded. `latest_token_usage_record`
/// is the last response's before it, whose `total_tokens` Codex counts as the context's.
fn compacted(place: &Place<'_>, payload: &Value) -> Event {
    Event::Compacted {
        at: place.at,
        id: text(payload, "window_id")
            .filter(|id| !id.is_empty())
            .map_or_else(|| format!("{}:{}", place.segment, place.offset), str::to_owned),
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
        out
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
}
