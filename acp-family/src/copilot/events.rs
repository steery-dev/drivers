//! What Copilot's `events.jsonl` says happened, as the protocol's events.
//!
//! Copilot appends one event per line, `{type, data, id, timestamp, parentId}`. A person's
//! turn is many of Copilot's `assistant.turn_start`/`assistant.turn_end` pairs, one per
//! model call: a pair whose answer asked for tools is followed by the next. The turn ends at
//! the `assistant.turn_end` after an answer that asked for none, after an `abort`
//! (interrupted), or after a `session.error` that no answer got past (failed). A session
//! that shut down or resumed with a turn open ended that turn as interrupted. The events of
//! a sub-agent are in the same file, each marked with `parentToolCallId`, the `task` call
//! that started it.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use steery_sdk::wire::harness::{
    Clip, Compactor, Count, Event, Execution, Failure, FailureKind, TurnState, Voice,
};

use super::calls;

/// How long a transcript must stand still before an end it leaves undecided is read as
/// decided: an answer whose `assistant.turn_end` is not written yet, an `abort` whose turn
/// has not closed, an error a retry may still get past.
pub const SETTLE: std::time::Duration = std::time::Duration::from_secs(60);

/// Where a turn stands between reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    /// Ended, or none began.
    Closed,
    Open,
    /// Read from the middle: whatever came before is not known.
    Unknown,
}

/// The turn as it stands between reads, and the harness's id for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Carried {
    pub turn: Turn,
    pub id: Option<String>,
}

/// What a read does with an end its records leave undecided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The transcript may still go on: the undecided records are left for the next read.
    Open,
    /// The transcript stood still for [`SETTLE`], or the batch would not move on otherwise:
    /// what was read stands.
    Decided,
}

/// How many background shells, and how many waiting calls, a transcript's [`Starts`]
/// keeps: the oldest is forgotten first.
const STARTS: usize = 256;

/// The shells that calls left running and the calls that wait on them, kept between reads
/// of one transcript.
#[derive(Clone, Default)]
pub struct Starts(Arc<Mutex<Ledger>>);

#[derive(Default)]
struct Ledger {
    /// The call that started each shell still running, by its `shellId`.
    started: Bounded,
    /// The shell each call named, by the call.
    named: Bounded,
}

#[derive(Default)]
struct Bounded {
    by: HashMap<String, String>,
    order: VecDeque<String>,
}

impl Bounded {
    fn put(&mut self, key: &str, value: &str) {
        if self.by.insert(key.to_owned(), value.to_owned()).is_none() {
            self.order.push_back(key.to_owned());
            if self.order.len() > STARTS
                && let Some(oldest) = self.order.pop_front()
            {
                self.by.remove(&oldest);
            }
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.by.get(key).map(String::as_str)
    }
}

/// What a run of records came to.
pub struct Read {
    pub events: Vec<Event>,
    pub carried: Carried,
    /// How many of the records these events are of: an undecided end and what follows it
    /// are left for the next read.
    pub consumed: usize,
}

/// The events of `records`, in order, of the conversation, or of its sub-agent `agent`,
/// read on from where `carried` says the turn stands.
pub fn read(
    agent: Option<&str>,
    starts: &Starts,
    records: &[Option<Value>],
    carried: Carried,
    end: End,
) -> Read {
    let mut state = State {
        agent,
        starts,
        at: 0,
        carried,
        pending: Pending::default(),
        model: None,
        responses: HashMap::new(),
        reasoned: HashSet::new(),
        out: Vec::new(),
    };
    let mut clean = (0, 0, state.carried.clone());
    for (index, record) in records.iter().enumerate() {
        if let Some(record) = record {
            state.record(record);
        }
        if state.pending.is_none() {
            clean = (index + 1, state.out.len(), state.carried.clone());
        }
    }
    let mut consumed = records.len();
    if !state.pending.is_none() {
        match end {
            End::Open => {
                state.out.truncate(clean.1);
                consumed = clean.0;
                state.carried = clean.2;
            }
            End::Decided => state.close(None),
        }
    }
    Read {
        events: state.out,
        carried: state.carried,
        consumed,
    }
}

/// What the stream said that a later record decides.
#[derive(Default)]
struct Pending {
    /// The agent answered without asking for a tool: its turn ends at `assistant.turn_end`.
    answered: bool,
    /// The turn was aborted.
    aborted: bool,
    /// An error no answer got past yet.
    failure: Option<Failure>,
}

impl Pending {
    fn is_none(&self) -> bool {
        !self.answered && !self.aborted && self.failure.is_none()
    }
}

struct State<'a> {
    agent: Option<&'a str>,
    starts: &'a Starts,
    at: u64,
    carried: Carried,
    pending: Pending,
    /// The model and effort last said.
    model: Option<(String, Option<String>)>,
    /// The model response each call came from, by the call.
    responses: HashMap<String, String>,
    /// The responses whose reasoning an `assistant.reasoning` said.
    reasoned: HashSet<String>,
    out: Vec<Event>,
}

impl State<'_> {
    fn record(&mut self, record: &Value) {
        if record.get("ephemeral").and_then(Value::as_bool) == Some(true) {
            return;
        }
        if let Some(at) = text(record, "timestamp").and_then(millis) {
            self.at = at;
        }
        let Some(kind) = text(record, "type") else {
            return;
        };
        let null = Value::Null;
        let data = record.get("data").unwrap_or(&null);
        let id = text(record, "id").unwrap_or_default().to_owned();
        if let Some(agent) = self.agent {
            // A sub-agent's records carry its call; its lead's records of that call begin and
            // end its turn.
            if text(data, "toolCallId") == Some(agent) && text(data, "parentToolCallId").is_none() {
                self.lead(agent, kind, id, data);
                return;
            }
            if text(data, "parentToolCallId") != Some(agent) {
                return;
            }
        } else if text(data, "parentToolCallId").is_some() {
            return;
        }
        match kind {
            "session.start" | "session.resume" => {
                // A resume that joined the live session ended nothing.
                if kind == "session.resume"
                    && data.get("sessionWasActive").and_then(Value::as_bool) != Some(true)
                {
                    self.close(Some(TurnState::Interrupted));
                }
                if let Some(model) = text(data, "selectedModel") {
                    self.model(model, text(data, "reasoningEffort"));
                }
            }
            "session.shutdown" => self.close(Some(TurnState::Interrupted)),
            "session.model_change" => {
                if let Some(model) = text(data, "newModel") {
                    self.model(model, text(data, "reasoningEffort"));
                }
            }
            "session.usage_record" => self.tokens(data.get("usage").unwrap_or(&null)),
            "session.error" => self.error(data),
            "session.compaction_complete" => self.compacted(id, data),
            "abort" => {
                if self.carried.turn != Turn::Closed {
                    self.pending.aborted = true;
                }
            }
            "user.message" => self.person(id, data),
            "assistant.turn_start" if self.agent.is_none() => match self.carried.turn {
                Turn::Closed => self.begin(text(data, "interactionId")),
                // Read from the middle: only a person's first model call begins a turn.
                Turn::Unknown if text(data, "turnId") == Some("0") => {
                    self.begin(text(data, "interactionId"))
                }
                Turn::Unknown => self.carried.turn = Turn::Open,
                Turn::Open => {}
            },
            "assistant.turn_end" => {
                if !self.pending.is_none() {
                    self.close(None);
                }
            }
            "assistant.reasoning" => {
                if let Some(response) = text(data, "apiCallId") {
                    self.reasoned.insert(response.to_owned());
                }
                let said = text(data, "content").unwrap_or_default();
                self.words(id, Voice::Thought, said, None, None, Vec::new());
            }
            "assistant.message" => self.answer(id, data),
            "tool.execution_start" => self.began(data),
            "tool.execution_complete" => self.ended(id, data),
            "subagent.started" => self.sub_agent(data, Execution::Running, None),
            "subagent.completed" => {
                let state = if data.get("cancelled").and_then(Value::as_bool) == Some(true) {
                    Execution::Interrupted
                } else {
                    Execution::Finished
                };
                self.sub_agent(data, state, None);
            }
            "subagent.failed" => {
                let error = text(data, "error").map(one_line);
                self.sub_agent(data, Execution::Failed, error);
            }
            _ => {}
        }
    }

    /// The lead's records of the call that started the sub-agent `agent`: the call's prompt
    /// begins its turn, and `subagent.completed` or `subagent.failed` ends it.
    fn lead(&mut self, agent: &str, kind: &str, id: String, data: &Value) {
        match kind {
            "tool.execution_start" => {
                if self.carried.turn != Turn::Open {
                    self.begin(Some(agent));
                }
                let prompt = data
                    .pointer("/arguments/prompt")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.words(id, Voice::Person, prompt, None, None, Vec::new());
            }
            "subagent.started" if self.carried.turn != Turn::Open => self.begin(Some(agent)),
            "subagent.completed" => {
                let cancelled = data.get("cancelled").and_then(Value::as_bool) == Some(true);
                self.pending = Pending::default();
                self.close(Some(if cancelled {
                    TurnState::Interrupted
                } else {
                    TurnState::Finished
                }));
            }
            "subagent.failed" => {
                self.pending = Pending::default();
                self.pending.failure = Some(Failure {
                    kind: FailureKind::Other,
                    line: text(data, "error").map(one_line).unwrap_or_default(),
                    service: false,
                    http_status: None,
                    code: None,
                    retry_at: None,
                    provider: None,
                    model: None,
                    thinking: None,
                });
                self.close(None);
            }
            _ => {}
        }
    }

    fn begin(&mut self, id: Option<&str>) {
        self.carried = Carried {
            turn: Turn::Open,
            id: id.map(str::to_owned),
        };
        self.out.push(Event::TurnBegan {
            at: self.at,
            turn: self.carried.id.clone(),
        });
    }

    /// Ends the open turn: as `state` where given, else as what is pending decides.
    fn close(&mut self, state: Option<TurnState>) {
        let pending = std::mem::take(&mut self.pending);
        if self.carried.turn == Turn::Closed {
            return;
        }
        let (state, failure) = if pending.aborted {
            (TurnState::Interrupted, None)
        } else if let Some(failure) = pending.failure {
            (TurnState::Failed, Some(failure))
        } else if pending.answered {
            (TurnState::Finished, None)
        } else {
            match state {
                Some(state) => (state, None),
                None => return,
            }
        };
        self.out.push(Event::TurnEnded {
            at: self.at,
            turn: self.carried.id.take(),
            state,
            failure,
        });
        self.carried = Carried {
            turn: Turn::Closed,
            id: None,
        };
    }

    /// `user.message`: a person's words, unless Copilot wrote it itself (autopilot's
    /// continuation, a skill's injected prompt, another agent's message). Words delivered
    /// while a turn runs steer it; any other begins one.
    fn person(&mut self, id: String, data: &Value) {
        let steering = matches!(text(data, "delivery"), Some("steering" | "queued"));
        if self.carried.turn == Turn::Open && !steering {
            self.close(Some(TurnState::Interrupted));
        }
        if self.carried.turn != Turn::Open && !(self.carried.turn == Turn::Unknown && steering) {
            self.begin(text(data, "interactionId"));
        }
        if self.agent.is_some() {
            return;
        }
        let injected = data.get("isAutopilotContinuation").and_then(Value::as_bool) == Some(true)
            || text(data, "source").is_some_and(|source| source != "user")
            || data
                .get("causalDependencies")
                .and_then(Value::as_array)
                .is_some_and(|ids| !ids.is_empty());
        if injected {
            return;
        }
        let clips = data
            .get("attachments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(clip)
            .collect();
        let said = text(data, "content").unwrap_or_default();
        let message = text(data, "messageId").map(str::to_owned);
        self.words(id, Voice::Person, said, message, None, clips);
    }

    /// `assistant.message`: one answer, its reasoning, and the calls it asked for.
    fn answer(&mut self, id: String, data: &Value) {
        let response = text(data, "apiCallId");
        let requests = data
            .get("toolRequests")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if let Some(response) = response {
            for request in requests {
                if let Some(call) = text(request, "toolCallId") {
                    self.responses.insert(call.to_owned(), response.to_owned());
                }
            }
        }
        let message = text(data, "messageId").map(str::to_owned);
        if let Some(reasoning) = text(data, "reasoningText")
            && !reasoning.trim().is_empty()
            && !response.is_some_and(|response| self.reasoned.contains(response))
        {
            self.words(
                format!("{id}:thought"),
                Voice::Thought,
                reasoning,
                message.clone(),
                None,
                Vec::new(),
            );
        }
        let said = text(data, "content").unwrap_or_default();
        if !said.trim().is_empty() {
            self.words(id, Voice::Agent, said, message, None, Vec::new());
        }
        // An answer gets past an error; one that asks for no tool ends its turn.
        self.pending.failure = None;
        self.pending.answered =
            self.agent.is_none() && requests.is_empty() && self.carried.turn != Turn::Closed;
    }

    fn began(&mut self, data: &Value) {
        let Some(id) = text(data, "toolCallId") else {
            return;
        };
        let input = data.get("arguments").cloned().unwrap_or(Value::Null);
        let shell = text(&input, "shellId").map(str::to_owned);
        let mut call = match (text(data, "mcpServerName"), text(data, "mcpToolName")) {
            (Some(server), Some(tool)) => calls::mcp(id, server, tool, input),
            _ => calls::call(id, text(data, "toolName").unwrap_or_default(), input),
        };
        call.parent = text(data, "parentToolCallId").map(str::to_owned);
        call.response = self.responses.get(id).cloned();
        if let Some(shell) = shell {
            lock(self.starts).named.put(id, &shell);
        }
        self.out.push(Event::CallBegan { at: self.at, call });
    }

    /// `tool.execution_complete`: the call's end, and its result as words. A shell started
    /// in the background answers with its `shellId` before it completed; a later call that
    /// names that shell waited on it.
    fn ended(&mut self, id: String, data: &Value) {
        let Some(call) = text(data, "toolCallId") else {
            return;
        };
        let ok = data.get("success").and_then(Value::as_bool) == Some(true);
        let said = if ok {
            data.pointer("/result/content").and_then(Value::as_str)
        } else {
            data.pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| data.pointer("/result/content").and_then(Value::as_str))
        }
        .unwrap_or_default()
        .trim()
        .to_owned();
        let waited = {
            let mut ledger = lock(self.starts);
            if ok
                && let Some(shell) = data
                    .pointer("/result/structuredContent/shellId")
                    .and_then(Value::as_str)
                && !said.contains("completed with exit code")
            {
                ledger.started.put(shell, call);
            }
            ledger
                .named
                .get(call)
                .and_then(|shell| ledger.started.get(shell))
                .filter(|started| *started != call)
                .map(str::to_owned)
                .into_iter()
                .collect()
        };
        let lines: Vec<&str> = said.lines().collect();
        let tail = lines[lines.len().saturating_sub(8)..].join("\n");
        self.out.push(Event::CallEnded {
            at: self.at,
            id: call.to_owned(),
            ok,
            chars: said.chars().count() as u64,
            tail: (!tail.trim().is_empty()).then_some(tail),
            waited,
        });
        if !said.is_empty() {
            self.words(
                id,
                Voice::Tool,
                &said,
                None,
                Some(call.to_owned()),
                Vec::new(),
            );
        }
    }

    fn sub_agent(&mut self, data: &Value, state: Execution, error: Option<String>) {
        let Some(agent) = text(data, "toolCallId") else {
            return;
        };
        self.out.push(Event::SubAgent {
            at: self.at,
            external_id: agent.to_owned(),
            call: Some(agent.to_owned()),
            state: Some(state),
            error,
        });
    }

    /// `session.error`: a failure that ends the turn unless an answer gets past it.
    fn error(&mut self, data: &Value) {
        if self.carried.turn == Turn::Closed {
            return;
        }
        let kind = match text(data, "errorType").unwrap_or_default() {
            "authentication" | "authorization" => FailureKind::SignIn,
            "rate_limit" | "quota" => FailureKind::RateLimit,
            kind if kind.contains("context") => FailureKind::ContextFull,
            _ => FailureKind::Other,
        };
        let http_status = data
            .get("statusCode")
            .and_then(Value::as_u64)
            .and_then(|status| u16::try_from(status).ok());
        self.pending.failure = Some(Failure {
            kind,
            line: text(data, "message").map(one_line).unwrap_or_default(),
            service: http_status.is_some(),
            http_status,
            code: text(data, "errorCode").map(str::to_owned),
            retry_at: None,
            provider: None,
            model: self.model.as_ref().map(|(model, _)| model.clone()),
            thinking: None,
        });
    }

    /// `session.compaction_complete`: `trigger` `manual` where its person ran `/compact`.
    fn compacted(&mut self, id: String, data: &Value) {
        if data.get("success").and_then(Value::as_bool) != Some(true) {
            return;
        }
        let by = match text(data, "trigger") {
            Some("manual") => Compactor::Person,
            Some("threshold") => Compactor::Harness {
                reason: Some("threshold".into()),
            },
            Some("context_limit_retry") => Compactor::Harness {
                reason: Some("overflow".into()),
            },
            other => Compactor::Harness {
                reason: other.map(str::to_owned),
            },
        };
        self.out.push(Event::Compacted {
            at: self.at,
            id: id.clone(),
            by,
            tokens_before: data.get("preCompactionTokens").and_then(Value::as_u64),
            tokens_after: data.get("postCompactionTokens").and_then(Value::as_u64),
        });
        if let Some(summary) = text(data, "summaryContent")
            && !summary.trim().is_empty()
        {
            self.words(
                format!("{id}:summary"),
                Voice::Summary,
                summary,
                None,
                None,
                Vec::new(),
            );
        }
    }

    /// `session.usage_record`: one model call's count. Copilot's `inputTokens` holds its
    /// cache reads and writes.
    fn tokens(&mut self, usage: &Value) {
        let number = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
        self.out.push(Event::Tokens {
            at: self.at,
            count: Count::Response,
            response: text(usage, "apiCallId").map(str::to_owned),
            model: text(usage, "model").map(str::to_owned),
            input: number("inputTokens"),
            cached: number("cacheReadTokens"),
            cache_write: usage.get("cacheWriteTokens").and_then(Value::as_u64),
            output: number("outputTokens"),
            reasoning: number("reasoningTokens"),
            context_left: None,
        });
    }

    fn model(&mut self, model: &str, thinking: Option<&str>) {
        let said = (model.to_owned(), thinking.map(str::to_owned));
        if self.model.as_ref() == Some(&said) {
            return;
        }
        self.out.push(Event::Model {
            at: self.at,
            model: said.0.clone(),
            provider: None,
            thinking: said.1.clone(),
        });
        self.model = Some(said);
    }

    fn words(
        &mut self,
        id: String,
        voice: Voice,
        said: &str,
        message: Option<String>,
        call: Option<String>,
        clips: Vec<Clip>,
    ) {
        if said.trim().is_empty() && clips.is_empty() {
            return;
        }
        self.out.push(Event::Words {
            at: self.at,
            id,
            voice,
            text: said.to_owned(),
            message,
            call,
            clips,
        });
    }
}

/// One attachment of a person's message: a file or folder they named, or bytes.
fn clip(attachment: &Value) -> Option<Clip> {
    let name = text(attachment, "displayName").map(str::to_owned);
    match text(attachment, "type")? {
        "file" | "directory" => Some(Clip::Path {
            path: text(attachment, "path")?.into(),
            name,
        }),
        "blob" => Some(Clip::Bytes {
            data: text(attachment, "data")?.to_owned(),
            media_type: text(attachment, "mimeType").map(str::to_owned),
            name,
        }),
        _ => None,
    }
}

fn lock(starts: &Starts) -> std::sync::MutexGuard<'_, Ledger> {
    starts.0.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Words as one line: every run of whitespace one space.
pub fn one_line(said: &str) -> String {
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
