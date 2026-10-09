//! omp's conversations, from their transcripts: `<root>/sessions/<folder>/<started-at>_<id>.jsonl`.
//!
//! A transcript opens with a fixed-width `title` record omp rewrites in place, then the
//! `session` record naming the conversation and its folder, then entries, each one JSON
//! record per line. omp only appends below the title, so a byte offset in the file is a
//! cursor that stays good. A sub-agent's transcript is filed in a folder of its lead's
//! and is not listed here.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;
use steery_sdk::wire::harness::{
    Event, Events, Failure, FailureKind, List, Listed, Read as ReadParams, Session, TurnState,
    Voice,
};
use steery_sdk::{Error, refusal};

/// Every transcript under `root`: `sessions/*/*.jsonl`.
fn files(root: &Path) -> Vec<PathBuf> {
    let Ok(folders) = std::fs::read_dir(root.join("sessions")) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for folder in folders.flatten() {
        let Ok(entries) = std::fs::read_dir(folder.path()) else {
            continue;
        };
        files.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
            path.extension().is_some_and(|ext| ext == "jsonl") && path.is_file()
        }));
    }
    files
}

/// The id a transcript is filed under: all that follows the first `_` of its name. The
/// stamp before it holds none, and an id may.
fn id_of(file: &Path) -> Option<&str> {
    Some(file.file_stem()?.to_str()?.split_once('_')?.1).filter(|id| !id.is_empty())
}

/// The transcript of conversation `id`.
pub fn find(root: &Path, id: &str) -> Option<PathBuf> {
    files(root)
        .into_iter()
        .find(|file| id_of(file) == Some(id))
}

/// What the head of a transcript records about its conversation.
#[derive(Default)]
pub struct Head {
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    pub created_at: Option<u64>,
}

/// The title omp keeps current, else the one it started with, and the folder and start
/// from the `session` record: all within the first 4 KiB.
pub fn head(file: &Path) -> Head {
    let mut found = Head::default();
    let mut bytes = Vec::new();
    let Ok(open) = File::open(file) else {
        return found;
    };
    if open.take(4096).read_to_end(&mut bytes).is_err() {
        return found;
    }
    let mut started = None;
    let mut session = false;
    for line in bytes.split(|&b| b == b'\n') {
        if line.starts_with(br#"{"type":"title""#) {
            if found.title.is_none()
                && let Ok(record) = serde_json::from_slice::<Value>(line)
            {
                found.title = text(&record, "title").and_then(clean);
            }
        } else if !session
            && line.starts_with(br#"{"type":"session""#)
            && let Ok(record) = serde_json::from_slice::<Value>(line)
        {
            session = true;
            started = text(&record, "title").and_then(clean);
            found.cwd = text(&record, "cwd")
                .map(PathBuf::from)
                .filter(|cwd| cwd.is_absolute());
            found.created_at = text(&record, "timestamp").and_then(millis);
        }
    }
    found.title = found.title.or(started);
    found
}

/// `sessions/list`.
pub fn list(root: &Path, list: List) -> Listed {
    let mut sessions = Vec::new();
    for file in files(root) {
        let Some(external_id) = id_of(&file) else {
            continue;
        };
        let Some(updated_at) = std::fs::metadata(&file)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok())
            .and_then(|at| u64::try_from(at.as_millis()).ok())
        else {
            continue;
        };
        if list.seen_since.is_some_and(|since| updated_at < since) {
            continue;
        }
        let head = head(&file);
        sessions.push(Session {
            external_id: external_id.to_owned(),
            cwd: head.cwd,
            title: head.title,
            created_at: head.created_at,
            updated_at,
            transcript: Some(file),
            lead: None,
        });
    }
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Listed { sessions }
}

/// `sessions/read`: every whole line after the cursor, as events. The cursor is the byte
/// offset after the last whole line read.
pub fn read(root: &Path, read: ReadParams) -> Result<Events, Error> {
    let file = find(root, &read.external_id).ok_or_else(|| {
        Error::refused(
            refusal::NOT_FOUND,
            format!("omp has no conversation {}", read.external_id),
        )
    })?;
    let from: u64 = match read.cursor.as_deref() {
        None | Some("") => 0,
        Some(cursor) => cursor
            .parse()
            .map_err(|_| Error::invalid(format!("`{cursor}` is no byte offset")))?,
    };
    let unreadable = |e: std::io::Error| {
        Error::refused(refusal::REFUSED, format!("{}: {e}", file.display()))
    };
    let mut open = File::open(&file).map_err(unreadable)?;
    let length = open.metadata().map_err(unreadable)?.len();
    if from > length {
        return Err(Error::invalid(format!(
            "the cursor {from} is past the transcript's {length} bytes"
        )));
    }
    let mut state = State::default();
    let mut line = Vec::new();
    // The model and thinking in force at the cursor, from the changes before it.
    if from > 0 {
        let mut before = BufReader::new((&open).take(from));
        loop {
            line.clear();
            match before.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(unreadable(e)),
            }
            if (line.starts_with(br#"{"type":"model_change""#)
                || line.starts_with(br#"{"type":"thinking_level_change""#))
                && let Ok(record) = serde_json::from_slice::<Value>(&line)
            {
                state.record(&record, &mut Vec::new());
            }
        }
    }
    open.seek(SeekFrom::Start(from)).map_err(unreadable)?;
    let mut rest = BufReader::new(open);
    let mut cursor = from;
    let mut events = Vec::new();
    loop {
        line.clear();
        let taken = rest.read_until(b'\n', &mut line).map_err(unreadable)?;
        // A line still being written is read next time, whole.
        if taken == 0 || line.last() != Some(&b'\n') {
            break;
        }
        cursor += taken as u64;
        if let Ok(record) = serde_json::from_slice::<Value>(&line) {
            state.record(&record, &mut events);
        }
    }
    Ok(Events {
        cursor: cursor.to_string(),
        events,
    })
}

/// What reading a transcript carries from one record to the next.
#[derive(Default)]
struct State {
    at: u64,
    model: Option<String>,
    thinking: Option<String>,
}

impl State {
    fn record(&mut self, record: &Value, out: &mut Vec<Event>) {
        if let Some(at) = text(record, "timestamp").and_then(millis) {
            self.at = at;
        }
        let at = self.at;
        match text(record, "type") {
            Some("model_change") => {
                if let Some(model) = text(record, "model") {
                    self.model = Some(model.to_owned());
                    out.push(Event::Model {
                        at,
                        model: model.to_owned(),
                        thinking: self.thinking.clone(),
                    });
                }
            }
            Some("thinking_level_change") => {
                self.thinking = text(record, "thinkingLevel").map(str::to_owned);
                if let Some(model) = &self.model {
                    out.push(Event::Model {
                        at,
                        model: model.clone(),
                        thinking: self.thinking.clone(),
                    });
                }
            }
            // Steery's own words, put in by its plugin.
            Some("custom_message") if text(record, "customType") == Some("steery") => {
                words(Voice::Steery, at, record.get("content"), out);
            }
            Some("message") => {
                if let Some(message) = record.get("message") {
                    self.message(at, message, out);
                }
            }
            _ => {}
        }
    }

    fn message(&mut self, at: u64, message: &Value, out: &mut Vec<Event>) {
        match text(message, "role") {
            Some("user") => {
                out.push(Event::TurnBegan { at });
                words(Voice::Person, at, message.get("content"), out);
            }
            Some("assistant") => {
                words(Voice::Agent, at, message.get("content"), out);
                for block in message
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if text(block, "type") == Some("toolCall")
                        && let (Some(id), Some(tool)) = (text(block, "id"), text(block, "name"))
                    {
                        out.push(Event::CallBegan {
                            at,
                            id: id.to_owned(),
                            tool: tool.to_owned(),
                            group: None,
                        });
                    }
                }
                if let Some(usage) = message.get("usage") {
                    out.push(tokens(at, message, usage));
                }
                let state = match text(message, "stopReason") {
                    Some("toolUse") | None => return,
                    Some("error") => TurnState::Failed,
                    Some("aborted") => TurnState::Interrupted,
                    Some(_) => TurnState::Finished,
                };
                out.push(Event::TurnEnded {
                    at,
                    state,
                    failure: (state == TurnState::Failed).then(|| failure(message)),
                });
            }
            Some("toolResult") => {
                if let Some(id) = text(message, "toolCallId") {
                    out.push(Event::CallEnded {
                        at,
                        id: id.to_owned(),
                        ok: message.get("isError").and_then(Value::as_bool) != Some(true),
                    });
                }
            }
            // developer, fileMention, bashExecution, pythonExecution and the rest: nobody's
            // words in the conversation.
            _ => {}
        }
    }
}

/// The words of a message's content: its text, or its text blocks joined. Thinking and
/// tool calls are not words.
fn words(voice: Voice, at: u64, content: Option<&Value>, out: &mut Vec<Event>) {
    let text = match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| text(block, "type") == Some("text"))
            .filter_map(|block| text(block, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return,
    };
    if !text.trim().is_empty() {
        out.push(Event::Words { at, voice, text });
    }
}

/// An assistant message's `usage`. `input` counts what the model read, the cache's reads
/// and writes included; `cacheWrite1h` is a part of `cacheWrite`, and `totalTokens` the
/// sum of the rest.
fn tokens(at: u64, message: &Value, usage: &Value) -> Event {
    let number = |name: &str| usage.get(name).and_then(Value::as_u64);
    let cache_read = number("cacheRead");
    let cache_write = number("cacheWrite");
    let model = match (text(message, "provider"), text(message, "model")) {
        (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
        (None, Some(model)) => Some(model.to_owned()),
        _ => None,
    };
    Event::Tokens {
        at,
        model,
        input: number("input").unwrap_or(0) + cache_read.unwrap_or(0) + cache_write.unwrap_or(0),
        output: number("output").unwrap_or(0),
        cache_read,
        cache_write,
    }
}

/// Why a model call failed, as omp classified it: the HTTP status it kept, else its own
/// `errorId` (pi-ai `error/flags.ts`, the same in omp 18.4.12). Never read from wording.
fn failure(message: &Value) -> Failure {
    const CLASSIFIED: u64 = 0x1000;
    const KINDS: u64 = 0xFFFF_E000;
    const AUTH_FAILED: u64 = 0x0100_0000;
    const USAGE_LIMIT: u64 = 0x8_0000;
    const CONTEXT_OVERFLOW: u64 = 0x80_0000;
    let status = message
        .get("errorStatus")
        .and_then(Value::as_u64)
        .filter(|status| (100..=599).contains(status));
    let id = message.get("errorId").and_then(Value::as_u64).unwrap_or(0);
    let classified = |bit: u64| id & (CLASSIFIED | bit) == CLASSIFIED | bit;
    let kind = if classified(CONTEXT_OVERFLOW) {
        FailureKind::ContextFull
    } else if status == Some(401)
        || (status.is_none() && id & CLASSIFIED != 0 && id & KINDS == AUTH_FAILED)
    {
        FailureKind::SignIn
    } else if status.is_some_and(|status| matches!(status, 429 | 503 | 529))
        || classified(USAGE_LIMIT)
    {
        FailureKind::RateLimit
    } else {
        FailureKind::Other
    };
    Failure {
        kind,
        line: text(message, "errorMessage").unwrap_or_default().to_owned(),
    }
}

fn text<'a>(value: &'a Value, name: &str) -> Option<&'a str> {
    value.get(name).and_then(Value::as_str)
}

/// A title as a person reads it: trimmed, and none where empty.
fn clean(title: &str) -> Option<String> {
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_owned())
}

/// An RFC 3339 time in milliseconds since 1970.
fn millis(at: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}
