//! `turn/read`: one line of a turn that `droid exec -o stream-json` runs. Each line of its
//! standard output is one JSON record that names the conversation by `session_id` (1.9.0):
//! - `system`/`init` first, with the model;
//! - `message` of the `user`: the person's words went in, under the id the file gives them;
//! - `tool_call` and `tool_result`, under the call's id in the file;
//! - `reasoning` and the agent's `message`, whose words the file splits by block;
//! - `completion` where the turn finished, or `error` where it failed.
//!
//! Droid refuses a model or a reasoning effort it does not run on standard error, in one
//! line, before any record.

use serde_json::Value;
use steery_sdk::refusal;
use steery_sdk::wire::harness::{Ended, Event, Stream, TurnLine, TurnRead, TurnState};

use super::events;

/// What one line of a turn says.
pub fn read(read: TurnRead) -> TurnLine {
    let line = match read.stream {
        Stream::Stderr => {
            return TurnLine {
                refused: refusal_of(&read.line).map(str::to_owned),
                ..TurnLine::default()
            };
        }
        Stream::Stdout => match serde_json::from_str::<Value>(&read.line) {
            Ok(line) => line,
            Err(_) => return TurnLine::default(),
        },
    };
    let named = line["session_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    let at = line["timestamp"]
        .as_u64()
        .unwrap_or_else(crate::turn::drive::now);
    let text = |key: &str| line[key].as_str().map(str::to_owned);
    let mut said = TurnLine {
        named,
        ..TurnLine::default()
    };
    match line["type"].as_str() {
        Some("system") if line["subtype"] == "init" => {
            said.took = Some(false);
            said.events = text("model")
                .filter(|model| !model.is_empty())
                .map(|model| Event::Model {
                    at,
                    model,
                    provider: None,
                    thinking: text("reasoning_effort"),
                })
                .into_iter()
                .collect();
        }
        Some("message") if line["role"] == "user" => {
            said.took = Some(true);
            said.events = text("id")
                .map(|turn| Event::TurnBegan {
                    at,
                    turn: Some(turn),
                })
                .into_iter()
                .collect();
        }
        Some("tool_call") => {
            if let (Some(id), Some(tool)) = (line["id"].as_str(), line["toolName"].as_str()) {
                said.events.push(Event::CallBegan {
                    at,
                    call: events::call(
                        id,
                        tool,
                        line["parameters"].clone(),
                        line["messageId"].as_str(),
                    ),
                });
            }
        }
        Some("tool_result") => {
            if let Some(id) = line["id"].as_str() {
                let failed = line["isError"] == true;
                let gave = if failed {
                    line["error"]["message"].as_str()
                } else {
                    line["value"].as_str()
                };
                said.events.push(events::call_ended(
                    at,
                    id,
                    !failed,
                    gave.unwrap_or_default(),
                ));
            }
        }
        Some("completion") => {
            said.ended = Some(Ended {
                state: TurnState::Finished,
                error: None,
            });
        }
        Some("error") => {
            let message = text("message").unwrap_or_default();
            said.refused = refused_by(&message).map(str::to_owned);
            said.ended = Some(Ended {
                state: TurnState::Failed,
                error: Some(message).filter(|message| !message.trim().is_empty()),
            });
        }
        _ => {}
    }
    said
}

/// The refusal word of one line of Droid's standard error, where it is one.
fn refusal_of(line: &str) -> Option<&'static str> {
    let line = line.trim();
    if line.starts_with("Invalid model:") {
        Some(refusal::MODEL_MISSING)
    } else if line.starts_with("Unsupported reasoning effort") {
        Some(refusal::SELECTION_UNSUPPORTED)
    } else {
        None
    }
}

/// The refusal word of the error that ended a turn, where it is one: a Factory model with
/// no sign-in.
fn refused_by(message: &str) -> Option<&'static str> {
    message
        .contains("Authentication failed")
        .then_some(refusal::REFUSED)
}
