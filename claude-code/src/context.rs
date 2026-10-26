//! What the end of a conversation's transcript says it stands at now: the model and effort
//! of its newest main-thread answer, as Claude Code wrote them on that `assistant` record
//! (`message.model`, `effort`).

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::events::{SYNTHETIC, text};

/// How much of a transcript's end is read for where the conversation stands now.
const TAIL: u64 = 1024 * 1024;

/// The whole records within the last [`TAIL`] bytes of `transcript`, newest first.
pub fn newest_first(transcript: &Path) -> Vec<Value> {
    let Ok(mut file) = std::fs::File::open(transcript) else {
        return Vec::new();
    };
    let Ok(meta) = file.metadata() else {
        return Vec::new();
    };
    let start = meta.len().saturating_sub(TAIL);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let whole = if start > 0 {
        match bytes.iter().position(|byte| *byte == b'\n') {
            Some(at) => &bytes[at + 1..],
            None => return Vec::new(),
        }
    } else {
        &bytes[..]
    };
    whole
        .split(|byte| *byte == b'\n')
        .rev()
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .collect()
}

/// Where a conversation stands now, as its transcript's end says it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Now {
    pub model: Option<String>,
    pub thinking: Option<String>,
}

/// [`Now`], from a transcript's records newest first: its newest answer of the model's own
/// in the main thread, never a sub-agent's inlined record or one Claude Code wrote itself.
pub fn now(newest_first: &[Value]) -> Now {
    let Some(answer) = newest_first.iter().find(|record| main_answer(record)) else {
        return Now::default();
    };
    Now {
        model: answer
            .pointer("/message/model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(str::to_owned),
        thinking: text(answer, "effort")
            .filter(|effort| !effort.is_empty())
            .map(str::to_owned),
    }
}

/// Whether a record is an answer of the model's own in the conversation's main thread.
fn main_answer(record: &Value) -> bool {
    text(record, "type") == Some("assistant")
        && record.get("isSidechain").and_then(Value::as_bool) != Some(true)
        && record.get("isApiErrorMessage").and_then(Value::as_bool) != Some(true)
        && record
            .pointer("/message/model")
            .and_then(Value::as_str)
            .is_some_and(|model| model.trim() != SYNTHETIC)
}
