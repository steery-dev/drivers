//! What a pi-family transcript's records say happened, as the protocol's events.
//!
//! A turn begins at a `user` message and ends at the assistant message that stops of its
//! own accord (`stop`), on an `error` or `aborted`. omp and pi retry a failed model call
//! themselves and write the failed attempt first, so an error ends its turn only once the
//! conversation went no further: a person's next message, retries given up (an `aborted`
//! answer with nothing in it), the process leaving (`session_exit`), or the transcript
//! still for [`SETTLE`]. Until then the error and what follows it are read again later.
//!
//! pi also says the thinking level a conversation runs at, what its tool results and its
//! own summaries spent, and, through Steery's plugin, a prompt past the model's limit.

use std::path::{Path, PathBuf};

use serde_json::Value;
use steery_sdk::wire::harness::{
    Clip, Count, Event, Execution, Failure, FailureKind, TurnState, Voice,
};

use crate::{Harness, calls};

/// The custom entry Steery's plugin appends in pi once pi gave up compacting a prompt over
/// the model's limit.
const CONTEXT_FULL: &str = "steery-context-full";

/// How long a transcript that ends on a failed model call stays still before the failure
/// is the turn's end: longer than omp waits between its own retries.
pub const SETTLE: std::time::Duration = std::time::Duration::from_secs(60);

/// What reading one transcript needs besides its records.
pub struct Reading {
    pub harness: Harness,
    /// The transcript.
    pub file: PathBuf,
    /// omp's blob store, where a picture's bytes are kept by their digest.
    pub blobs: PathBuf,
    /// The folder the conversation runs in, against which a relative path is read.
    pub cwd: Option<PathBuf>,
    /// The MCP servers omp's configuration names.
    pub servers: Vec<String>,
    /// The conversation this one was forked from, and when the fork began: the records
    /// stamped before are the parent's, copied.
    pub fork: Option<(String, u64)>,
}

/// One whole record on the conversation's path, and where it starts: `None` where it is
/// no JSON.
pub struct Record {
    pub start: u64,
    pub value: Option<Value>,
}

/// What a run of records came to.
pub struct Read {
    pub events: Vec<Event>,
    /// How many of the records these events are of: a failure still undecided and what
    /// follows it are left for the next read.
    pub consumed: usize,
    /// The fork event was said.
    pub forked: bool,
}

/// The events of `records`, read on from where `forked` says the fork stands. `settled`:
/// a failure still undecided at their end is the turn's end; otherwise it and everything
/// after it are left for the next read.
pub fn read(reading: &Reading, records: &[Record], forked: bool, settled: bool) -> Read {
    let mut state = State {
        reading,
        at: 0,
        index: 0,
        forked,
        pending: None,
        model: None,
        thinking: None,
        out: Vec::new(),
    };
    let mut consumed = records.len();
    for (index, record) in records.iter().enumerate() {
        state.index = index;
        if let Some(value) = &record.value {
            state.record(record.start, value);
        }
    }
    match state.pending.take() {
        Some(pending) if !settled => {
            state.out.truncate(pending.mark);
            consumed = pending.index;
            state.forked = pending.forked;
        }
        Some(pending) => state.out.push(pending.ended()),
        None => {}
    }
    Read {
        events: state.out,
        consumed,
        forked: state.forked,
    }
}

/// A failed model call whose turn may still go on.
struct Pending {
    at: u64,
    failure: Failure,
    /// Which record it is, and how many events came before it.
    index: usize,
    mark: usize,
    forked: bool,
}

impl Pending {
    fn ended(self) -> Event {
        Event::TurnEnded {
            at: self.at,
            turn: None,
            state: TurnState::Failed,
            failure: Some(self.failure),
        }
    }
}

impl State<'_> {
    /// What the conversation runs on now: a `model` event where the model, or in pi the
    /// thinking level, changed. omp's transcript says no level Steery reads: its level is
    /// the door's alone.
    fn runs_on(&mut self, at: u64, model: Option<String>, thinking: Option<&str>) {
        let thinking = thinking
            .filter(|_| self.reading.harness == Harness::Pi)
            .map(str::trim)
            .filter(|level| !level.is_empty())
            .map(str::to_owned);
        let model = model.or_else(|| self.model.clone());
        let thinking = thinking.or_else(|| self.thinking.clone());
        if model == self.model && thinking == self.thinking {
            return;
        }
        self.model = model;
        self.thinking = thinking;
        if let Some(model) = &self.model {
            self.out.push(Event::Model {
                at,
                model: model.clone(),
                provider: model
                    .split_once('/')
                    .map(|(provider, _)| provider.to_owned()),
                thinking: self.thinking.clone(),
            });
        }
    }
}

struct State<'a> {
    reading: &'a Reading,
    at: u64,
    /// Which record is being read.
    index: usize,
    forked: bool,
    pending: Option<Pending>,
    /// What the conversation runs on, as last said.
    model: Option<String>,
    thinking: Option<String>,
    out: Vec<Event>,
}

impl State<'_> {
    fn record(&mut self, start: u64, record: &Value) {
        let kind = text(record, "type");
        let stamped = text(record, "timestamp").and_then(millis);
        if let Some(at) = stamped {
            self.at = at;
        }
        // The first record the fork wrote itself: everything before was its parent's.
        if !self.forked
            && let Some((parent, began)) = &self.reading.fork
            && !matches!(kind, Some("title" | "session"))
            && stamped.is_some_and(|at| at >= *began)
        {
            self.forked = true;
            self.out.push(Event::Fork {
                at: *began,
                parent: parent.clone(),
            });
        }
        let id = text(record, "id").map_or_else(|| format!("@{start}"), str::to_owned);
        let at = self.at;
        let pi = self.reading.harness == Harness::Pi;
        match kind {
            Some("model_change") => self.runs_on(at, selector(record), None),
            Some("thinking_level_change") => {
                self.runs_on(at, None, text(record, "thinkingLevel"));
            }
            Some("compaction" | "branch_summary") => {
                if let Some(summary) = text(record, "summary").filter(|s| !s.trim().is_empty()) {
                    self.words(
                        at,
                        id.clone(),
                        Voice::Summary,
                        summary.to_owned(),
                        None,
                        Vec::new(),
                    );
                }
                if pi && let Some(usage) = record.get("usage") {
                    self.out.push(tokens(at, &id, record, usage));
                }
            }
            // pi counts what its own entries spent too.
            Some("usage") if pi => {
                if let Some(usage) = record.get("usage") {
                    self.out.push(tokens(at, &id, record, usage));
                }
            }
            Some("custom_message") if text(record, "customType") == Some("async-result") => {
                self.results(at, record);
            }
            Some("custom") if text(record, "customType") == Some("session_exit") => {
                self.settle();
            }
            // Steery's plugin says so in pi, which classifies no error, once pi gave up
            // compacting a prompt over the model's limit.
            Some("custom") if pi && text(record, "customType") == Some(CONTEXT_FULL) => {
                if let Some(pending) = &mut self.pending {
                    pending.failure.kind = FailureKind::ContextFull;
                }
            }
            Some("message") => {
                if let Some(message) = record.get("message") {
                    self.message(at, id, message);
                }
            }
            _ => {}
        }
    }

    fn message(&mut self, at: u64, id: String, message: &Value) {
        match text(message, "role") {
            Some("user") => {
                self.settle();
                self.out.push(Event::TurnBegan { at, turn: None });
                // What a lead agent asks its sub-agent is no person's.
                if text(message, "attribution") != Some("agent") {
                    let (said, clips) = self.said(message.get("content"));
                    if !said.trim().is_empty() || !clips.is_empty() {
                        self.words(at, id, Voice::Person, said, None, clips);
                    }
                }
            }
            Some("fileMention") => {
                let clips = self.mentioned(message);
                if !clips.is_empty() {
                    self.words(at, id, Voice::Person, String::new(), None, clips);
                }
            }
            Some("assistant") => self.answer(at, id, message),
            Some("toolResult") => self.result(at, id, message),
            // developer, bashExecution, pythonExecution and the rest: nobody's words.
            _ => {}
        }
    }

    fn answer(&mut self, at: u64, id: String, message: &Value) {
        let stop = text(message, "stopReason");
        let blocks: &[Value] = message
            .get("content")
            .and_then(Value::as_array)
            .map_or(&[], Vec::as_slice);
        let typed = |kind: &str, field: &str| -> String {
            blocks
                .iter()
                .filter(|block| text(block, "type") == Some(kind))
                .filter_map(|block| text(block, field))
                .filter(|said| !said.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let thought = typed("thinking", "thinking");
        let said = typed("text", "text");
        let calls: Vec<&Value> = blocks
            .iter()
            .filter(|block| text(block, "type") == Some("toolCall"))
            .collect();
        // The model answered again: a failure before it was one omp got past.
        if stop != Some("error") && (!calls.is_empty() || !thought.is_empty() || !said.is_empty()) {
            self.pending = None;
        }
        let mark = self.out.len();
        let forked = self.forked;
        if !thought.is_empty() {
            self.words(
                at,
                format!("{id}:thought"),
                Voice::Thought,
                thought,
                None,
                Vec::new(),
            );
        }
        if !said.is_empty() {
            self.words(at, id.clone(), Voice::Agent, said, None, Vec::new());
        }
        for block in calls {
            let (Some(call), Some(name)) = (text(block, "id"), text(block, "name")) else {
                continue;
            };
            let input = block.get("arguments").cloned().unwrap_or(Value::Null);
            let carried = flattened(&input);
            self.out.push(Event::CallBegan {
                at,
                call: calls::call(call, name, input, Some(&id), &self.reading.servers),
            });
            if !carried.trim().is_empty() {
                self.words(
                    at,
                    format!("{id}:{call}"),
                    Voice::Tool,
                    carried,
                    Some(call.to_owned()),
                    Vec::new(),
                );
            }
        }
        self.runs_on(at, selector(message), text(message, "thinkingLevel"));
        if let Some(usage) = message.get("usage") {
            self.out.push(tokens(at, &id, message, usage));
        }
        match stop {
            Some("toolUse") | None => {}
            Some("error") => {
                self.pending = Some(Pending {
                    at,
                    failure: failure(message, at),
                    index: self.index,
                    mark,
                    forked,
                });
            }
            // Retries given up end the failed turn as failed; an abort alone interrupts it.
            Some("aborted") => match self.pending.take() {
                Some(pending) => self.out.push(pending.ended()),
                None => self.out.push(Event::TurnEnded {
                    at,
                    turn: None,
                    state: TurnState::Interrupted,
                    failure: None,
                }),
            },
            Some(_) => self.out.push(Event::TurnEnded {
                at,
                turn: None,
                state: TurnState::Finished,
                failure: None,
            }),
        }
    }

    /// A call's result: when it came, as omp stamped it on the message, and its text.
    fn result(&mut self, at: u64, id: String, message: &Value) {
        let Some(call) = text(message, "toolCallId") else {
            return;
        };
        let at = message
            .get("timestamp")
            .and_then(Value::as_u64)
            .unwrap_or(at);
        let said = texts(message.get("content"));
        if self.reading.harness == Harness::Pi
            && let Some(usage) = message.get("usage")
        {
            self.out.push(tokens(at, &id, message, usage));
        }
        let lines: Vec<&str> = said.lines().collect();
        let tail = lines[lines.len().saturating_sub(8)..].join("\n");
        self.out.push(Event::CallEnded {
            at,
            id: call.to_owned(),
            ok: message.get("isError").and_then(Value::as_bool) != Some(true),
            chars: said.chars().count() as u64,
            tail: (!tail.trim().is_empty()).then_some(tail),
        });
        if !said.trim().is_empty() {
            self.words(at, id, Voice::Tool, said, Some(call.to_owned()), Vec::new());
        }
        // `task` names the sub-agents it started by their jobs, which are their files.
        if text(message, "toolName") == Some("task") {
            let jobs = message
                .pointer("/details/progress")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|job| text(job, "id"));
            for job in jobs {
                if let Some(external_id) = self.sub_agent(job) {
                    self.out.push(Event::SubAgent {
                        at,
                        external_id,
                        call: Some(call.to_owned()),
                        state: Some(Execution::Running),
                        error: None,
                    });
                }
            }
        }
    }

    /// omp's `async-result`: one `<task-result id=… status=…>` for each background
    /// sub-agent that settled, named by its job: `completed`, `failed`, or `cancelled`
    /// with an `<abort-reason>`.
    fn results(&mut self, at: u64, record: &Value) {
        let said = texts(record.get("content"));
        for result in said.split("<task-result ").skip(1) {
            let result = result.split("</task-result>").next().unwrap_or(result);
            let opening = result.split('>').next().unwrap_or_default();
            let (Some(job), Some(status)) =
                (attribute(opening, "id"), attribute(opening, "status"))
            else {
                continue;
            };
            let state = match status {
                "completed" | "success" => Execution::Finished,
                "failed" | "error" => Execution::Failed,
                "cancelled" | "canceled" | "aborted" => Execution::Interrupted,
                _ => continue,
            };
            let error = ["error", "abort-reason"]
                .into_iter()
                .find_map(|tag| tagged(result, tag))
                .filter(|_| state != Execution::Finished)
                .map(one_line);
            if let Some(external_id) = self.sub_agent(job) {
                self.out.push(Event::SubAgent {
                    at,
                    external_id,
                    call: None,
                    state: Some(state),
                    error,
                });
            }
        }
    }

    /// The id of the sub-agent whose job is `job`: its transcript `<job>.jsonl` sits in the
    /// folder named for this one's, and its `session` header names it.
    fn sub_agent(&self, job: &str) -> Option<String> {
        if job.is_empty() || job.contains(['/', '\\']) || job.starts_with('.') {
            return None;
        }
        let file = self
            .reading
            .file
            .with_extension("")
            .join(format!("{job}.jsonl"));
        file.is_file()
            .then(|| crate::transcripts::head(&file).id)
            .map(|id| id.unwrap_or_else(|| job.to_owned()))
    }

    /// A failure still undecided is its turn's end.
    fn settle(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.out.push(pending.ended());
        }
    }

    fn words(
        &mut self,
        at: u64,
        id: String,
        voice: Voice,
        text: String,
        call: Option<String>,
        clips: Vec<Clip>,
    ) {
        self.out.push(Event::Words {
            at,
            id,
            voice,
            text,
            call,
            clips,
        });
    }

    /// A person's message: its text, and the pictures it carried.
    fn said(&self, content: Option<&Value>) -> (String, Vec<Clip>) {
        match content {
            Some(Value::String(said)) => (said.clone(), Vec::new()),
            Some(Value::Array(blocks)) => {
                let clips = blocks
                    .iter()
                    .filter(|block| text(block, "type") == Some("image"))
                    .filter_map(|block| self.picture(block, None))
                    .collect();
                (texts(content), clips)
            }
            _ => (String::new(), Vec::new()),
        }
    }

    /// omp's `fileMention`: the files a person named, each a picture where omp read one.
    fn mentioned(&self, message: &Value) -> Vec<Clip> {
        message
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|file| {
                let named = PathBuf::from(text(file, "path")?);
                let path = if named.is_absolute() {
                    named
                } else {
                    self.reading.cwd.as_ref()?.join(named)
                };
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned);
                match file.get("image") {
                    Some(image) => self.picture(image, name),
                    None => Some(Clip::Path { path, name }),
                }
            })
            .collect()
    }

    /// `{data, mimeType}`: the bytes in the record, or `blob:sha256:<digest>`, the file omp
    /// keeps them in.
    fn picture(&self, image: &Value, name: Option<String>) -> Option<Clip> {
        let data = text(image, "data")?;
        let media_type = text(image, "mimeType").map(str::to_owned);
        match data.strip_prefix("blob:sha256:") {
            Some(digest) => {
                let kept = digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit());
                let path = self.reading.blobs.join(digest);
                (kept && path.is_file()).then_some(Clip::File {
                    path,
                    media_type,
                    name,
                })
            }
            None => (!data.is_empty()).then(|| Clip::Bytes {
                data: data.to_owned(),
                media_type,
                name,
            }),
        }
    }
}

/// What one entry off the conversation's path spent, as its own `tokens`: an assistant's
/// answer, and in pi also a tool's result and pi's own summaries and `usage` entries. Each
/// is named by its entry's id, as on the path, so a response said twice counts once.
pub fn spent(reading: &Reading, record: &Value) -> Option<Event> {
    let id = text(record, "id")?;
    let at = text(record, "timestamp").and_then(millis).unwrap_or(0);
    let pi = reading.harness == Harness::Pi;
    match text(record, "type")? {
        "message" => {
            let message = record.get("message")?;
            let usage = message.get("usage")?;
            match text(message, "role")? {
                "assistant" => Some(tokens(at, id, message, usage)),
                "toolResult" if pi => {
                    let at = message
                        .get("timestamp")
                        .and_then(Value::as_u64)
                        .unwrap_or(at);
                    Some(tokens(at, id, message, usage))
                }
                _ => None,
            }
        }
        "compaction" | "branch_summary" | "usage" if pi => {
            Some(tokens(at, id, record, record.get("usage")?))
        }
        _ => None,
    }
}

/// One response's `usage`: `input` counts what the model read, the cache's reads and
/// writes included; `cacheWrite1h` is part of `cacheWrite`, and `totalTokens` the sum.
fn tokens(at: u64, response: &str, message: &Value, usage: &Value) -> Event {
    let number = |name: &str| usage.get(name).and_then(Value::as_u64);
    let cached = number("cacheRead").unwrap_or(0);
    let cache_write = number("cacheWrite");
    Event::Tokens {
        at,
        count: Count::Response,
        response: Some(response.to_owned()),
        model: selector(message),
        input: number("input").unwrap_or(0) + cached + cache_write.unwrap_or(0),
        cached,
        cache_write,
        output: number("output").unwrap_or(0),
        reasoning: number("reasoning").unwrap_or(0),
        context_left: None,
    }
}

/// A record's model as the harness selects it: `provider/model`, from a message's
/// `model` or a `model_change`'s `modelId` (pi) or `model` (omp, already a selector).
fn selector(message: &Value) -> Option<String> {
    let model = text(message, "modelId")
        .or_else(|| text(message, "model"))?
        .trim();
    if model.is_empty() {
        return None;
    }
    Some(
        match text(message, "provider").filter(|provider| !provider.is_empty()) {
            Some(provider) if !model.starts_with(&format!("{provider}/")) => {
                format!("{provider}/{model}")
            }
            _ => model.to_owned(),
        },
    )
}

/// Why a model call failed, as omp classified it: the HTTP status it kept, and its own
/// `errorId` (pi-ai `error/flags.ts`, the same in omp 18.4.12). Never read from wording.
fn failure(message: &Value, at: u64) -> Failure {
    const CLASSIFIED: u64 = 0x1000;
    const KINDS: u64 = 0xFFFF_E000;
    const AUTH_FAILED: u64 = 0x0100_0000;
    const USAGE_LIMIT: u64 = 0x8_0000;
    const CONTEXT_OVERFLOW: u64 = 0x80_0000;
    let http_status = message
        .get("errorStatus")
        .and_then(Value::as_u64)
        .filter(|status| (100..=599).contains(status))
        .and_then(|status| u16::try_from(status).ok());
    let id = message.get("errorId").and_then(Value::as_u64).unwrap_or(0);
    let classified = |bit: u64| id & (CLASSIFIED | bit) == CLASSIFIED | bit;
    // omp's own word for a sign-in it could not make: a classified id of that kind alone.
    let code =
        (id & CLASSIFIED != 0 && id & KINDS == AUTH_FAILED).then(|| "auth-failed".to_owned());
    let limited = http_status.is_some_and(|status| matches!(status, 429 | 503 | 529))
        || classified(USAGE_LIMIT);
    let kind = if classified(CONTEXT_OVERFLOW) {
        FailureKind::ContextFull
    } else if match http_status {
        Some(status) => status == 401,
        None => code.is_some(),
    } {
        FailureKind::SignIn
    } else if limited {
        FailureKind::RateLimit
    } else {
        FailureKind::Other
    };
    let said = text(message, "errorMessage").unwrap_or_default();
    Failure {
        kind,
        line: one_line(said),
        service: http_status.is_some() || limited,
        http_status,
        code,
        retry_at: retry_at(said, message, at),
        provider: text(message, "provider")
            .filter(|provider| !provider.is_empty())
            .map(str::to_owned),
        model: selector(message),
        thinking: None,
    }
}

/// When the provider takes requests again: omp ends the error it keeps with its own
/// `retry-after-ms=<ms>` once the provider said how long to wait, counted from when the
/// answer came (`completedAt`).
fn retry_at(said: &str, message: &Value, at: u64) -> Option<u64> {
    const MARK: &str = "retry-after-ms=";
    let wait = &said[said.find(MARK)? + MARK.len()..];
    let wait: u64 = wait[..wait
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(wait.len())]
        .parse()
        .ok()?;
    message
        .get("completedAt")
        .and_then(Value::as_u64)
        .unwrap_or(at)
        .checked_add(wait)
}

/// Every string a call's input carries, in order, one to a line.
fn flattened(input: &Value) -> String {
    fn walk<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
        match value {
            Value::String(said) if !said.is_empty() => out.push(said),
            Value::Array(items) => items.iter().for_each(|item| walk(item, out)),
            Value::Object(fields) => fields.values().for_each(|field| walk(field, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(input, &mut out);
    out.join("\n")
}

/// The words of a content: itself where it is a string, else its text blocks joined.
fn texts(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(said)) => said.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|block| text(block, "type") == Some("text"))
            .filter_map(|block| text(block, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// What the first `<tag>…</tag>` in `said` holds, trimmed.
fn tagged<'a>(said: &'a str, tag: &str) -> Option<&'a str> {
    let rest = said.split_once(&format!("<{tag}>"))?.1;
    Some(rest.split_once(&format!("</{tag}>"))?.0.trim())
}

/// The value of `name="…"` in a tag's opening.
fn attribute<'a>(opening: &'a str, name: &str) -> Option<&'a str> {
    let rest = opening.split_once(&format!("{name}=\""))?.1;
    rest.split_once('"').map(|(value, _)| value)
}

/// Words as one line: every run of whitespace one space.
fn one_line(said: &str) -> String {
    said.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn text<'a>(value: &'a Value, name: &str) -> Option<&'a str> {
    value.get(name).and_then(Value::as_str)
}

/// An RFC 3339 time in milliseconds since 1970.
pub fn millis(at: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}

/// omp's blob store for a transcript: `blobs` beside the `sessions` folder it is filed
/// under, else beside omp's own.
pub fn blobs(file: &Path, root: &Path) -> PathBuf {
    file.ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == "sessions"))
        .and_then(Path::parent)
        .unwrap_or(root)
        .join("blobs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcripts::tests::{Home, read};
    use serde_json::json;

    const FILED: &str = ".omp/agent/sessions/--tmp--/2026-10-01T13-00-00-000Z_f1.jsonl";
    const SESSION: &str = r#"{"type":"session","version":3,"id":"f1","cwd":"/tmp","timestamp":"2026-10-01T13:00:00Z"}"#;
    const ASKED: &str = r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-10-01T13:19:00Z","message":{"role":"user","content":[{"type":"text","text":"go on"}]}}"#;

    /// The process leaving, after the entry `parent`.
    fn exit(parent: &str) -> String {
        json!({"type": "custom", "customType": "session_exit", "id": "z", "parentId": parent,
            "timestamp": "2026-10-01T13:20:00Z"})
        .to_string()
    }

    /// A model call that failed, with the fields given, as the entry `id` under `parent`.
    fn failed(id: &str, parent: &str, fields: Value) -> String {
        let mut message = json!({"role": "assistant", "stopReason": "error", "content": [],
            "provider": "anthropic", "model": "claude-opus-5"});
        message
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        json!({"type": "message", "id": id, "parentId": parent,
            "timestamp": "2026-10-01T13:19:26.216Z", "message": message})
        .to_string()
    }

    fn failures(events: &[Event]) -> Vec<(u64, &Failure)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::TurnEnded {
                    at,
                    state: TurnState::Failed,
                    failure: Some(failure),
                    ..
                } => Some((*at, failure)),
                _ => None,
            })
            .collect()
    }

    /// The one failure a transcript says that ends on a failed call `y`, then `after` (one
    /// entry `x` at most), then the process leaving.
    fn failure(harness: Harness, fields: Value, after: Option<&str>) -> Failure {
        let home = Home::new("failure");
        let filed = match harness {
            Harness::Omp => FILED.to_owned(),
            Harness::Pi => FILED.replace(".omp", ".pi"),
        };
        let mut records = vec![
            SESSION.to_owned(),
            ASKED.to_owned(),
            failed("y", "a", fields),
        ];
        records.extend(after.map(str::to_owned));
        records.push(exit(if after.is_some() { "x" } else { "y" }));
        home.write(&filed, &records);
        let got = read(&home.store(harness, None), "f1", None, None);
        let [(_, failure)] = failures(&got.events)[..] else {
            panic!("one failed turn: {:#?}", got.events);
        };
        failure.clone()
    }

    /// A failed turn says why by the harness's own classification, never its wording: the
    /// HTTP status it kept and its `errorId`; with the provider and the model as the
    /// harness selects it, and when the provider takes requests again where it said.
    #[test]
    fn a_failed_turn_says_why_by_the_harness_own_classification() {
        assert_eq!(
            failure(
                Harness::Omp,
                json!({"errorStatus": 429, "errorMessage": "Too many requests"}),
                None
            ),
            Failure {
                kind: FailureKind::RateLimit,
                line: "Too many requests".into(),
                service: true,
                http_status: Some(429),
                code: None,
                retry_at: None,
                provider: Some("anthropic".into()),
                model: Some("anthropic/claude-opus-5".into()),
                thinking: None,
            }
        );
        let kind = |fields: Value| {
            let failure = failure(Harness::Omp, fields, None);
            (failure.kind, failure.http_status, failure.code)
        };
        assert_eq!(
            kind(json!({"errorId": 0x0100_1000, "errorMessage": "No API key for provider"})),
            (FailureKind::SignIn, None, Some("auth-failed".to_owned())),
            "a provider omp holds no key for"
        );
        assert_eq!(
            kind(json!({"errorStatus": 403, "errorId": 17_321_984})),
            (FailureKind::RateLimit, Some(403), None),
            "a spent quota carries the sign-in flag beside a usage limit"
        );
        assert_eq!(
            kind(json!({"errorId": 401})),
            (FailureKind::Other, None, None),
            "a bare number is no classification"
        );
        assert_eq!(
            kind(json!({"errorId": 0x80_1000})).0,
            FailureKind::ContextFull
        );
        let limited = failure(
            Harness::Omp,
            json!({"errorStatus": 429, "errorId": 135_168, "completedAt": 1_790_860_766_214_u64,
                "errorMessage": "429 Rate limit reached retry-after-ms=90000\nRate limit reached"}),
            None,
        );
        assert_eq!(limited.retry_at, Some(1_790_860_766_214 + 90_000));
        // pi classifies no error: Steery's plugin says once pi gave up compacting.
        let full = r#"{"type":"custom","customType":"steery-context-full","id":"x","parentId":"y","timestamp":"2026-10-01T13:19:30Z"}"#;
        assert_eq!(
            failure(
                Harness::Pi,
                json!({"errorMessage": "prompt is too long"}),
                Some(full)
            )
            .kind,
            FailureKind::ContextFull
        );
    }

    /// A turn is failed only by its final outcome. A failed call the harness retried past
    /// is no event, and while the transcript may still go on, the failure is left for the
    /// next read; retries given up, or the person's next words, end the turn as failed,
    /// when the call failed.
    #[test]
    fn a_failure_the_harness_got_past_is_no_event() {
        let home = Home::new("got-past");
        let failure = failed("y", "a", json!({"errorMessage": "stream error: timed out"}));
        let file = home.write(FILED, &[SESSION, ASKED, failure.as_str()]);
        let store = home.store(Harness::Omp, None);
        let first = read(&store, "f1", None, None);
        assert!(failures(&first.events).is_empty(), "it may still go on");
        home.append(
            &file,
            &[
                r#"{"type":"message","id":"b","parentId":"y","timestamp":"2026-10-01T13:19:36Z","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"toolCall","id":"k1","name":"write","arguments":{}}]}}"#.to_owned(),
                r#"{"type":"message","id":"c","parentId":"b","timestamp":"2026-10-01T13:19:37Z","message":{"role":"toolResult","toolCallId":"k1","content":[{"type":"text","text":"429 credit limit exceeded"}]}}"#.to_owned(),
                exit("c"),
            ],
        );
        let on = read(&store, "f1", None, Some(&first.cursor));
        assert!(failures(&on.events).is_empty(), "{:#?}", on.events);
        assert!(
            on.events
                .iter()
                .any(|event| matches!(event, Event::CallEnded { id, .. } if id == "k1")),
            "what came after the failure is read once it was decided"
        );

        let failed_at = millis("2026-10-01T13:19:26.216Z").unwrap();
        let given_up = r#"{"type":"message","id":"b","parentId":"y","timestamp":"2026-10-01T13:19:50Z","message":{"role":"assistant","stopReason":"aborted","content":[],"errorMessage":"Aborted after 3 retry attempts"}}"#;
        home.write(FILED, &[SESSION, ASKED, failure.as_str(), given_up]);
        let ended = read(&store, "f1", None, None);
        assert_eq!(
            failures(&ended.events)
                .iter()
                .map(|(at, _)| *at)
                .collect::<Vec<_>>(),
            [failed_at]
        );

        let typed = r#"{"type":"message","id":"b","parentId":"y","timestamp":"2026-10-01T13:20:00Z","message":{"role":"user","content":[{"type":"text","text":"try again"}]}}"#;
        home.write(FILED, &[SESSION, ASKED, failure.as_str(), typed]);
        let carried = read(&store, "f1", None, None).events;
        let at = carried
            .iter()
            .position(|event| matches!(event, Event::TurnEnded { .. }))
            .expect("the failed turn ended");
        assert_eq!(failures(&carried[at..=at]).len(), 1);
        assert!(matches!(carried[at + 1], Event::TurnBegan { .. }));
    }

    /// A person's words with no answer yet are a turn begun and not ended. Calls made side
    /// by side each begin and end by their own id, each when its own result says it came
    /// back, though omp writes the batch's results together once the last returned.
    #[test]
    fn calls_side_by_side_end_each_when_it_came_back() {
        let home = Home::new("side-by-side");
        let file = home.write(FILED, &[SESSION, ASKED]);
        let store = home.store(Harness::Omp, None);
        let asked = read(&store, "f1", None, None);
        assert!(matches!(asked.events[0], Event::TurnBegan { .. }));
        assert!(
            !asked
                .events
                .iter()
                .any(|event| matches!(event, Event::TurnEnded { .. }))
        );

        home.append(
            &file,
            &[
                r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-10-04T19:05:56.294Z","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"toolCall","id":"k1","name":"bash","arguments":{"command":"sleep 20"}},{"type":"toolCall","id":"k2","name":"bash","arguments":{"command":"sleep 6"}},{"type":"toolCall","id":"k3","name":"read","arguments":{"path":"runtime.sh"}}]}}"#,
                r#"{"type":"message","id":"c","parentId":"b","timestamp":"2026-10-04T19:06:16.652Z","message":{"role":"toolResult","toolCallId":"k1","toolName":"bash","timestamp":1791140776586,"content":[]}}"#,
                r#"{"type":"message","id":"d","parentId":"c","timestamp":"2026-10-04T19:06:16.701Z","message":{"role":"toolResult","toolCallId":"k2","toolName":"bash","timestamp":1791140762311,"content":[]}}"#,
                r#"{"type":"message","id":"e","parentId":"d","timestamp":"2026-10-04T19:06:16.789Z","message":{"role":"toolResult","toolCallId":"k3","toolName":"read","timestamp":1791140756309,"content":[]}}"#,
            ],
        );
        let calls = read(&store, "f1", None, Some(&asked.cursor));
        let mut began = Vec::new();
        let mut ended = Vec::new();
        for event in &calls.events {
            match event {
                Event::CallBegan { call, .. } => began.push(call.id.as_str()),
                Event::CallEnded { at, id, .. } => ended.push((id.as_str(), *at)),
                Event::TurnEnded { .. } => panic!("the turn goes on: {event:?}"),
                _ => {}
            }
        }
        assert_eq!(began, ["k1", "k2", "k3"]);
        assert_eq!(
            ended,
            [
                ("k1", 1_791_140_776_586),
                ("k2", 1_791_140_762_311),
                ("k3", 1_791_140_756_309)
            ]
        );
    }
}
