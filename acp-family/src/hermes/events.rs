//! The events of a conversation's rows in Hermes's `messages` table, in their order.
//!
//! A row is a person's message (`user`), the agent's (`assistant`: its reasoning, its words
//! and its tool calls) or a call's result (`tool`). Hermes writes rows of its own that no
//! model said, typed by `display_kind`: a closer of a turn that failed (`failed_turn`,
//! `agent/conversation_loop.py:1956-1957`), an invisible stand-in for the reply an
//! interruption cut (`hidden` with the placeholder as `api_content`,
//! `agent/agent_runtime_helpers_placeholders.py:16-26`), and messages it puts in the
//! person's place, such as a background delegation's result (`async_delegation_complete`).

use std::collections::HashMap;

use serde_json::Value;
use steery_sdk::wire::harness::{
    Compactor, Event, Execution, Failure, FailureKind, TurnState, Voice,
};

use super::calls;

/// One row of `messages`.
pub struct Row {
    pub role: String,
    pub content: String,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<String>,
    pub tool_name: Option<String>,
    pub at: u64,
    pub finish_reason: Option<String>,
    pub reasoning: Option<String>,
    pub summary: bool,
    pub display_kind: Option<String>,
    pub display_metadata: Option<String>,
    pub api_content: Option<String>,
    /// `message_uid`, which an in-place compaction keeps on the rows it writes again.
    pub uid: String,
}

/// A sub-agent session of the conversation, by the `_subagent_id` its delegation names.
pub struct Child {
    pub external_id: String,
    pub ended: bool,
}

/// `finish_reason` values of a turn that ended in an error (`hermes_state_sessions.py:251`).
const FAILED: &[&str] = &["error", "agent_error", "content_filter"];

/// The interruption stand-in's `api_content`, current and older spelling
/// (`agent/agent_runtime_helpers_placeholders.py:16,20`).
const INTERRUPTED: &[&str] = &[
    "[interrupt: no assistant output for this turn]",
    "[response interrupted]",
];

/// How many of a result's last lines `call_ended` carries.
const TAIL_LINES: usize = 8;

pub struct Fold<'a> {
    pub events: Vec<Event>,
    pub servers: &'a [String],
    pub children: &'a HashMap<String, Child>,
    /// The latest time of a row folded so far.
    pub latest: u64,
}

impl Fold<'_> {
    pub fn row(&mut self, row: &Row) {
        if row.summary {
            // The summary keeps the uid and time of the message it was written over, so the
            // compaction is as late as the rows before it.
            let at = row.at.max(self.latest);
            self.events.push(Event::Compacted {
                at,
                id: row.uid.clone(),
                by: Compactor::Harness { reason: None },
                tokens_before: None,
                tokens_after: None,
            });
            self.words(
                at,
                format!("{}:summary", row.uid),
                Voice::Summary,
                &row.content,
                None,
            );
            return;
        }
        self.latest = self.latest.max(row.at);
        match row.role.as_str() {
            "user" => self.asked(row),
            "assistant" => self.answered(row),
            "tool" => self.result(row),
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

    /// A person's row begins a turn with their words. A row Hermes put in the person's place
    /// begins one too, but says nothing as the person; a refused message stays theirs.
    fn asked(&mut self, row: &Row) {
        self.events.push(Event::TurnBegan {
            at: row.at,
            turn: None,
        });
        if matches!(row.display_kind.as_deref(), None | Some("content_refused")) {
            self.words(row.at, row.uid.clone(), Voice::Person, &row.content, None);
        }
    }

    fn answered(&mut self, row: &Row) {
        match row.display_kind.as_deref() {
            Some("failed_turn") => {
                let failure = failure(row);
                return self.ended(row.at, TurnState::Failed, Some(failure));
            }
            Some("hidden") => {
                if row
                    .api_content
                    .as_deref()
                    .is_some_and(|said| INTERRUPTED.contains(&said.trim()))
                {
                    self.ended(row.at, TurnState::Interrupted, None);
                }
                return;
            }
            _ => {}
        }
        if let Some(thought) = &row.reasoning {
            self.words(
                row.at,
                format!("{}:thought", row.uid),
                Voice::Thought,
                thought,
                None,
            );
        }
        self.words(row.at, row.uid.clone(), Voice::Agent, &row.content, None);
        let tool_calls: Vec<Value> = row
            .tool_calls
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or_default();
        let mut called = false;
        for entry in &tool_calls {
            if let Some(call) = calls::call(entry, Some(&row.uid), self.servers) {
                called = true;
                self.events.push(Event::CallBegan { at: row.at, call });
            }
        }
        if called {
            return;
        }
        match row.finish_reason.as_deref() {
            Some(reason) if FAILED.contains(&reason) => self.ended(
                row.at,
                TurnState::Failed,
                Some(Failure {
                    kind: FailureKind::Other,
                    line: first_line(&row.content).unwrap_or(reason).to_owned(),
                    service: true,
                    http_status: None,
                    code: Some(reason.to_owned()),
                    retry_at: None,
                    provider: None,
                    model: None,
                    thinking: None,
                }),
            ),
            _ => self.ended(row.at, TurnState::Finished, None),
        }
    }

    fn result(&mut self, row: &Row) {
        let Some(call) = row.tool_call_id.clone() else {
            return;
        };
        let lines: Vec<&str> = row.content.lines().collect();
        let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
        self.events.push(Event::CallEnded {
            at: row.at,
            id: call.clone(),
            ok: ok(&row.content),
            chars: row.content.chars().count() as u64,
            tail: (!tail.trim().is_empty()).then_some(tail),
            waited: Vec::new(),
        });
        self.words(
            row.at,
            row.uid.clone(),
            Voice::Tool,
            &row.content,
            Some(call.clone()),
        );
        if row.tool_name.as_deref() == Some("delegate_task") {
            self.delegated(row, &call);
        }
    }

    /// `delegate_task` answers with the `subagent_ids` it started (`tools/delegate_tool.py`);
    /// each child session names its own in `model_config._subagent_id`.
    fn delegated(&mut self, row: &Row, call: &str) {
        let Ok(result) = serde_json::from_str::<Value>(&row.content) else {
            return;
        };
        for id in result
            .get("subagent_ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if let Some(child) = self.children.get(id) {
                self.events.push(Event::SubAgent {
                    at: row.at,
                    external_id: child.external_id.clone(),
                    call: Some(call.to_owned()),
                    state: Some(if child.ended {
                        Execution::Finished
                    } else {
                        Execution::Running
                    }),
                    error: None,
                });
            }
        }
    }
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// A result is a failure where Hermes's JSON says so: an `error`, a non-zero `exit_code`,
/// `success: false` or a `status` of `error` or `failed`.
fn ok(content: &str) -> bool {
    let Ok(Value::Object(result)) = serde_json::from_str::<Value>(content) else {
        return true;
    };
    let error = match result.get("error") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    };
    let exited = result
        .get("exit_code")
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0);
    let unsuccessful = result.get("success").and_then(Value::as_bool) == Some(false);
    let status = matches!(
        result.get("status").and_then(Value::as_str),
        Some("error" | "failed")
    );
    !(error || exited || unsuccessful || status)
}

/// A `failed_turn` row's failure: its `display_metadata` holds the error and, where Hermes
/// classified it, `error_surface {layer, code, provider, model}` (`agent/error_surface.py:91-106`).
fn failure(row: &Row) -> Failure {
    let metadata: Value = row
        .display_metadata
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(Value::Null);
    let surface = metadata.get("error_surface");
    let code = surface
        .and_then(|surface| surface.get("code"))
        .and_then(Value::as_str);
    let said = |surface: Option<&Value>, key: &str| {
        surface
            .and_then(|surface| surface.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let kind = match code {
        Some("auth" | "auth_permanent") => FailureKind::SignIn,
        Some("rate_limit" | "upstream_rate_limit") => FailureKind::RateLimit,
        Some("context_overflow") => FailureKind::ContextFull,
        _ => FailureKind::Other,
    };
    let line = metadata
        .get("error")
        .and_then(Value::as_str)
        .and_then(first_line)
        .or_else(|| first_line(&row.content))
        .unwrap_or("failed")
        .to_owned();
    Failure {
        kind,
        line,
        service: surface
            .and_then(|surface| surface.get("layer"))
            .and_then(Value::as_str)
            .is_some_and(|layer| layer != "gateway"),
        http_status: None,
        code: code.map(str::to_owned),
        retry_at: None,
        provider: said(surface, "provider"),
        model: said(surface, "model"),
        thinking: None,
    }
}
