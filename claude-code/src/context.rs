//! What the end of a conversation's transcript says it stands at now: the model and effort
//! of its newest main-thread answer, as Claude Code wrote them on that `assistant` record
//! (`message.model`, `effort`), how much of its context window is left, and when its
//! newest turn ended.
//!
//! The context is what the newest answer's `message.usage` counted (its input, fresh and
//! from or into a cache, and its output), or after a compaction with no answer since, what
//! the `compact_boundary` says it left (`postTokens`). The window is the one Claude Code
//! gives the model (https://code.claude.com/docs/en/model-config, "Extended context"):
//! 1M for an id carrying `[1m]` and for the models that run with it natively, else 200K;
//! an auto-compact window a person set is the effective one, capped at the model's.
//!
//! A transcript is read back from its end only as far as its newest answer, in windows
//! that grow until one is found, then on from where it was read as it grows ([`Tails`]).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde_json::Value;
use steery_driver_common::quiet::Stamp;

use crate::events::{ENDS, SYNTHETIC, millis, text};

/// The window of a model with an extended context.
const EXTENDED: u64 = 1_000_000;
/// The window of every other model, and of one Claude Code does not recognize.
const STANDARD: u64 = 200_000;
/// The least auto-compact window Claude Code takes.
const LEAST_WINDOW: u64 = 100_000;

/// How much of a transcript's end is read first, looking back; each look after reads four
/// times as much, until the file is read whole.
const FIRST_LOOK: u64 = 1024 * 1024;

/// The newest record of `transcript` for which `wanted` holds, among its whole lines.
pub fn newest(transcript: &Path, wanted: impl Fn(&Value) -> bool) -> Option<Value> {
    let mut file = File::open(transcript).ok()?;
    let end = file.metadata().ok()?.len();
    back(&mut file, end, wanted).map(|(_, record)| record)
}

/// The newest record before `end` for which `wanted` holds, and where its line starts.
fn back(file: &mut File, end: u64, wanted: impl Fn(&Value) -> bool) -> Option<(u64, Value)> {
    let mut look = FIRST_LOOK;
    loop {
        let start = end.saturating_sub(look);
        let (base, bytes) = whole(file, start, end).ok()?;
        let mut at = bytes.len();
        while at > 0 {
            let from = bytes[..at - 1]
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |newline| newline + 1);
            if let Ok(record) = serde_json::from_slice::<Value>(&bytes[from..at - 1])
                && wanted(&record)
            {
                return Some((base + from as u64, record));
            }
            at = from;
        }
        if start == 0 {
            return None;
        }
        look = look.saturating_mul(4);
    }
}

/// The whole lines between `start` and `end`, each ending in its newline, and where the
/// first starts: a line `start` cuts is left out, one it begins is kept.
fn whole(file: &mut File, start: u64, end: u64) -> std::io::Result<(u64, Vec<u8>)> {
    // The byte before `start` says whether a line begins there.
    let from = start.saturating_sub(1);
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::new();
    file.take(end - from).read_to_end(&mut bytes)?;
    let skip = if start > 0 {
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |newline| newline + 1)
    } else {
        0
    };
    let last = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1)
        .max(skip);
    bytes.truncate(last);
    bytes.drain(..skip);
    Ok((from + skip as u64, bytes))
}

/// Where a conversation stands now, as its transcript's end says it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Now {
    pub model: Option<String>,
    pub thinking: Option<String>,
    /// The share of the context window left, 0 to 1.
    pub context_left: Option<f64>,
    /// When its newest turn ended, in milliseconds, where the records read say so.
    pub ended: Option<u64>,
}

/// What the records read so far say, newest last.
#[derive(Debug, Clone, Default)]
struct Seen {
    model: Option<String>,
    thinking: Option<String>,
    requested: Option<String>,
    held: Option<u64>,
    ended: Option<u64>,
}

impl Seen {
    /// One more record, in the order they were written: a sub-agent's inlined record or
    /// one Claude Code wrote itself says nothing of the conversation's model.
    fn record(&mut self, record: &Value) {
        if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let at = || text(record, "timestamp").and_then(millis);
        if main_answer(record) {
            let message = &record["message"];
            self.model = text(message, "model")
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_owned);
            self.thinking = text(record, "effort")
                .filter(|effort| !effort.is_empty())
                .map(str::to_owned);
            self.requested = text(record, "requestedModel").map(str::to_owned);
            if let Some(usage) = message.get("usage") {
                self.held = Some(used(usage));
            }
            if text(message, "stop_reason").is_some_and(|reason| ENDS.contains(&reason)) {
                self.ended = at().or(self.ended);
            }
        } else if text(record, "type") == Some("system") {
            match text(record, "subtype") {
                Some("compact_boundary") => {
                    if let Some(after) = record.pointer("/compactMetadata/postTokens") {
                        self.held = after.as_u64().or(self.held);
                    }
                }
                Some("turn_duration") => self.ended = at().or(self.ended),
                _ => {}
            }
        }
    }

    fn now(&self, window: &Window) -> Now {
        Now {
            model: self.model.clone(),
            thinking: self.thinking.clone(),
            context_left: self
                .model
                .as_deref()
                .zip(self.held)
                .map(|(model, held)| window.left(model, self.requested.as_deref(), held)),
            ended: self.ended,
        }
    }
}

/// What each transcript asked about said as far as it was read, and the window the
/// settings say, read again only once `settings.json` changed.
pub struct Tails {
    root: PathBuf,
    read: Mutex<HashMap<PathBuf, Kept>>,
    window: Mutex<Option<(Option<Stamp>, Window)>>,
}

/// One transcript as far as it was read: which file, and the end of its last whole line.
struct Kept {
    file: (u64, u64),
    through: u64,
    seen: Seen,
}

impl Tails {
    /// For the conversations of Claude Code's configuration root `root`.
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            read: Mutex::new(HashMap::new()),
            window: Mutex::new(None),
        }
    }

    /// The window the settings say now.
    pub fn window(&self) -> Window {
        let stamp = Stamp::at(&self.root.join("settings.json"));
        let mut kept = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        match kept.as_ref() {
            Some((at, window)) if *at == stamp && stamp.is_some() => window.clone(),
            _ => {
                let window = Window::read(&self.root);
                *kept = Some((stamp, window.clone()));
                window
            }
        }
    }

    /// Where the conversation `transcript` holds stands now: what was appended since the
    /// last look is read on, and a file new to this or no longer the one read is read back
    /// from its end as far as its newest answer.
    pub fn now(&self, transcript: &Path) -> Now {
        let window = self.window();
        let Ok(mut file) = File::open(transcript) else {
            return Now::default();
        };
        let Some(stamp) = file.metadata().ok().and_then(|meta| Stamp::of(&meta)) else {
            return Now::default();
        };
        let end = stamp.length();
        let kept = self
            .read
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(transcript)
            .filter(|read| read.file == stamp.file() && read.through <= end);
        let (from, mut seen) = match kept {
            Some(read) => (read.through, read.seen),
            None => (
                back(&mut file, end, main_answer).map_or(0, |(at, _)| at),
                Seen::default(),
            ),
        };
        let through = match whole(&mut file, from, end) {
            Ok((base, bytes)) => {
                for line in bytes.split(|byte| *byte == b'\n') {
                    if let Ok(record) = serde_json::from_slice::<Value>(line) {
                        seen.record(&record);
                    }
                }
                base + bytes.len() as u64
            }
            Err(_) => from,
        };
        let now = seen.now(&window);
        let mut read = self.read.lock().unwrap_or_else(PoisonError::into_inner);
        read.insert(
            transcript.to_owned(),
            Kept {
                file: stamp.file(),
                through,
                seen,
            },
        );
        now
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
    fn read(root: &Path) -> Self {
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