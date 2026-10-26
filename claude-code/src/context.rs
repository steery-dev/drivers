//! What the end of a conversation's transcript says it stands at now: the model and effort
//! of its newest main-thread answer, as Claude Code wrote them on that `assistant` record
//! (`message.model`, `effort`), and how much of its context window is left.
//!
//! The context is what the newest answer's `message.usage` counted (its input, fresh and
//! from or into a cache, and its output), or after a compaction with no answer since, what
//! the `compact_boundary` says it left (`postTokens`). The window is the one Claude Code
//! gives the model (https://code.claude.com/docs/en/model-config, "Extended context"):
//! 1M for an id carrying `[1m]` and for the models that run with it natively, else 200K;
//! an auto-compact window a person set is the effective one, capped at the model's.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

use crate::events::{SYNTHETIC, text};

/// The window of a model with an extended context.
const EXTENDED: u64 = 1_000_000;
/// The window of every other model, and of one Claude Code does not recognize.
const STANDARD: u64 = 200_000;
/// The least auto-compact window Claude Code takes.
const LEAST_WINDOW: u64 = 100_000;

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
    /// The share of the context window left, 0 to 1.
    pub context_left: Option<f64>,
}

/// [`Now`], from a transcript's records newest first: its newest answer of the model's own
/// in the main thread, never a sub-agent's inlined record or one Claude Code wrote itself.
pub fn now(newest_first: &[Value], window: &Window) -> Now {
    let Some(answer) = newest_first.iter().find(|record| main_answer(record)) else {
        return Now::default();
    };
    let model = answer
        .pointer("/message/model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty());
    let held = newest_first.iter().find_map(|record| {
        if main_answer(record) {
            record.pointer("/message/usage").map(used)
        } else if text(record, "subtype") == Some("compact_boundary")
            && record.get("isSidechain").and_then(Value::as_bool) != Some(true)
        {
            record.pointer("/compactMetadata/postTokens")?.as_u64()
        } else {
            None
        }
    });
    Now {
        model: model.map(str::to_owned),
        thinking: text(answer, "effort")
            .filter(|effort| !effort.is_empty())
            .map(str::to_owned),
        context_left: model
            .zip(held)
            .map(|(model, held)| window.left(model, text(answer, "requestedModel"), held)),
    }
}

/// What one answer's `usage` says its context held: its input, fresh, read from a cache
/// and written to one, and its output.
pub fn used(usage: &Value) -> u64 {
    [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
    ]
    .iter()
    .filter_map(|field| usage.get(field)?.as_u64())
    .sum()
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

/// What a person's Claude Code settings say of its context window: an auto-compact window
/// (`CLAUDE_CODE_AUTO_COMPACT_WINDOW`, which wins, else `autoCompactWindow`), and whether
/// extended context is off (`CLAUDE_CODE_DISABLE_1M_CONTEXT`). A variable is read from the
/// settings' `env`, which Claude Code puts in its own environment, else this process's.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Window {
    auto_compact: Option<u64>,
    standard_only: bool,
}

impl Window {
    /// The window settings under configuration root `root` say.
    pub fn read(root: &Path) -> Self {
        let settings: Value = std::fs::read(root.join("settings.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Value::Null);
        let variable = |name: &str| {
            settings
                .pointer(&format!("/env/{name}"))
                .and_then(|value| match value {
                    Value::String(said) => Some(said.clone()),
                    Value::Number(said) => Some(said.to_string()),
                    _ => None,
                })
                .or_else(|| std::env::var(name).ok())
        };
        let set = settings
            .get("autoCompactWindow")
            .and_then(Value::as_u64)
            .map(|tokens| tokens.clamp(LEAST_WINDOW, EXTENDED));
        Self {
            auto_compact: variable("CLAUDE_CODE_AUTO_COMPACT_WINDOW")
                .as_deref()
                .and_then(tokens)
                .or(set),
            standard_only: variable("CLAUDE_CODE_DISABLE_1M_CONTEXT")
                .is_some_and(|said| !said.is_empty() && said != "0" && said != "false"),
        }
    }

    /// The share of the window left with `used` tokens in it, for an answer of `model`
    /// Claude Code was asked for as `requested`.
    pub fn left(&self, model: &str, requested: Option<&str>, used: u64) -> f64 {
        let window = self.of(model, requested);
        (1.0 - used as f64 / window as f64).clamp(0.0, 1.0)
    }

    /// The effective window of `model`: the auto-compact window where one is set, capped
    /// at the model's own.
    fn of(&self, model: &str, requested: Option<&str>) -> u64 {
        let extended = !self.standard_only
            && ([Some(model), requested]
                .into_iter()
                .flatten()
                .any(|id| id.to_ascii_lowercase().contains("[1m]"))
                || natively_extended(model));
        let full = if extended { EXTENDED } else { STANDARD };
        self.auto_compact.map_or(full, |window| window.min(full))
    }
}

/// A token count as `CLAUDE_CODE_AUTO_COMPACT_WINDOW` takes it: a number of tokens, or
/// thousands or millions with a `k` or `M`, taken within 100K to 1M; none where it is no
/// count, which Claude Code ignores.
fn tokens(said: &str) -> Option<u64> {
    let said = said.trim();
    let (number, times) = match said.char_indices().last()? {
        (at, 'k' | 'K') => (&said[..at], 1_000),
        (at, 'm' | 'M') => (&said[..at], 1_000_000),
        _ => (said, 1),
    };
    let count = number.trim().parse::<f64>().ok().filter(|n| *n > 0.0)? * times as f64;
    Some((count as u64).clamp(LEAST_WINDOW, EXTENDED))
}

/// Whether a model runs with the 1M window with no `[1m]` asked: the Fable models, Sonnet 5
/// and later, Haiku 5.5 and later, Opus 4.7 and later, by the version in their id
/// (`claude-<family>-<major>-<minor>`, anywhere in a provider's spelling).
fn natively_extended(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    let Some((_, named)) = model.split_once("claude-") else {
        return false;
    };
    let mut parts = named.split(['-', '@', '.', ':', '[']);
    let family = parts.next().unwrap_or_default();
    let mut number = || parts.next().and_then(|part| part.parse::<u32>().ok());
    let major = number();
    let version = (major.unwrap_or(0), number().filter(|minor| *minor < 100).unwrap_or(0));
    match family {
        "fable" => true,
        "sonnet" => version >= (5, 0),
        "haiku" => version >= (5, 5),
        "opus" => version >= (4, 7),
        _ => false,
    }
}