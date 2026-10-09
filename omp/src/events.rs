//! What omp's records say happened, as the protocol's events.
//!
//! A turn begins at a `user` message and ends at the assistant message that stops of its
//! own accord (`stop`), on an `error` or `aborted`. omp retries a failed model call
//! itself and writes the failed attempt first, so an error ends its turn only once the
//! conversation went no further: a person's next message, retries given up (an `aborted`
//! answer with nothing in it), the process leaving (`session_exit`), or the transcript
//! still for [`SETTLE`]. Until then the error and what follows it are read again later.

use std::path::{Path, PathBuf};

use serde_json::Value;
use steery_sdk::wire::harness::{
    Clip, Count, Event, Execution, Failure, FailureKind, TurnState, Voice,
};

use crate::calls;

/// How long a transcript that ends on a failed model call stays still before the failure
/// is the turn's end: longer than omp waits between its own retries.
pub const SETTLE: std::time::Duration = std::time::Duration::from_secs(60);

/// What reading one transcript needs besides its records.
pub struct Reading {
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

/// One whole record of a transcript, and where it lies: `None` where it is no JSON.
pub struct Record {
    pub start: u64,
    pub end: u64,
    pub value: Option<Value>,
}

/// What a run of records came to.
pub struct Read {
    pub events: Vec<Event>,
    /// The offset after the last record whose events these are.
    pub through: u64,
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
        forked,
        pending: None,
        out: Vec::new(),
    };
    let mut through = records.first().map_or(0, |record| record.start);
    for record in records {
        if let Some(value) = &record.value {
            state.record(record.start, value);
        }
        through = record.end;
    }
    match state.pending.take() {
        Some(pending) if !settled => {
            state.out.truncate(pending.mark);
            through = pending.start;
            state.forked = pending.forked;
        }
        Some(pending) => state.out.push(pending.ended()),
        None => {}
    }
    Read {
        events: state.out,
        through,
        forked: state.forked,
    }
}

/// A failed model call whose turn may still go on.
struct Pending {
    at: u64,
    failure: Failure,
    /// Where its record starts, and how many events came before it.
    start: u64,
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

struct State<'a> {
    reading: &'a Reading,
    at: u64,
    forked: bool,
    pending: Option<Pending>,
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
        match kind {
            Some("model_change") => {
                if let Some(model) = text(record, "model").filter(|model| !model.is_empty()) {
                    self.out.push(Event::Model {
                        at,
                        model: model.to_owned(),
                        provider: model
                            .split_once('/')
                            .map(|(provider, _)| provider.to_owned()),
                        thinking: None,
                    });
                }
            }
            Some("compaction" | "branch_summary") => {
                if let Some(summary) = text(record, "summary").filter(|s| !s.trim().is_empty()) {
                    self.words(at, id, Voice::Summary, summary.to_owned(), None, Vec::new());
                }
            }
            Some("custom_message") if text(record, "customType") == Some("async-result") => {
                self.results(at, record);
            }
            Some("custom") if text(record, "customType") == Some("session_exit") => {
                self.settle();
            }
            Some("message") => {
                if let Some(message) = record.get("message") {
                    self.message(start, at, id, message);
                }
            }
            _ => {}
        }
    }

    fn message(&mut self, start: u64, at: u64, id: String, message: &Value) {
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
            Some("assistant") => self.answer(start, at, id, message),
            Some("toolResult") => self.result(at, id, message),
            // developer, bashExecution, pythonExecution and the rest: nobody's words.
            _ => {}
        }
    }

    fn answer(&mut self, start: u64, at: u64, id: String, message: &Value) {
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
                call: calls::call(call, name, input, &id, &self.reading.servers),
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
        if let Some(usage) = message.get("usage") {
            self.out.push(tokens(at, &id, message, usage));
        }
        match stop {
            Some("toolUse") | None => {}
            Some("error") => {
                self.pending = Some(Pending {
                    at,
                    failure: failure(message, at),
                    start,
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

/// A message's model as omp selects it: `provider/model`.
fn selector(message: &Value) -> Option<String> {
    let model = text(message, "model")
        .or_else(|| text(message, "modelId"))?
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
