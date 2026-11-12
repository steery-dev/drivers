//! `turn/read`: one line of a turn that `qwen -o stream-json` runs. Its standard output is
//! Claude Code's stream-json: `system` `init` names the conversation and its model, an
//! assistant's `tool_use` or a user's `tool_result` is a call that began or ended, and
//! `result` ends the turn. A call's `tool_result` is on standard output before the chat
//! file holds it, so its events come from the line, under the ids the chat file gives the
//! same call. Qwen runs on for a while after its `result` line, and the turn has ended
//! there: what follows it says nothing. Its standard error is Qwen's notices, and one line
//! of its own where it would not run the turn.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use serde_json::Value;
use steery_sdk::refusal;
use steery_sdk::wire::harness::{Ended, Event, Stream, TurnLine, TurnRead, TurnState};

use super::{drive, events};

/// Each turn's state between its lines, by the runtime's name of the turn.
#[derive(Default)]
pub struct Turns {
    turns: Mutex<HashMap<String, Run>>,
}

/// One turn as its lines so far left it.
#[derive(Default)]
struct Run {
    named: Option<String>,
    ended: bool,
}

impl Turns {
    /// What one line of `read.turn` says.
    pub fn read(&self, read: TurnRead) -> TurnLine {
        let mut turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
        let run = turns.entry(read.turn.clone()).or_default();
        let mut line = TurnLine::default();
        if run.ended {
            return line;
        }
        let record = match read.stream {
            Stream::Stderr => {
                if let Some(word) = refusal_of(&read.line) {
                    run.ended = true;
                    line.refused = Some(word.to_owned());
                    line.ended = Some(Ended {
                        state: TurnState::Failed,
                        error: Some(read.line.trim().to_owned()),
                    });
                }
                return line;
            }
            Stream::Stdout => match serde_json::from_str::<Value>(&read.line) {
                Ok(record) => record,
                Err(_) => return line,
            },
        };
        if let Some(named) = record["session_id"].as_str().filter(|id| !id.is_empty())
            && run.named.as_deref() != Some(named)
        {
            run.named = Some(named.to_owned());
            line.named = Some(named.to_owned());
        }
        match record["type"].as_str() {
            Some("system") if record["subtype"] == "init" => {
                line.events.extend(
                    record["model"]
                        .as_str()
                        .filter(|model| !model.is_empty())
                        .map(|model| Event::Model {
                            at: drive::now(),
                            model: model.to_owned(),
                            provider: None,
                            thinking: None,
                        }),
                );
            }
            Some("assistant") => line.events = began(&record),
            Some("user") => line.events = ended(&record),
            Some("result") => {
                run.ended = true;
                let ended = ended_by(&record);
                line.refused = ended
                    .error
                    .as_deref()
                    .and_then(refused_by)
                    .map(str::to_owned);
                line.ended = Some(ended);
            }
            _ => {}
        }
        line
    }
}

/// The calls an assistant's message begins: each `tool_use` part, as the chat file's
/// `functionCall` of the same id.
fn began(record: &Value) -> Vec<Event> {
    parts(record)
        .filter(|part| part["type"] == "tool_use")
        .filter_map(|part| {
            Some(Event::CallBegan {
                at: drive::now(),
                call: events::call(
                    part["id"].as_str()?,
                    part["name"].as_str().unwrap_or_default(),
                    part["input"].clone(),
                ),
            })
        })
        .collect()
}

/// The calls a user's message ends: each `tool_result` part, with the words the model was
/// given.
fn ended(record: &Value) -> Vec<Event> {
    parts(record)
        .filter(|part| part["type"] == "tool_result")
        .filter_map(|part| {
            let id = part["tool_use_id"].as_str()?.to_owned();
            let output = match &part["content"] {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            Some(events::ended(
                drive::now(),
                id,
                part["is_error"] != true,
                output,
            ))
        })
        .flatten()
        .collect()
}

fn parts(record: &Value) -> impl Iterator<Item = &Value> {
    record["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
}

/// How a `result` line ends the turn: a success finished; an error failed with its text.
fn ended_by(result: &Value) -> Ended {
    if result["is_error"] == false && result["subtype"] == "success" {
        return Ended {
            state: TurnState::Finished,
            error: None,
        };
    }
    let text = result["error"]["message"]
        .as_str()
        .or_else(|| result["result"].as_str())
        .filter(|text| !text.trim().is_empty())
        .or_else(|| result["subtype"].as_str())
        .map(str::to_owned);
    Ended {
        state: TurnState::Failed,
        error: text,
    }
}

/// The refusal word that one line of Qwen's standard error means, where it would not run
/// the turn: a conversation it does not keep, or no way to reach a model. Its notices
/// (a tool held back, a setting it ignored) are none.
fn refusal_of(line: &str) -> Option<&'static str> {
    let line = line.trim();
    if line.starts_with("No saved session found with ID") {
        Some(refusal::NOT_FOUND)
    } else if line.starts_with("No auth type is selected")
        || line.starts_with("The configured auth type is")
        || line.starts_with("Error authenticating")
    {
        Some(refusal::REFUSED)
    } else if line.starts_with("No input provided via stdin") {
        Some(refusal::START_FAILED)
    } else {
        None
    }
}

/// The refusal word of a failed turn's text, where the model's provider refused the model.
fn refused_by(said: &str) -> Option<&'static str> {
    (said.contains("model") && (said.contains("not found") || said.contains("404")))
        .then_some(refusal::MODEL_MISSING)
}
