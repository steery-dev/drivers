//! What the end of a conversation's transcript says it stands at now: the model and effort
//! of its newest main-thread answer, as Claude Code wrote them on that `assistant` record
//! (`message.model`, `effort`), how much of its context window is left, when its newest
//! turn ended, the request it waits to try again (an `api_error` record no answer
//! followed), the background work its calls left running and no notice told over yet, and
//! its newest call with no result.
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
use steery_sdk::wire::harness::LiveRetry;

use crate::events::{self, ENDS, SYNTHETIC, millis, text};

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
    /// The newest request that failed, where no answer came after it, and until when
    /// Claude Code said it waits before it tries again.
    pub retry: Option<Retry>,
    /// When each piece of background work the main thread's calls left running began, in
    /// milliseconds where its record says, oldest first: work no notice told over yet.
    pub jobs: Vec<Option<u64>>,
    /// The call of the newest answer first in line for its result: its tool, and what it
    /// acts on where its input names that (`Bash: rm -rf build`). Claude Code runs an
    /// answer's calls in its order, one not safe beside others once those before it are
    /// done, and asks its person about a call as it comes to run it.
    pub open: Option<String>,
}

/// A request Claude Code waits to try again, as its `api_error` record says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retry {
    /// When it tries again, in milliseconds: the record's time and its `retryInMs`.
    pub until: u64,
    pub retry: LiveRetry,
}

/// The most calls, jobs and stops kept, the newest kept.
const KEPT: usize = 64;

/// The most characters of what a call acts on said in [`Now::open`].
const SUBJECT: usize = 100;

/// The most characters of a failed request's words said (PROTOCOL.md §4.3).
const RETRY_WORDS: usize = 600;

/// What the records read so far say, newest last.
#[derive(Debug, Clone, Default)]
struct Seen {
    model: Option<String>,
    thinking: Option<String>,
    requested: Option<String>,
    held: Option<u64>,
    ended: Option<u64>,
    retry: Option<Retry>,
    /// The work left running, by Claude Code's id, with when its record began, oldest
    /// first.
    jobs: Vec<(String, Option<u64>)>,
    /// The calls that stop work ([`crate::events::STOPS`]), by id, and the work each names.
    stops: Vec<(String, String)>,
    /// The newest answer, by its `message.id`, whose calls `open` holds.
    answer: Option<String>,
    /// The newest answer's calls with no result yet, in its order: id, tool, subject.
    open: Vec<(String, String, Option<String>)>,
}

impl Seen {
    /// One more record, in the order they were written: a sub-agent's inlined record or
    /// one Claude Code wrote itself says nothing of the conversation's model.
    fn record(&mut self, record: &Value) {
        if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let at = || text(record, "timestamp").and_then(millis);
        if let Some(work) = events::job_told(record) {
            self.jobs.retain(|(job, _)| *job != work);
        }
        match text(record, "type") {
            // An answer, or Claude Code's own error in place of one, ends the wait.
            Some("assistant") => self.retry = None,
            Some("user") => self.results(record, at()),
            _ => {}
        }
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
            self.calls(message);
        } else if text(record, "type") == Some("system") {
            match text(record, "subtype") {
                Some("compact_boundary") => {
                    if let Some(after) = record.pointer("/compactMetadata/postTokens") {
                        self.held = after.as_u64().or(self.held);
                    }
                }
                Some("turn_duration") => self.ended = at().or(self.ended),
                Some("api_error") => self.retry = retry(record),
                _ => {}
            }
        }
    }

    /// The calls an answer makes: each open until its result is written. Claude Code
    /// writes one record per block of an answer, each with the answer's `message.id`, and
    /// asks for the next answer only once every call of this one has its result, so a call
    /// of an earlier answer still open is one a process that stopped left without one.
    fn calls(&mut self, message: &Value) {
        if let Some(answer) = text(message, "id")
            && self.answer.as_deref() != Some(answer)
        {
            self.open.clear();
            self.answer = Some(answer.to_owned());
        }
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            return;
        };
        for block in blocks {
            if text(block, "type") != Some("tool_use") {
                continue;
            }
            let (Some(id), Some(tool)) = (text(block, "id"), text(block, "name")) else {
                continue;
            };
            let input = block.get("input").unwrap_or(&Value::Null);
            if events::STOPS.contains(&tool)
                && let Some(work) = events::waits_on(tool, input)
            {
                keep(&mut self.stops, (id.to_owned(), work.to_owned()));
            }
            keep(
                &mut self.open,
                (id.to_owned(), tool.to_owned(), subject(tool, input)),
            );
        }
    }

    /// A user record's call results, written `at`: each closes its call, and may leave
    /// work running, or stop work one left.
    fn results(&mut self, record: &Value, at: Option<u64>) {
        let Some(blocks) = record.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        for block in blocks {
            if text(block, "type") != Some("tool_result") {
                continue;
            }
            let Some(call) = text(block, "tool_use_id") else {
                continue;
            };
            self.open.retain(|(id, _, _)| id != call);
            if let Some(work) = events::job_started(record, block)
                && !self.jobs.iter().any(|(job, _)| *job == work)
            {
                keep(&mut self.jobs, (work, at));
            }
            if let Some(at) = self.stops.iter().position(|(id, _)| id == call) {
                let (_, work) = self.stops.remove(at);
                if block.get("is_error").and_then(Value::as_bool) != Some(true) {
                    self.jobs.retain(|(job, _)| *job != work);
                }
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
            retry: self.retry.clone(),
            jobs: self.jobs.iter().map(|(_, began)| *began).collect(),
            open: self.open.first().map(|(_, tool, subject)| match subject {
                Some(subject) => format!("{tool}: {subject}"),
                None => tool.clone(),
            }),
        }
    }
}

/// `item` kept as the newest of `kept`, the oldest let go past [`KEPT`].
fn keep<T>(kept: &mut Vec<T>, item: T) {
    kept.push(item);
    let past = kept.len().saturating_sub(KEPT);
    kept.drain(..past);
}

/// What a call acts on, as its input names it: a command, a file, a page or a pattern,
/// on one line and at most [`SUBJECT`] characters.
fn subject(tool: &str, input: &Value) -> Option<String> {
    let field = match tool {
        "Bash" | "PowerShell" => "command",
        "Read" | "Write" | "Edit" | "MultiEdit" => "file_path",
        "NotebookEdit" => "notebook_path",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Glob" | "Grep" => "pattern",
        _ => return None,
    };
    let said = events::one_line(text(input, field)?);
    (!said.is_empty()).then(|| cut(&said, SUBJECT))
}

/// At most `most` characters of `said`, an ellipsis ending one cut short.
fn cut(said: &str, most: usize) -> String {
    match said.char_indices().nth(most) {
        Some((at, _)) => format!("{}…", &said[..at]),
        None => said.to_owned(),
    }
}

/// The wait an `api_error` record says: Claude Code writes one as a request fails and
/// before it waits `retryInMs` to try again, with the try that failed (`retryAttempt`, the
/// first 1), the retries it makes (`maxRetries`, so one try more in all) and its error,
/// in the words it shows (`error.formatted`, else the service's `error.message`).
fn retry(record: &Value) -> Option<Retry> {
    let wait = record.get("retryInMs")?.as_u64()?;
    let at = text(record, "timestamp").and_then(millis)?;
    let attempt = record
        .get("retryAttempt")
        .and_then(Value::as_u64)
        .and_then(|attempt| u32::try_from(attempt).ok())?;
    let of = record
        .get("maxRetries")
        .and_then(Value::as_u64)
        .and_then(|retries| u32::try_from(retries).ok())
        .map(|retries| retries.saturating_add(1));
    let error = record.get("error");
    let words = error
        .and_then(|error| text(error, "formatted").or_else(|| text(error, "message")))
        .or_else(|| error.and_then(Value::as_str))
        .map(events::one_line)
        .filter(|words| !words.is_empty())
        .map(|words| cut(&words, RETRY_WORDS - 1));
    Some(Retry {
        until: at.saturating_add(wait),
        retry: LiveRetry {
            attempt,
            of,
            error: words,
        },
    })
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn seen(records: &[Value]) -> Now {
        let mut seen = Seen::default();
        for record in records {
            seen.record(record);
        }
        seen.now(&Window::default())
    }

    fn answer(blocks: Value) -> Value {
        json!({"type":"assistant","isSidechain":false,"timestamp":"2026-10-15T12:59:00.000Z",
            "message":{"model":"claude-opus-5-5","role":"assistant","content":blocks,
                "stop_reason":"tool_use"}})
    }

    /// One block of an answer, as Claude Code writes each in a record of its own carrying
    /// the answer's `message.id`.
    fn answer_in(message: &str, blocks: Value) -> Value {
        let mut record = answer(blocks);
        record["message"]["id"] = json!(message);
        record
    }

    fn result(call: &str, said: &str, result: Value) -> Value {
        json!({"type":"user","isSidechain":false,"toolUseResult":result,
            "message":{"role":"user","content":[{"type":"tool_result","tool_use_id":call,"content":said}]}})
    }

    /// As Claude Code 2.1.318 writes one before it waits to retry (`api_error`).
    fn api_error(at: &str) -> Value {
        json!({"isSidechain":false,"type":"system","subtype":"api_error","level":"error",
            "error":{"message":"429 {\"type\":\"error\"}","status":429,
                "formatted":"429 All 3 accounts exhausted. Retry in 60s."},
            "retryInMs":60000,"retryAttempt":1,"maxRetries":10,"timestamp":at})
    }

    #[test]
    fn a_failed_request_is_waited_on_until_an_answer_comes() {
        let failed = api_error("2026-10-15T12:59:38.774Z");
        let waiting = seen(std::slice::from_ref(&failed));
        assert_eq!(
            waiting.retry,
            Some(Retry {
                until: millis("2026-10-15T13:00:38.774Z").unwrap(),
                retry: LiveRetry {
                    attempt: 1,
                    of: Some(11),
                    error: Some("429 All 3 accounts exhausted. Retry in 60s.".into()),
                },
            })
        );
        assert_eq!(seen(&[failed, answer(json!([]))]).retry, None);
    }

    #[test]
    fn background_work_counts_until_it_is_told_or_stopped() {
        let notice = |work: &str, status: &str| {
            json!({"type":"attachment","isSidechain":false,"attachment":{"type":"queued_command",
                "commandMode":"task-notification",
                "prompt":format!("<task-notification>\n<task-id>{work}</task-id>\n<status>{status}</status>\n</task-notification>")}})
        };
        let check_in = json!({"type":"attachment","isSidechain":false,"attachment":{"type":"queued_command",
            "commandMode":"task-notification",
            "prompt":"<task-notification>\n<task-id>bj9ss2xbj</task-id>\n<summary>Background command \"build\" is still running</summary>\n</task-notification>"}});
        let monitor = |event: &str| {
            json!({"type":"attachment","isSidechain":false,"attachment":{"type":"queued_command",
                "commandMode":"task-notification",
                "prompt":format!("<task-notification>\n<task-id>bq10gp7qp</task-id>\n<summary>Monitor event: \"ship\"</summary>\n<event>{event}</event>\n</task-notification>")}})
        };
        let mut began = result(
            "t1",
            "Command running in background with ID: bj9ss2xbj. Output is being written to: /tmp/x",
            json!({"backgroundTaskId":"bj9ss2xbj"}),
        );
        began["timestamp"] = json!("2026-10-15T12:59:10.000Z");
        let started = [
            answer(
                json!([{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"make","run_in_background":true}}]),
            ),
            began,
            answer(json!([{"type":"tool_use","id":"t2","name":"Agent","input":{"prompt":"read"}}])),
            result(
                "t2",
                "Async agent launched",
                json!({"status":"async_launched","agentId":"a77006"}),
            ),
            answer(
                json!([{"type":"tool_use","id":"t3","name":"Monitor","input":{"command":"tail -f log"}}]),
            ),
            result(
                "t3",
                "Monitor started",
                json!({"taskId":"bq10gp7qp","timeoutMs":1_800_000,"persistent":false}),
            ),
        ];
        assert_eq!(
            seen(&started).jobs,
            [millis("2026-10-15T12:59:10.000Z"), None, None]
        );
        let mut told = started.to_vec();
        told.extend([
            check_in,
            notice("bj9ss2xbj", "completed"),
            monitor("SHIP_EXIT=0"),
        ]);
        assert_eq!(seen(&told).jobs.len(), 2);
        // Told while the session is idle, a notice is the words of a turn of its own.
        told.push(json!({"type":"user","isSidechain":false,"origin":{"kind":"task-notification"},
            "message":{"role":"user","content":"<task-notification>\n<task-id>a77006</task-id>\n<status>completed</status>\n<summary>Agent \"read\" finished</summary>\n</task-notification>"}}));
        assert_eq!(seen(&told).jobs.len(), 1);
        let mut stopped = told.clone();
        stopped.extend([
            answer(json!([{"type":"tool_use","id":"t4","name":"TaskStop","input":{"task_id":"bq10gp7qp"}}])),
            result("t4", "stopped", json!({})),
        ]);
        assert!(seen(&stopped).jobs.is_empty());
        told.push(monitor(
            "[Monitor expired after 30m with 16 events delivered. Re-arm it if you still need the watch.]",
        ));
        assert!(seen(&told).jobs.is_empty());
    }

    #[test]
    fn the_newest_answers_first_call_without_a_result_is_open() {
        let records = [
            // A process that stopped left this call with no result.
            answer_in(
                "m1",
                json!([{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"sleep 9"}}]),
            ),
            answer_in(
                "m2",
                json!([{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/tmp/a"}}]),
            ),
            answer_in(
                "m2",
                json!([{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"rm -rf\n  build"}}]),
            ),
        ];
        assert_eq!(seen(&records).open.as_deref(), Some("Read: /tmp/a"));
        let mut answered = records.to_vec();
        answered.push(result("t2", "", Value::Null));
        assert_eq!(seen(&answered).open.as_deref(), Some("Bash: rm -rf build"));
        answered.push(result("t3", "", Value::Null));
        assert_eq!(seen(&answered).open, None);
    }
}
