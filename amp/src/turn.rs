//! `turn/read`: one line of a turn that `amp -x --stream-json` runs. Its standard output is
//! Claude Code's stream-json: `system/init` names the thread (`T-…`) before Amp takes the
//! words, the `user` line echoes the words Amp took, and `result` ends the turn. Its
//! standard error is Amp's `Error: …` line where it would not run the turn.
//!
//! A line gives no event: Amp's stream-json carries none of the ids that `sessions/read`
//! gives, so the runtime reads the thread for every fact.

use serde_json::Value;
use steery_sdk::refusal;
use steery_sdk::wire::harness::{Ended, Stream, TurnLine, TurnRead, TurnState};

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
        .filter(|id| id.starts_with("T-"))
        .map(str::to_owned);
    match line["type"].as_str() {
        Some("system") if line["subtype"] == "init" => TurnLine {
            named,
            took: Some(false),
            ..TurnLine::default()
        },
        Some("user") if line["parent_tool_use_id"].is_null() => TurnLine {
            took: Some(took(&line, read.delivery.as_deref())),
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
        _ => TurnLine {
            named,
            ..TurnLine::default()
        },
    }
}

/// Whether the person's line holds the words that carry `delivery`, where one is named.
fn took(line: &Value, delivery: Option<&str>) -> bool {
    let Some(marker) = delivery else {
        return true;
    };
    line["message"]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .any(|text| text.contains(marker))
}

/// How a `result` line ends the turn: a success finished, any error failed with Amp's text.
fn ended(result: &Value) -> Ended {
    if result["is_error"] == false && result["subtype"] == "success" {
        return Ended {
            state: TurnState::Finished,
            error: None,
        };
    }
    let error = ["error", "result"]
        .iter()
        .filter_map(|key| result[*key].as_str())
        .find(|text| !text.trim().is_empty())
        .or_else(|| result["subtype"].as_str())
        .map(str::to_owned);
    Ended {
        state: TurnState::Failed,
        error,
    }
}

/// The refusal word that one line of Amp's standard error means: only its own `Error:` line,
/// never its log.
fn refusal_of(line: &str) -> Option<&'static str> {
    let said = line.trim().strip_prefix("Error:")?;
    Some(refused_by(said).unwrap_or(refusal::START_FAILED))
}

/// The refusal word of what Amp said it would not run, where it is one (Amp 0.0.1794225648).
fn refused_by(said: &str) -> Option<&'static str> {
    if said.contains("already open and active in another Amp") {
        Some(refusal::HARNESS_BUSY)
    } else if said.contains("does not exist") {
        Some(refusal::NOT_FOUND)
    } else if said.contains("Run 'amp login'") || said.contains("Out of Credits") {
        Some(refusal::REFUSED)
    } else {
        None
    }
}
