//! What the app server's lines of a turn Steery runs say of its conversation, as the events
//! `sessions/read` folds from its rollout and under the ids the rollout gives the same
//! facts (PROTOCOL.md §4.2), so the runtime folds each once: its turns begun and ended
//! (`turn/started`, `turn/completed`, by the turn's id), the agent's words and reasoning
//! (`agentMessage`, `reasoning`, by the item's id, which is the rollout record's), its shell
//! and MCP calls (by the item's id, which is the call's `call_id`), the thread's running
//! total (`thread/tokenUsage/updated`, the rollout's `token_count` numbers), what it runs on
//! (the thread's model as the app server opened it, and `model/rerouted`), and its title
//! (`thread/name/updated`). Each is stamped as the app server stamped its line
//! (`emittedAtMs`); a line it did not stamp says no event.
//!
//! Left to the rollout, whose ids the app server does not give: a person's words, one
//! `userMessage` item named by none of the rollout's `message` records (Codex 0.163:
//! item `01a158a8-094a-…`, record `msg_01a158a8-0949-…`); a compaction, whose
//! `contextCompaction` item is not the `window_id` its `compacted` record is named by; and
//! every call whose item the rollout files under another tool or id (a command the person
//! ran, one typed into a running session, a patch, a picture, a sub-agent's).

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex, PoisonError};

use serde_json::{Value, json};
use steery_sdk::wire::harness::{Count, Event, TurnState, Voice};

use crate::events::{self, number, text};
use crate::calls;

/// How many turns' state is kept: one per run the runtime reads, the newest.
const TURNS: usize = 64;

/// What one run's lines carried from one to the next: what the conversation runs on, the
/// model last said as an event, and when a spent quota resets.
#[derive(Default)]
struct Heard {
    model: Option<String>,
    effort: Option<String>,
    provider: Option<String>,
    said: Option<(String, Option<String>)>,
    spent_until: Option<u64>,
}

#[derive(Default)]
struct Runs {
    by: HashMap<String, Heard>,
    order: VecDeque<String>,
}

impl Runs {
    fn of(&mut self, turn: &str) -> &mut Heard {
        if !self.by.contains_key(turn) {
            if self.order.len() >= TURNS
                && let Some(oldest) = self.order.pop_front()
            {
                self.by.remove(&oldest);
            }
            self.order.push_back(turn.to_owned());
        }
        self.by.entry(turn.to_owned()).or_default()
    }
}

static RUNS: LazyLock<Mutex<Runs>> = LazyLock::new(Default::default);

/// The events one line of the run `turn` says, and the title it gives the thread.
pub fn said(turn: &str, line: &Value) -> (Vec<Event>, Option<String>) {
    let mut runs = RUNS.lock().unwrap_or_else(PoisonError::into_inner);
    let mut out = Vec::new();
    let title = runs.of(turn).line(line, &mut out);
    (out, title)
}

impl Heard {
    fn line(&mut self, line: &Value, out: &mut Vec<Event>) -> Option<String> {
        let Some(method) = text(line, "method") else {
            // The response that opened the thread says what it runs on.
            if let Some(result) = line.get("result").filter(|result| result.get("thread").is_some()) {
                let set = |field: &mut Option<String>, key: &str| {
                    if let Some(value) = text(result, key).filter(|value| !value.is_empty()) {
                        *field = Some(value.to_owned());
                    }
                };
                set(&mut self.model, "model");
                set(&mut self.effort, "reasoningEffort");
                set(&mut self.provider, "modelProvider");
            }
            return None;
        };
        let params = line.get("params").unwrap_or(&Value::Null);
        if method == "thread/name/updated" {
            return text(params, "threadName")
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned);
        }
        if method == "account/rateLimits/updated" {
            if let Some(limits) = params.get("rateLimits").filter(|l| l.is_object()) {
                self.spent_until = events::spent_until(limits);
            }
            return None;
        }
        let at = line.get("emittedAtMs").and_then(Value::as_u64)?;
        match method {
            "turn/started" => {
                out.push(Event::TurnBegan {
                    at,
                    turn: params.pointer("/turn/id").and_then(Value::as_str).map(str::to_owned),
                });
                self.settings(at, out);
            }
            "turn/completed" => self.ended(params.get("turn")?, at, out),
            "model/rerouted" => {
                if let Some(model) = text(params, "toModel").filter(|model| !model.is_empty()) {
                    self.model = Some(model.to_owned());
                    self.settings(at, out);
                }
            }
            "thread/tokenUsage/updated" => self.tokens(params.get("tokenUsage")?, at, out),
            "item/started" => {
                if let Some(call) = called(params.get("item")?) {
                    out.push(Event::CallBegan { at, call });
                }
            }
            "item/completed" => completed(params.get("item")?, at, out),
            _ => {}
        }
        None
    }

    /// A `model` event where what the thread runs on is not what was last said.
    fn settings(&mut self, at: u64, out: &mut Vec<Event>) {
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

    /// `turn/completed`: the turn ended as its status says, a failed one with the error the
    /// app server gave and what the turn ran on, as the rollout's `task_complete` reads.
    fn ended(&self, turn: &Value, at: u64, out: &mut Vec<Event>) {
        let state = match text(turn, "status") {
            Some("completed") => TurnState::Finished,
            Some("failed") => TurnState::Failed,
            Some("interrupted") => TurnState::Interrupted,
            _ => return,
        };
        let failure = (state == TurnState::Failed).then(|| {
            let mut failure = events::failure_of(turn.get("error").unwrap_or(&Value::Null));
            failure.provider = self.provider.clone();
            failure.model = self.model.clone();
            failure.thinking = self.effort.clone();
            if failure.kind == steery_sdk::wire::harness::FailureKind::RateLimit {
                failure.retry_at = self.spent_until.filter(|reset| *reset > at);
            }
            failure
        });
        out.push(Event::TurnEnded {
            at,
            turn: text(turn, "id").map(str::to_owned),
            state,
            failure,
        });
    }

    /// The thread's running total, and the context window's share its last response left.
    fn tokens(&self, usage: &Value, at: u64, out: &mut Vec<Event>) {
        let Some(total) = usage.get("total").filter(|total| total.is_object()) else {
            return;
        };
        let context_left = usage
            .get("modelContextWindow")
            .and_then(Value::as_u64)
            .filter(|window| *window > 0)
            .zip(usage.pointer("/last/totalTokens").and_then(Value::as_u64))
            .map(|(window, used)| (1.0 - used as f64 / window as f64).clamp(0.0, 1.0));
        out.push(Event::Tokens {
            at,
            count: Count::Total,
            response: None,
            model: self.model.clone(),
            input: number(total, "inputTokens"),
            cached: number(total, "cachedInputTokens"),
            cache_write: None,
            output: number(total, "outputTokens"),
            reasoning: number(total, "reasoningOutputTokens"),
            context_left,
        });
    }
}

/// The call an item is, as the rollout files it: a command Codex's shell tool started
/// (`exec_command` with its `cmd`), or an MCP tool's (`mcp__<server>__<tool>`).
fn called(item: &Value) -> Option<steery_sdk::wire::harness::Call> {
    let id = text(item, "id").filter(|id| !id.is_empty())?;
    match text(item, "type")? {
        "commandExecution" if text(item, "source") == Some("unifiedExecStartup") => {
            let command = text(item, "command")?;
            Some(calls::call(id, "exec_command", json!({"cmd": script(command)})))
        }
        "mcpToolCall" => {
            let server = text(item, "server")?;
            let tool = calls::spelled(text(item, "tool")?, Some(&format!("mcp__{server}")));
            let input = item.get("arguments").cloned().unwrap_or(Value::Null);
            Some(calls::call(id, &tool, input))
        }
        _ => None,
    }
}

/// What an item says once it is done: the agent's words or reasoning, or the end of a call
/// [`called`] began and its result, under the rollout's `<call_id>:out`.
fn completed(item: &Value, at: u64, out: &mut Vec<Event>) {
    let Some(id) = text(item, "id").filter(|id| !id.is_empty()) else {
        return;
    };
    let words = |voice: Voice, said: String, call: Option<String>, out: &mut Vec<Event>| {
        if !said.trim().is_empty() {
            out.push(Event::Words {
                at,
                id: call.as_ref().map_or_else(|| id.to_owned(), |call| format!("{call}:out")),
                voice,
                text: said,
                call,
                clips: Vec::new(),
            });
        }
    };
    match text(item, "type") {
        Some("agentMessage") => {
            words(Voice::Agent, text(item, "text").unwrap_or_default().to_owned(), None, out);
        }
        Some("reasoning") => {
            let parts = |key: &str| -> Vec<&str> {
                item.get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter(|said| !said.trim().is_empty())
                    .collect()
            };
            let summary = parts("summary");
            let thought = if summary.is_empty() { parts("content") } else { summary };
            words(Voice::Thought, thought.join("\n\n"), None, out);
        }
        _ => {
            if called(item).is_none() {
                return;
            }
            let (ok, output) = match text(item, "type") {
                Some("mcpToolCall") => (
                    text(item, "status") == Some("completed")
                        && item.get("error").is_none_or(Value::is_null),
                    events::output_text(item.get("result").filter(|result| !result.is_null())),
                ),
                _ => (
                    match item.get("exitCode").and_then(Value::as_i64) {
                        Some(code) => code == 0,
                        None => text(item, "status") == Some("completed"),
                    },
                    text(item, "aggregatedOutput").unwrap_or_default().to_owned(),
                ),
            };
            out.push(Event::CallEnded {
                at,
                id: id.to_owned(),
                ok,
                chars: output.chars().count() as u64,
                tail: events::tail(&output),
            });
            words(Voice::Tool, output, Some(id.to_owned()), out);
        }
    }
}

/// The script a command runs, as the model wrote it: Codex runs `exec_command`'s `cmd` as
/// `<shell> -lc '<cmd>'`, and its item says that whole line.
fn script(command: &str) -> String {
    match shell_words(command).as_deref() {
        Some([_, flag, script]) if matches!(flag.as_str(), "-lc" | "-c") => script.clone(),
        _ => command.to_owned(),
    }
}

/// A line's POSIX shell words, quotes taken away; none where a quote is left open.
fn shell_words(line: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                let word = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
            }
            '"' => {
                let word = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            c @ ('"' | '\\' | '$' | '`') => word.push(c),
                            '\n' => {}
                            c => {
                                word.push('\\');
                                word.push(c);
                            }
                        },
                        c => word.push(c),
                    }
                }
            }
            '\\' => match chars.next() {
                Some('\n') | None => {}
                Some(c) => word.get_or_insert_with(String::new).push(c),
            },
            c if c.is_whitespace() => words.extend(word.take()),
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    Some(words)
}
