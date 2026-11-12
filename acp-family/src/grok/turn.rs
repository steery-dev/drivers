//! `turn/read`: one line of a turn that `grok --output-format streaming-messages-json`
//! runs. Its standard output is Claude Code's stream-json: `system/init` names the
//! conversation and its model, `result` ends the turn, and an assistant's `tool_use` or a
//! user's `tool_result` is a call that began or ended. A call's events are the ones the
//! transcript gives of it again ([`events::calls`]), read off `updates.jsonl` by its id,
//! so the runtime folds each once. Its standard error is Grok's tracing, and one
//! `Error: ...` line where it would not run the turn.

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader};
use std::path::Path;
use std::sync::Mutex;

use serde_json::Value;
use steery_sdk::refusal;
use steery_sdk::wire::harness::{Ended, Event, Stream, TurnLine, TurnRead, TurnState};

use super::{drive, events, store::Store};

/// The conversation each turn named, by the runtime's id of the turn.
#[derive(Default)]
pub struct Turns {
    named: Mutex<HashMap<String, String>>,
}

impl Turns {
    /// What one line of `read.turn` says.
    pub fn read(&self, store: &Store, read: TurnRead) -> TurnLine {
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
        let mut turns = self.named.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = &named {
            turns.insert(read.turn.clone(), id.clone());
        }
        let session = turns.get(&read.turn).cloned();
        if line["type"] == "result" {
            turns.remove(&read.turn);
        }
        drop(turns);
        match line["type"].as_str() {
            Some("system") if line["subtype"] == "init" => TurnLine {
                events: line["model"]
                    .as_str()
                    .filter(|model| !model.is_empty() && *model != "unknown")
                    .map(|model| Event::Model {
                        at: drive::now(),
                        model: model.to_owned(),
                        provider: None,
                        thinking: None,
                    })
                    .into_iter()
                    .collect(),
                named,
                ..TurnLine::default()
            },
            Some("result") => {
                let ended = ended(&line);
                TurnLine {
                    named,
                    refused: (ended.state == TurnState::Failed)
                        .then(|| ended.error.as_deref().and_then(refused_by))
                        .flatten()
                        .map(str::to_owned),
                    ended: Some(ended),
                    ..TurnLine::default()
                }
            }
            Some(kind @ ("assistant" | "user")) => TurnLine {
                events: session
                    .and_then(|id| store.find(&id).ok())
                    .map(|found| calls(&found.transcript, &line, kind == "user"))
                    .unwrap_or_default(),
                named,
                ..TurnLine::default()
            },
            _ => TurnLine {
                named,
                ..TurnLine::default()
            },
        }
    }
}

/// How a `result` line ends the turn: a success at `end_turn` finished, a stop at
/// `cancelled` interrupted, any other error failed with its text.
fn ended(result: &Value) -> Ended {
    let error = result["is_error"].as_bool().unwrap_or(false);
    if result["stop_reason"] == "cancelled" {
        return Ended {
            state: TurnState::Interrupted,
            error: None,
        };
    }
    if !error && result["subtype"] == "success" {
        return Ended {
            state: TurnState::Finished,
            error: None,
        };
    }
    let text = result["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    let text = Some(text)
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            result["result"]
                .as_str()
                .filter(|text| !text.trim().is_empty() && *text != "null")
                .map(str::to_owned)
        })
        .or_else(|| result["subtype"].as_str().map(str::to_owned));
    Ended {
        state: TurnState::Failed,
        error: text,
    }
}

/// The events of the calls a message line names, as the transcript gives them: a
/// `tool_use` part's `call_began`, or a `tool_result` part's `call_ended`. A call the
/// transcript does not hold yet gives none.
fn calls(transcript: &Path, line: &Value, ended: bool) -> Vec<Event> {
    let ids: Vec<&str> = line["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| match (part["type"].as_str(), ended) {
            (Some("tool_use"), false) => part["id"].as_str(),
            (Some("tool_result"), true) => part["tool_use_id"].as_str(),
            _ => None,
        })
        .collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let Ok(file) = std::fs::File::open(transcript) else {
        return Vec::new();
    };
    let want = if ended {
        "tool_call_update"
    } else {
        "tool_call"
    };
    let mut found: Vec<(usize, Vec<Event>)> = Vec::new();
    for record in BufReader::new(file).lines().map_while(Result::ok) {
        let Some(id) = ids.iter().position(|id| record.contains(id)) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<Value>(&record) else {
            continue;
        };
        if record["params"]["update"]["sessionUpdate"] != want {
            continue;
        }
        let events = events::calls(&record, ids[id]);
        if !events.is_empty() {
            found.retain(|(of, _)| *of != id);
            found.push((id, events));
        }
    }
    found.sort_by_key(|(id, _)| *id);
    found.into_iter().flat_map(|(_, events)| events).collect()
}

/// The refusal word that one line of Grok's standard error means: only its own `Error:`
/// line, never its tracing (`INFO`, `WARN`, an MCP server's sign-in).
fn refusal_of(line: &str) -> Option<&'static str> {
    let said = line.trim().strip_prefix("Error:")?;
    Some(refused_by(said).unwrap_or(refusal::START_FAILED))
}

/// The refusal word of what Grok said it would not run, where it is one.
fn refused_by(said: &str) -> Option<&'static str> {
    if said.contains("Couldn't set model") || said.contains("unknown model id") {
        Some(refusal::MODEL_MISSING)
    } else if said.contains("No saved copy of session") || said.contains("not found") {
        Some(refusal::NOT_FOUND)
    } else if said.contains("Not signed in") {
        Some(refusal::REFUSED)
    } else if said.contains("already in use") {
        Some(refusal::HARNESS_BUSY)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_result_says_how_the_turn_ended() {
        let state = |result: Value| ended(&result);
        assert_eq!(
            state(json!({"subtype": "success", "is_error": false, "stop_reason": "end_turn"})),
            Ended {
                state: TurnState::Finished,
                error: None
            }
        );
        assert_eq!(
            state(
                json!({"subtype": "error_during_execution", "is_error": true,
                         "stop_reason": "cancelled", "result": "null"})
            )
            .state,
            TurnState::Interrupted
        );
        let failed = state(
            json!({"subtype": "error_during_execution", "is_error": true,
            "stop_reason": null,
            "errors": ["Couldn't set model 'x': Invalid params: \"unknown model id\"."]}),
        );
        assert_eq!(failed.state, TurnState::Failed);
        assert_eq!(
            refused_by(failed.error.as_deref().unwrap()),
            Some(refusal::MODEL_MISSING)
        );
    }

    #[test]
    fn only_grok_s_error_line_refuses() {
        assert_eq!(
            refusal_of("Error: Not signed in. To authenticate without a browser, run:"),
            Some(refusal::REFUSED)
        );
        assert_eq!(
            refusal_of(
                "Error: Couldn't set model 'no-such-model': Invalid params: \"unknown model id\". Run 'grok models' to see available models."
            ),
            Some(refusal::MODEL_MISSING)
        );
        assert_eq!(
            refusal_of("Error: No saved copy of session 0000 was found remotely."),
            Some(refusal::NOT_FOUND)
        );
        assert_eq!(
            refusal_of("2026-11-10T12:36:13.240586Z  WARN preferred model not in available models"),
            None
        );
        assert_eq!(
            refusal_of(
                "2026-11-10T12:36:13.927939Z ERROR worker quit with fatal: Transport channel closed, when AuthRequired"
            ),
            None
        );
        assert_eq!(
            refusal_of("Not signed in. To authenticate without a browser, run:"),
            None
        );
    }
}
