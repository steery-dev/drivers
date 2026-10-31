//! What Claude Code's records say happened, as the protocol's events.
//!
//! Claude Code appends one record per line: `user` records carry what went into the model
//! (a person's words, a call's result, the harness's own notes marked `isMeta`, a
//! compaction's summary marked `isCompactSummary`), `assistant` records carry one content
//! block of a response each, every one repeating the response's `message.id` and its
//! `usage`, and `system` records carry the harness's bookkeeping. A sub-agent's records are
//! marked `isSidechain`: in its own transcript they are its own, and in its lead's (where
//! older releases inlined them) they are none of the lead's. Words that arrive while a turn
//! runs — a person typing, another session's message through the inbox — are an
//! `attachment` record instead ([`queued`]), and the turn goes on with them.
//!
//! A turn begins at what the agent is given to work on: a person's message, a command, a
//! notice from a background task. It ends at the agent's last answer of it, which stops for
//! any reason but a call (`end_turn`, `stop_sequence`, `max_tokens`, `refusal`), at the
//! `system` `turn_duration` record newer releases write once it is over, at Claude Code's
//! answer to a local command (`<local-command-stdout>`), at `[Request interrupted by
//! user…]`, or at the API error Claude Code writes once it gave up retrying
//! (`isApiErrorMessage`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;
use steery_sdk::wire::harness::{
    Clip, Compactor, Count, Event, Execution, Failure, FailureKind, TurnState, Voice,
};

use crate::calls;
use crate::context::Window;

/// What Claude Code names what it writes itself — an API error, a filler reply — in place
/// of a model.
pub(crate) const SYNTHETIC: &str = "<synthetic>";

/// What reading one transcript needs besides its records.
pub struct Reading {
    /// It is a sub-agent's own transcript, whose `isSidechain` records are its own.
    pub own: bool,
    /// The folder the conversation's sub-agents are filed in, `<conversation>/subagents`.
    pub agents: PathBuf,
    /// The context window a count is measured against.
    pub window: Window,
    /// Where the conversation is a fork: its parent, and the uuid of its own first record,
    /// before which every record is a copy of the parent's.
    pub fork: Option<(String, String)>,
}

/// One whole record of a transcript, and where it starts: `None` where it is no JSON.
pub use steery_driver_common::tree::Record;

/// How long a transcript must stand still before an error Claude Code wrote is read as a
/// turn's end: until then it may be one it gets past, and a retry writes the answer after
/// it (Claude Code 2.1.237 wrote `API Error: The socket connection was closed
/// unexpectedly.` mid-response and went on with the same turn).
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

/// What a read does with an error its records leave undecided at their end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The transcript may still go on: an error Claude Code may get past is left, with
    /// everything after it, for the next read.
    Open,
    /// The transcript stood still for [`SETTLE`]: an error still undecided is its turn's end.
    Still,
    /// What was read stands: a batch that held back everything it read would never move on.
    Forced,
}

/// What a run of records came to.
pub struct Read {
    pub events: Vec<Event>,
    /// Where the turn stands after them.
    pub turn: Turn,
    /// How many of the records these events are of: an error still undecided and what
    /// follows it are left for the next read.
    pub consumed: usize,
}

/// The events of `records`, the records of the conversation's path in order, read on from
/// where `turn` says the turn stands. `end` says what becomes of an error the records
/// leave undecided at their end.
pub fn read(reading: &Reading, records: &[Record], turn: Turn, end: End) -> Read {
    let mut state = State {
        reading,
        at: 0,
        index: 0,
        turn,
        model: None,
        tokens: None,
        running: HashSet::new(),
        started: None,
        handbacks: HashSet::new(),
        pending: None,
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
        Some(pending) if end == End::Open => {
            state.out.truncate(pending.held.mark);
            consumed = pending.held.index;
            state.turn = pending.held.turn;
        }
        Some(pending) => {
            state.out.push(pending.ended());
            state.turn = Turn::Closed;
        }
        None => {}
    }
    Read {
        events: state.out,
        turn: state.turn,
        consumed,
    }
}

/// A record left for the next read with everything after it: which it is, how many events
/// came before it, and where the turn stood.
struct Held {
    index: usize,
    mark: usize,
    turn: Turn,
}

/// An error Claude Code wrote whose turn may still go on.
struct Pending {
    at: u64,
    failure: Failure,
    held: Held,
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

/// What a record off the conversation's path still spent: a response on a branch a rewind
/// left behind is no part of the conversation's history, and its tokens were spent all the
/// same. Named by its response's id, as on the path, so a response said twice counts once.
pub fn spent(record: &Value) -> Option<Event> {
    let message = record
        .get("message")
        .filter(|_| text(record, "type") == Some("assistant"))?;
    let model = text(message, "model").map(str::trim);
    if model == Some(SYNTHETIC) || flag(record, "isApiErrorMessage") {
        return None;
    }
    let at = text(record, "timestamp").and_then(millis).unwrap_or(0);
    Some(tokens(
        at,
        text(message, "id")?.to_owned(),
        model.filter(|model| !model.is_empty()),
        message.get("usage")?,
        None,
    ))
}

/// The response a record is one block of: an assistant record's `message.id`. Claude Code
/// writes a response one content block a record, each the parent of the next.
pub fn response(record: &Value) -> Option<&str> {
    record
        .get("message")
        .filter(|_| text(record, "type") == Some("assistant"))
        .and_then(|message| text(message, "id"))
}

/// Whether a record carries calls' results: a user record of `tool_result` blocks.
pub fn returns(record: &Value) -> bool {
    text(record, "type") == Some("user")
        && record.pointer("/message/content").is_some_and(|content| {
            blocks(content)
                .iter()
                .any(|block| text(block, "type") == Some("tool_result"))
        })
}

/// One response's `usage`: what it read fresh, wrote to a cache and read from one are all
/// of `input`; its thinking is of `output`. `context_left` is the share of the window left
/// after it, where it is on the conversation's path.
fn tokens(
    at: u64,
    response: String,
    model: Option<&str>,
    usage: &Value,
    context_left: Option<f64>,
) -> Event {
    let number = |value: Option<&Value>| value.and_then(Value::as_u64).unwrap_or(0);
    let cached = number(usage.get("cache_read_input_tokens"));
    let cache_write = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    Event::Tokens {
        at,
        count: Count::Response,
        response: Some(response),
        model: model.map(str::to_owned),
        input: number(usage.get("input_tokens")) + cache_write.unwrap_or(0) + cached,
        cached,
        cache_write,
        output: number(usage.get("output_tokens")),
        reasoning: number(usage.pointer("/output_tokens_details/thinking_tokens")),
        context_left,
    }
}

struct State<'a> {
    reading: &'a Reading,
    at: u64,
    /// Which record is being read.
    index: usize,
    turn: Turn,
    /// The model and effort last said.
    model: Option<(String, Option<String>)>,
    /// The response and `usage` last said, so a response its next block repeats unchanged
    /// is said once.
    tokens: Option<(String, Value)>,
    /// The sub-agents said running.
    running: HashSet<String>,
    /// The sub-agent each call started, by the `.meta.json` Claude Code files beside each:
    /// read once, where a call needs it.
    started: Option<HashMap<String, String>>,
    /// The calls that handed a sub-agent's report back to its lead ([`HANDBACK`]), whose
    /// result ends the sub-agent's own turn.
    handbacks: HashSet<String>,
    /// An error Claude Code wrote that its turn may still go on past.
    pending: Option<Pending>,
    out: Vec<Event>,
}

impl State<'_> {
    fn record(&mut self, start: u64, record: &Value) {
        if let Some(at) = text(record, "timestamp").and_then(millis) {
            self.at = at;
        }
        if !self.reading.own && flag(record, "isSidechain") {
            return;
        }
        let id = text(record, "uuid").map_or_else(|| format!("@{start}"), str::to_owned);
        if let Some((parent, own)) = &self.reading.fork
            && id == *own
        {
            self.out.push(Event::Fork {
                at: self.at,
                parent: parent.clone(),
            });
        }
        match text(record, "type") {
            Some("user") => self.user(id, record),
            Some("assistant") => self.assistant(id, record),
            Some("attachment") => self.attachment(id, record),
            Some("system") if text(record, "subtype") == Some("turn_duration") => {
                self.end(TurnState::Finished, None);
            }
            Some("system") if text(record, "subtype") == Some("compact_boundary") => {
                self.compacted(id, record.get("compactMetadata"));
            }
            _ => {}
        }
    }

    /// Claude Code's `compact_boundary`: `trigger` `manual` where its person ran `/compact`,
    /// `auto` where the context reached its limit, and the context's tokens before and
    /// after (`preTokens`, `postTokens`).
    fn compacted(&mut self, id: String, metadata: Option<&Value>) {
        let tokens = |name: &str| metadata?.get(name)?.as_u64();
        let by = match metadata.and_then(|metadata| text(metadata, "trigger")) {
            Some("manual") => Compactor::Person,
            Some("auto") => Compactor::Harness {
                reason: Some("threshold".into()),
            },
            _ => Compactor::Harness { reason: None },
        };
        self.out.push(Event::Compacted {
            at: self.at,
            id,
            by,
            tokens_before: tokens("preTokens"),
            tokens_after: tokens("postTokens"),
        });
    }

    /// An `attachment` record: the words Claude Code took in while a turn ran
    /// ([`queued`]), or its own notice that a sub-agent running in the background stopped
    /// ([`notification`]), which is nobody's words and begins no turn.
    fn attachment(&mut self, id: String, record: &Value) {
        if let Some(notice) = notification(record) {
            self.notified(notice);
            return;
        }
        if let Some(queued) = queued(record)
            && let Some((said, clips)) = person(queued, queued.get("prompt"))
        {
            self.words(id, Voice::Person, said, None, clips);
        }
    }

    fn user(&mut self, id: String, record: &Value) {
        let Some(content) = record.pointer("/message/content") else {
            return;
        };
        let results: Vec<&Value> = blocks(content)
            .iter()
            .filter(|block| text(block, "type") == Some("tool_result"))
            .collect();
        if !results.is_empty() {
            for block in results {
                self.result(&id, record, block);
            }
            return;
        }
        let said = texts(content);
        if interrupted(&said) {
            self.end(TurnState::Interrupted, None);
            return;
        }
        if flag(record, "isCompactSummary") {
            self.words(id, Voice::Summary, said.trim().to_owned(), None, Vec::new());
            return;
        }
        // The harness talking to itself, and the Claude Agent SDK echoing a message the
        // person already sent. Words another session sent through a live session's inbox
        // are marked `isMeta` too, with a `peer` origin, and are the agent's to work on.
        let peer = record.pointer("/origin/kind").and_then(Value::as_str) == Some("peer");
        if (flag(record, "isMeta") && !peer)
            || flag(record, "isSynthetic")
            || flag(record, "isReplay")
        {
            return;
        }
        let trimmed = said.trim_start();
        if trimmed.starts_with("<local-command-stdout>")
            || trimmed.starts_with("<local-command-stderr>")
        {
            self.end(TurnState::Finished, None);
            return;
        }
        self.notified(&said);
        self.begin();
        if let Some((said, clips)) = person(record, record.pointer("/message/content")) {
            self.words(id, Voice::Person, said, None, clips);
        }
    }

    fn assistant(&mut self, id: String, record: &Value) {
        let Some(message) = record.get("message") else {
            return;
        };
        if flag(record, "isApiErrorMessage") {
            // Held back: Claude Code writes the error it may still get past, and a retry's
            // answer follows it in the same turn. Only a transcript that stood still, or a
            // batch that would otherwise never move on, makes it the turn's end
            // ([`read`]).
            self.pending = Some(Pending {
                at: self.at,
                failure: failure(record, message, self.at),
                held: Held {
                    index: self.index,
                    mark: self.out.len(),
                    turn: self.turn,
                },
            });
            return;
        }
        let model = text(message, "model").map(str::trim);
        if model == Some(SYNTHETIC) {
            return;
        }
        let model = model.filter(|model| !model.is_empty());
        let at = self.at;
        if let Some(model) = model {
            let said = (
                model.to_owned(),
                text(record, "effort")
                    .filter(|effort| !effort.is_empty())
                    .map(str::to_owned),
            );
            if self.model.as_ref() != Some(&said) {
                self.out.push(Event::Model {
                    at,
                    model: said.0.clone(),
                    provider: None,
                    thinking: said.1.clone(),
                });
                self.model = Some(said);
            }
        }
        let response = text(message, "id").map(str::to_owned);
        let content = blocks(message.get("content").unwrap_or(&Value::Null));
        let joined = |kind: &str, field: &str| -> String {
            content
                .iter()
                .filter(|block| text(block, "type") == Some(kind))
                .filter_map(|block| text(block, field))
                .filter(|said| !said.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let thought = joined("thinking", "thinking");
        let said = joined("text", "text");
        // The model answered again: an error before it is one Claude Code got past.
        if !thought.is_empty()
            || !said.is_empty()
            || content
                .iter()
                .any(|block| text(block, "type") == Some("tool_use"))
        {
            self.pending = None;
        }
        if !thought.is_empty() {
            self.words(
                format!("{id}:thought"),
                Voice::Thought,
                thought,
                None,
                Vec::new(),
            );
        }
        if !said.is_empty() {
            self.words(id.clone(), Voice::Agent, said, None, Vec::new());
        }
        for block in content
            .iter()
            .filter(|block| text(block, "type") == Some("tool_use"))
        {
            let (Some(call), Some(name)) = (text(block, "id"), text(block, "name")) else {
                continue;
            };
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            let mut began = calls::call(call, name, input);
            began.response = response.clone();
            self.out.push(Event::CallBegan { at, call: began });
            if AGENTS.contains(&name)
                && let Some(agent) = self.started_by(call)
            {
                self.sub_agent(agent, Some(call.to_owned()), Execution::Running, None);
            }
            if self.reading.own && name == HANDBACK {
                self.handbacks.insert(call.to_owned());
            }
        }
        if let (Some(response), Some(usage)) = (response, message.get("usage")) {
            let left = model.map(|model| {
                self.reading.window.left(
                    model,
                    text(record, "requestedModel"),
                    crate::context::used(usage),
                )
            });
            self.tokens(response, model, usage, left);
        }
        if text(message, "stop_reason").is_some_and(|reason| ENDS.contains(&reason)) {
            self.end(TurnState::Finished, None);
        }
    }

    /// One call's result: its text, its last lines, and where the call started a
    /// sub-agent, how that sub-agent's run came out as Claude Code answered the call: its
    /// `toolUseResult` `completed`, `async_launched` for one that went on in the
    /// background, or an error, which `[Request interrupted…]` says the person caused.
    fn result(&mut self, id: &str, record: &Value, block: &Value) {
        let Some(call) = text(block, "tool_use_id") else {
            return;
        };
        let at = self.at;
        let said = without_reminders(&flatten(block.get("content")))
            .trim()
            .to_owned();
        let failed = flag(block, "is_error");
        let lines: Vec<&str> = said.lines().collect();
        let tail = lines[lines.len().saturating_sub(8)..].join("\n");
        self.out.push(Event::CallEnded {
            at,
            id: call.to_owned(),
            ok: !failed,
            chars: said.chars().count() as u64,
            tail: (!tail.trim().is_empty()).then_some(tail),
        });
        let result = record.get("toolUseResult");
        let answered = if failed {
            Some(if interrupted(&said) {
                (Execution::Interrupted, None)
            } else {
                (
                    Execution::Failed,
                    Some(one_line(&said)).filter(|why| !why.is_empty()),
                )
            })
        } else {
            match result.and_then(|result| text(result, "status")) {
                Some("completed") => Some((Execution::Finished, None)),
                Some("async_launched") => Some((Execution::Running, None)),
                _ => None,
            }
        };
        if let Some((state, error)) = answered {
            let agent = result
                .and_then(|result| text(result, "agentId"))
                .map(str::to_owned)
                .or_else(|| self.started_by(call));
            if let Some(agent) = agent
                && !(state == Execution::Running && self.running.contains(&agent))
            {
                self.sub_agent(agent, Some(call.to_owned()), state, error);
            }
        }
        let handed = self
            .handbacks
            .contains(call)
            .then(|| (one_line(&said), failed || refused(result)));
        if !said.is_empty() {
            self.words(
                format!("{id}:{call}"),
                Voice::Tool,
                said,
                Some(call.to_owned()),
                Vec::new(),
            );
        }
        if let Some((said, failed)) = handed {
            self.handed(&said, failed);
        }
    }

    /// A `<task-notification>` Claude Code posts each time a sub-agent running in the
    /// background stops, naming it as `task-id` and the call that started it as
    /// `tool-use-id`: `completed`, `failed` with why in its summary, `killed` when its lead
    /// stopped it, `stopped` when Claude Code found no record of how it ended. A background
    /// shell's notice names no sub-agent filed here, and says nothing of one.
    fn notified(&mut self, said: &str) {
        let Some((_, notice)) = said.split_once("<task-notification>") else {
            return;
        };
        let state = match tagged(notice, "status") {
            Some("completed") => Execution::Finished,
            Some("failed") => Execution::Failed,
            Some("killed" | "stopped") => Execution::Interrupted,
            _ => return,
        };
        let Some(agent) = tagged(notice, "task-id").filter(|agent| self.filed(agent)) else {
            return;
        };
        let error = tagged(notice, "summary")
            .filter(|_| state == Execution::Failed)
            .map(|summary| {
                one_line(
                    summary
                        .split_once(" failed: ")
                        .map_or(summary, |(_, why)| why),
                )
            });
        let call = tagged(notice, "tool-use-id").map(str::to_owned);
        self.sub_agent(agent.to_owned(), call, state, error);
    }

    fn sub_agent(
        &mut self,
        external_id: String,
        call: Option<String>,
        state: Execution,
        error: Option<String>,
    ) {
        if state == Execution::Running {
            self.running.insert(external_id.clone());
        } else {
            self.running.remove(&external_id);
        }
        self.out.push(Event::SubAgent {
            at: self.at,
            external_id,
            call,
            state: Some(state),
            error,
        });
    }

    /// Whether a sub-agent of this id is filed with this conversation's.
    fn filed(&self, agent: &str) -> bool {
        agent_id(agent) && agent_file(&self.reading.agents, agent).is_file()
    }

    /// The sub-agent `call` started, as the `.meta.json` beside its transcript names the
    /// call (`toolUseId`).
    fn started_by(&mut self, call: &str) -> Option<String> {
        self.started
            .get_or_insert_with(|| started(&self.reading.agents))
            .get(call)
            .cloned()
    }

    /// One response's count, said once however many of its blocks repeat it unchanged.
    fn tokens(&mut self, response: String, model: Option<&str>, usage: &Value, left: Option<f64>) {
        let said = (response, usage.clone());
        if self.tokens.as_ref() == Some(&said) {
            return;
        }
        self.out.push(tokens(self.at, said.0.clone(), model, usage, left));
        self.tokens = Some(said);
    }

    fn begin(&mut self) {
        if self.turn != Turn::Open {
            self.out.push(Event::TurnBegan {
                at: self.at,
                turn: None,
            });
            self.turn = Turn::Open;
        }
    }

    /// Ends the turn, where one is open. An error Claude Code wrote and did not get past
    /// is what ended it, whatever the record that says the turn is over
    /// (`turn_duration`, a stop of its person's): a turn is failed by its final outcome.
    fn end(&mut self, state: TurnState, failure: Option<Failure>) {
        let (state, failure) = match self.pending.take() {
            Some(pending) => (TurnState::Failed, Some(pending.failure)),
            None => (state, failure),
        };
        if self.turn != Turn::Closed {
            self.out.push(Event::TurnEnded {
                at: self.at,
                turn: None,
                state,
                failure,
            });
            self.turn = Turn::Closed;
        }
    }

    /// A sub-agent handed its report to its lead ([`HANDBACK`]): its own turn is over,
    /// failed where Claude Code refused the hand-back, in its words.
    fn handed(&mut self, said: &str, failed: bool) {
        let failure = failed.then(|| Failure {
            kind: FailureKind::Other,
            line: one_line(said),
            service: false,
            http_status: None,
            code: None,
            retry_at: None,
            provider: None,
            model: None,
            thinking: None,
        });
        match failure {
            Some(failure) => self.end(TurnState::Failed, Some(failure)),
            None => self.end(TurnState::Finished, None),
        }
    }

    fn words(
        &mut self,
        id: String,
        voice: Voice,
        text: String,
        call: Option<String>,
        clips: Vec<Clip>,
    ) {
        if text.is_empty() && clips.is_empty() {
            return;
        }
        self.out.push(Event::Words {
            at: self.at,
            id,
            voice,
            text,
            call,
            clips,
        });
    }
}

/// The tools that start a sub-agent: `Task`, renamed `Agent` in later releases.
const AGENTS: [&str; 2] = ["Task", "Agent"];

/// The `stop_reason`s of a response that ends its turn: any but a call.
pub(crate) const ENDS: [&str; 4] = ["end_turn", "stop_sequence", "max_tokens", "refusal"];

/// The tool a sub-agent of Claude Code 2.1.316 hands its report back to its lead with: it
/// ends the sub-agent's own turn, which writes no answer of its own after it.
const HANDBACK: &str = "SubagentHandback";

/// Whether Claude Code refused a hand-back: its answer says it did not take the report
/// (`toolUseResult` `success` false).
fn refused(result: Option<&Value>) -> bool {
    result.and_then(|result| result.get("success")?.as_bool()) == Some(false)
}

/// Why a turn failed, as Claude Code recorded it: a context over the model's limit
/// ([`context_full`]), else by the API's status (`apiErrorStatus`) and its own code
/// (`error`) ([`failure_kind`]), its words in the message. Every such failure
/// is the model service's. A limit's end is the quota Claude Code says refused it
/// (`quotaLimits`, `status` `rejected`): its `resetsAt`, in seconds; else the wait it
/// named in its own words ([`retry_in`]), from `at`.
fn failure(record: &Value, message: &Value, at: u64) -> Failure {
    let http_status = record
        .get("apiErrorStatus")
        .and_then(Value::as_u64)
        .filter(|status| (100..=599).contains(status))
        .and_then(|status| u16::try_from(status).ok());
    let code = text(record, "error")
        .filter(|code| !code.is_empty())
        .map(str::to_owned);
    let line = blocks(message.get("content").unwrap_or(&Value::Null))
        .iter()
        .find_map(|block| text(block, "text"))
        .map(one_line)
        .unwrap_or_default();
    let kind = if context_full(text(record, "apiError"), &line) {
        FailureKind::ContextFull
    } else {
        failure_kind(http_status, code.as_deref())
    };
    let limited = kind == FailureKind::RateLimit;
    let quota = || {
        record
            .get("quotaLimits")
            .filter(|quota| text(quota, "status") == Some("rejected"))
            .and_then(|quota| quota.get("resetsAt"))
            .and_then(Value::as_u64)
            .and_then(|seconds| seconds.checked_mul(1000))
    };
    let retry_at = limited
        .then(|| quota().or_else(|| retry_in(&line).and_then(|wait| at.checked_add(wait))))
        .flatten();
    Failure {
        kind,
        line,
        service: true,
        http_status,
        code,
        retry_at,
        provider: None,
        model: text(message, "model")
            .filter(|model| *model != SYNTHETIC && !model.is_empty())
            .map(str::to_owned),
        thinking: None,
    }
}

/// How long Claude Code said to wait before trying again, in milliseconds: the `Retry in
/// <seconds>s` it writes where a service limited it without saying when the limit lifts
/// (`API Error: Server is temporarily limiting requests (not your usage limit) · All 3
/// accounts exhausted. Retry in 3600s.`, Claude Code 2.1.219). A wait over a day is none:
/// no limit Claude Code waits out is that long.
fn retry_in(said: &str) -> Option<u64> {
    const DAY: u64 = 24 * 60 * 60;
    let rest = said.split_once("Retry in ")?.1;
    let seconds: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if !rest[seconds.len()..].starts_with('s') {
        return None;
    }
    seconds
        .parse::<u64>()
        .ok()
        .filter(|seconds| *seconds > 0 && *seconds <= DAY)
        .map(|seconds| seconds * 1000)
}

/// The kind of a failure Claude Code gave up on, by its own code (the transcript's
/// `error`, the `StopFailure` hook's `error`) and the API's status where it recorded one,
/// never by wording: `authentication_failed` is a sign-in it could not make, whatever the
/// status beside it — Claude Code 2.1.237 wrote it with a 403 for `Failed to
/// authenticate. API Error: 403 Request not allowed`, and 2.1.316 writes it with a 401
/// for an invalid key and with none at all for `Not logged in · Please run /login` — and
/// so is a bare 401; a 429, 503 or 529, `rate_limit` or `overloaded` a limit.
pub fn failure_kind(http_status: Option<u16>, code: Option<&str>) -> FailureKind {
    let signed_out = code == Some("authentication_failed") || http_status == Some(401);
    let limited = http_status.is_some_and(|status| matches!(status, 429 | 503 | 529))
        || matches!(code, Some("rate_limit" | "overloaded"));
    if signed_out {
        FailureKind::SignIn
    } else if limited {
        FailureKind::RateLimit
    } else {
        FailureKind::Other
    }
}

/// Whether Claude Code gave up because the context is over the model's limit: its code for
/// compaction failing over and over (`apiError` `autocompact_thrashing`), or the words it
/// writes for a prompt over the limit, which carry no code of their own beside
/// `invalid_request` (https://code.claude.com/docs/en/errors, "Prompt is too long").
pub fn context_full(api_error: Option<&str>, said: &str) -> bool {
    api_error == Some("autocompact_thrashing")
        || ["Prompt is too long", "Input is too long"]
            .iter()
            .any(|opening| said.trim_start().starts_with(opening))
}

/// What a person put in a message, and what it carried: only one Claude Code marks as
/// theirs (`origin.kind` `human` on `holder`, the record or the attachment that carries
/// `content`), never the harness's own notes inside it (`<system-reminder>`), and never a
/// message that is one element and nothing else, which is how a harness posts in a person's
/// place.
fn person(holder: &Value, content: Option<&Value>) -> Option<(String, Vec<Clip>)> {
    if holder.pointer("/origin/kind").and_then(Value::as_str) != Some("human") {
        return None;
    }
    let content = content?;
    let said = without_reminders(&texts(content)).trim().to_owned();
    if enveloped(&said) {
        return None;
    }
    let clips: Vec<Clip> = blocks(content).iter().filter_map(carried).collect();
    (!said.is_empty() || !clips.is_empty()).then_some((said, clips))
}

/// The words Claude Code took in while a turn ran: an `attachment` record whose
/// `attachment` is a `queued_command` prompt, carrying the words as `prompt` and who sent
/// them as `origin` (`human`, or `peer` through the inbox). No `user` record repeats them
/// (Claude Code 2.1.316). A sub-agent's own (`isSidechain`) is none of its lead's.
pub fn queued(record: &Value) -> Option<&Value> {
    if text(record, "type") != Some("attachment") || flag(record, "isSidechain") {
        return None;
    }
    let attachment = record.get("attachment")?;
    (text(attachment, "type") == Some("queued_command")
        && text(attachment, "commandMode") == Some("prompt"))
    .then_some(attachment)
}

/// Claude Code's own notice that a sub-agent it ran in the background stopped: a
/// `queued_command` attachment whose `commandMode` is `task-notification`, carrying the
/// `<task-notification>` element as its `prompt` (Claude Code 2.1.316 files it there and
/// repeats it in no `user` record). It is nobody's words: it says how a sub-agent ended
/// ([`State::notified`]) and nothing else.
fn notification(record: &Value) -> Option<&str> {
    if text(record, "type") != Some("attachment") || flag(record, "isSidechain") {
        return None;
    }
    let attachment = record.get("attachment")?;
    (text(attachment, "type") == Some("queued_command")
        && text(attachment, "commandMode") == Some("task-notification"))
    .then(|| text(attachment, "prompt"))
    .flatten()
}

/// The words a user record gives the agent to work on: what it says, the harness's notes
/// taken out, where it is no call's result, no note of the harness's own and no summary.
/// A conversation's title and a sub-agent's task are read from its first.
pub fn instruction(record: &Value) -> Option<String> {
    if text(record, "type") != Some("user")
        || ["isMeta", "isCompactSummary", "isSynthetic", "isReplay"]
            .iter()
            .any(|name| flag(record, name))
    {
        return None;
    }
    let content = record.pointer("/message/content")?;
    if blocks(content)
        .iter()
        .any(|block| text(block, "type") == Some("tool_result"))
    {
        return None;
    }
    let said = without_reminders(&texts(content)).trim().to_owned();
    (!said.is_empty()).then_some(said)
}

/// A picture or a document a person's message carried: its bytes in the record, or an
/// address Claude Code passed on. A note where the bytes were (`[base64 omitted: …]`) is
/// bytes Claude Code did not keep, and no clip.
fn carried(block: &Value) -> Option<Clip> {
    if !matches!(text(block, "type"), Some("image" | "document")) {
        return None;
    }
    let source = block.get("source")?;
    match text(source, "type")? {
        "base64" => {
            let data = text(source, "data")?;
            (!data.is_empty() && !data.starts_with("[base64 omitted")).then(|| Clip::Bytes {
                data: data.to_owned(),
                media_type: text(source, "media_type").map(str::to_owned),
                name: None,
            })
        }
        "url" => Some(Clip::Link {
            url: text(source, "url")?.to_owned(),
        }),
        _ => None,
    }
}

/// The sub-agent each call started: every `agent-<id>.meta.json` in `agents` naming its
/// call as `toolUseId`.
fn started(agents: &Path) -> HashMap<String, String> {
    let Ok(entries) = std::fs::read_dir(agents) else {
        return HashMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let agent = name
                .to_str()?
                .strip_prefix("agent-")?
                .strip_suffix(".meta.json")?;
            if !agent_id(agent) || !entry.file_type().is_ok_and(|kind| kind.is_file()) {
                return None;
            }
            let meta: Value = serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
            Some((text(&meta, "toolUseId")?.to_owned(), agent.to_owned()))
        })
        .collect()
}

/// Whether a sub-agent's id can be one component of a path: letters, digits, `-` and `_`.
pub fn agent_id(agent: &str) -> bool {
    !agent.is_empty()
        && agent
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Where Claude Code files sub-agent `agent`'s transcript: `<agents>/agent-<id>.jsonl`.
pub fn agent_file(agents: &Path, agent: &str) -> PathBuf {
    agents.join(format!("agent-{agent}.jsonl"))
}

/// Claude Code's own words where a person stopped the turn: `[Request interrupted by
/// user]`, and `… for tool use]` where they stopped it inside a call.
fn interrupted(said: &str) -> bool {
    said.trim_start()
        .starts_with("[Request interrupted by user")
}

/// The content blocks of a message: none where it is a string.
fn blocks(content: &Value) -> &[Value] {
    content.as_array().map_or(&[], Vec::as_slice)
}

/// The words of a content: itself where it is a string, else its text blocks joined.
fn texts(content: &Value) -> String {
    match content {
        Value::String(said) => said.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| text(block, "type") == Some("text"))
            .filter_map(|block| text(block, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The words inside a call's result, whatever shape Claude Code wrote it in: a string, or
/// content blocks, of which the text ones are read.
fn flatten(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(said)) => said.clone(),
        Some(content @ Value::Array(_)) => texts(content),
        _ => String::new(),
    }
}

/// Text with every `<system-reminder>…</system-reminder>` taken out: Claude Code's own
/// notes, written into a person's message ahead of what they typed and into results.
fn without_reminders(said: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(said.len());
    let mut rest = said;
    while let Some(start) = rest.find(OPEN) {
        let Some(end) = rest[start..].find(CLOSE) else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + end + CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

/// Whether the text is one element, `<name …>…</name>`, and nothing else: how a harness
/// wraps what it posts as the person, never how a person types.
fn enveloped(said: &str) -> bool {
    let said = said.trim();
    let Some(rest) = said.strip_prefix('<') else {
        return false;
    };
    let name = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.')))
        .map_or(rest, |end| &rest[..end]);
    !name.is_empty()
        && rest[name.len()..].starts_with(|c: char| c == '>' || c.is_whitespace())
        && said
            .strip_suffix('>')
            .and_then(|said| said.strip_suffix(name))
            .is_some_and(|said| said.ends_with("</"))
}

/// What the first `<tag>…</tag>` in `said` holds, trimmed.
fn tagged<'a>(said: &'a str, tag: &str) -> Option<&'a str> {
    let rest = said.split_once(&format!("<{tag}>"))?.1;
    Some(rest.split_once(&format!("</{tag}>"))?.0.trim())
}

/// Words as one line: every run of whitespace one space.
pub fn one_line(said: &str) -> String {
    said.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn flag(value: &Value, name: &str) -> bool {
    value.get(name).and_then(Value::as_bool) == Some(true)
}

pub fn text<'a>(value: &'a Value, name: &str) -> Option<&'a str> {
    value.get(name).and_then(Value::as_str)
}

/// An RFC 3339 time in milliseconds since 1970.
pub fn millis(at: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn reading(own: bool) -> Reading {
        Reading {
            own,
            agents: PathBuf::from("/nowhere/c/subagents"),
            window: Window::default(),
            fork: None,
        }
    }

    fn records(lines: &[Value]) -> Vec<Record> {
        lines
            .iter()
            .enumerate()
            .map(|(at, value)| Record {
                start: at as u64 * 10,
                value: Some(value.clone()),
            })
            .collect()
    }

    fn events(lines: &[Value]) -> Vec<Event> {
        read(&reading(false), &records(lines), Turn::Closed, End::Still).events
    }

    fn said(events: &[Event]) -> Vec<(Voice, &str)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Words { voice, text, .. } => Some((*voice, text.as_str())),
                _ => None,
            })
            .collect()
    }

    fn turns(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::TurnBegan { .. } => Some("began".to_owned()),
                Event::TurnEnded { state, .. } => Some(format!("{state:?}")),
                _ => None,
            })
            .collect()
    }

    /// A sub-agent's records inlined into its lead's file, and the harness talking to
    /// itself, are nobody's words in the conversation; reasoning is the agent's thought,
    /// a call's input and result the tool's (the runtime's
    /// `claude_code_skips_sidechains_thinking_and_tool_arguments`).
    #[test]
    fn sidechains_and_the_harness_s_own_notes_are_nobody_s_words() {
        let read = events(&[
            json!({"type":"custom-title","customTitle":"What it is called"}),
            json!({"uuid":"a","type":"user","isMeta":true,"origin":{"kind":"human"},"message":{"role":"user","content":"harness talking to itself"}}),
            json!({"uuid":"b","type":"user","isSidechain":true,"origin":{"kind":"human"},"message":{"role":"user","content":"a sub-agent's instruction"}}),
            json!({"uuid":"c","type":"user","origin":{"kind":"human"},"message":{"role":"user","content":"the real question"}}),
            json!({"uuid":"d","type":"assistant","message":{"id":"m1","role":"assistant","content":[{"type":"thinking","thinking":"private"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"rm -rf /"}},{"type":"text","text":"the answer"}]}}),
            json!({"uuid":"e","type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"what the tool said"}]},{"type":"image","source":{"type":"base64","data":"AAAA"}}]}}),
        ]);
        assert_eq!(
            said(&read),
            [
                (Voice::Person, "the real question"),
                (Voice::Thought, "private"),
                (Voice::Agent, "the answer"),
                (Voice::Tool, "what the tool said"),
            ]
        );
        assert!(read.iter().any(|event| matches!(
            event,
            Event::CallBegan { call, .. } if call.id == "t1" && call.response.as_deref() == Some("m1")
        )));
        assert!(read.iter().any(|event| matches!(
            event,
            Event::CallEnded { id, ok: true, chars: 18, .. } if id == "t1"
        )));
    }

    /// The person's words are only what Claude Code marks as theirs, its notes in front of
    /// them taken out; a task's notice and a command are not theirs (the runtime's
    /// `a_tail_reads_whole_records_one_at_a_time` and typed inputs).
    #[test]
    fn a_person_is_only_who_claude_code_marks_human() {
        let read = events(&[
            json!({"type": "user", "origin": {"kind": "human"}, "message": {"role": "user", "content":
                "<system-reminder>\nIn a worktree.\n</system-reminder>\nship the chips"}}),
            json!({"type": "user", "origin": {"kind": "task-notification"},
                "message": {"role": "user", "content": "<task-notification>done</task-notification>"}}),
            json!({"type": "user", "message": {"role": "user", "content": "<command-name>/compact</command-name>"}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Shipped."}]}}),
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": "ok"}]}}),
            json!({"type": "user", "origin": {"kind": "human"}, "message": {"role": "user", "content": "  and the retries\n\ttoo  "}}),
        ]);
        assert_eq!(
            said(&read),
            [
                (Voice::Person, "ship the chips"),
                (Voice::Agent, "Shipped."),
                (Voice::Tool, "ok"),
                (Voice::Person, "and the retries\n\ttoo"),
            ]
        );
    }

    /// A picture a person pasted is a clip of their words, its bytes as the record holds
    /// them; one Claude Code did not keep is none.
    #[test]
    fn a_pasted_picture_is_a_clip() {
        let read = events(&[
            json!({"uuid": "p", "type": "user", "origin": {"kind": "human"},
            "message": {"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}},
                {"type": "image", "source": {"type": "base64", "data": "[base64 omitted: image/png]"}},
                {"type": "text", "text": "this one"}]}}),
        ]);
        assert!(read.contains(&Event::Words {
            at: 0,
            id: "p".into(),
            voice: Voice::Person,
            text: "this one".into(),
            call: None,
            clips: vec![Clip::Bytes {
                data: "iVBORw0KGgo=".into(),
                media_type: Some("image/png".into()),
                name: None,
            }],
        }));
    }

    /// A turn ends where the agent stops for anything but a call, once however many of
    /// the response's blocks say so; the person interrupting it ends it interrupted; a
    /// local command's answer ends the turn its command began.
    #[test]
    fn a_turn_ends_once_and_says_how() {
        let asked =
            json!({"type": "user", "origin": {"kind": "human"}, "message": {"content": "go"}});
        let answered = |reason: &str| json!({"type": "assistant", "message": {"id": "m", "stop_reason": reason, "content": []}});
        assert_eq!(
            turns(&events(&[
                asked.clone(),
                answered("tool_use"),
                answered("end_turn"),
                answered("end_turn"),
                json!({"type": "system", "subtype": "turn_duration"}),
            ])),
            ["began", "Finished"]
        );
        assert_eq!(
            turns(&events(&[
                asked.clone(),
                answered("tool_use"),
                json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t", "is_error": true, "content": "[Request interrupted by user for tool use]"}]}}),
                json!({"type": "user", "message": {"content": [{"type": "text", "text": "[Request interrupted by user for tool use]"}]}}),
            ])),
            ["began", "Interrupted"]
        );
        assert_eq!(
            turns(&events(&[
                json!({"type": "user", "message": {"content": "<command-name>/model</command-name>\n<command-message>model</command-message>"}}),
                json!({"type": "user", "message": {"content": "<local-command-stdout>Set model to opus</local-command-stdout>"}}),
            ])),
            ["began", "Finished"]
        );
        // A meta note asks the agent nothing.
        assert!(
            turns(&events(&[
                json!({"type": "user", "isMeta": true, "message": {"content": "Caveat"}})
            ]))
            .is_empty()
        );
        // Read from the middle of a turn, its end is still said.
        let middle = read(
            &reading(false),
            &records(&[answered("end_turn")]),
            Turn::Unknown,
            End::Still,
        );
        assert_eq!(turns(&middle.events), ["Finished"]);
        assert_eq!(middle.turn, Turn::Closed);
    }

    /// The API error Claude Code writes once it gave up is the turn's end, failed, with the
    /// service's status and Claude Code's own code, never its wording (the runtime's
    /// `claude_api_error_is_current_until_the_model_answers_or_the_person_types` and
    /// `claude_captured_failure_retains_exact_native_record`).
    #[test]
    fn an_api_error_fails_its_turn_by_status_and_code() {
        let failed = |status: Option<u64>, code: &str| {
            let mut record = json!({"type":"assistant","uuid":"483f5161","timestamp":"2026-09-03T13:26:16.609Z",
                "isApiErrorMessage":true,"error":code,
                "message":{"model":"<synthetic>","content":[{"type":"text","text":"API Error: 500\n Internal server error"}]}});
            if let Some(status) = status {
                record["apiErrorStatus"] = json!(status);
            }
            let read = events(&[
                json!({"type": "user", "origin": {"kind": "human"}, "message": {"content": "go"}}),
                record,
            ]);
            match read.last() {
                Some(Event::TurnEnded {
                    state: TurnState::Failed,
                    failure: Some(failure),
                    at,
                    ..
                }) => {
                    assert_eq!(*at, 1_788_441_976_609);
                    failure.clone()
                }
                other => panic!("{other:?}"),
            }
        };
        let server = failed(Some(500), "server_error");
        assert_eq!(server.kind, FailureKind::Other);
        assert_eq!(server.line, "API Error: 500 Internal server error");
        assert_eq!(
            (server.http_status, server.code.as_deref()),
            (Some(500), Some("server_error"))
        );
        assert!(server.service);
        assert_eq!(
            server.model, None,
            "Claude Code's own record names no model"
        );
        assert_eq!(
            failed(Some(529), "overloaded_error").kind,
            FailureKind::RateLimit
        );
        assert_eq!(failed(None, "rate_limit").kind, FailureKind::RateLimit);
        assert_eq!(
            failed(Some(401), "authentication_failed").kind,
            FailureKind::SignIn
        );
        assert_eq!(
            failed(None, "authentication_failed").kind,
            FailureKind::SignIn
        );
        assert_eq!(
            failed(Some(403), "authentication_failed").kind,
            FailureKind::SignIn,
            "Claude Code's own code for a sign-in it could not make, whatever the status \
             beside it: 2.1.237 wrote `Failed to authenticate. API Error: 403 Request not \
             allowed` with it"
        );
        assert_eq!(failed(None, "invalid_request").kind, FailureKind::Other);

        // When a limit lifts: the quota Claude Code says refused it, else the wait it
        // named in its own words (records as 2.1.219 and 2.1.316 wrote them).
        let limited = |extra: Value, said: &str| {
            let mut record = json!({"type":"assistant","uuid":"9f31","timestamp":"2026-09-03T13:26:16.609Z",
                "isApiErrorMessage":true,"error":"rate_limit","apiErrorStatus":429,"apiErrorIsTransient":true,
                "message":{"model":"<synthetic>","content":[{"type":"text","text":said}]}});
            record.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            match events(&[
                json!({"type": "user", "origin": {"kind": "human"}, "message": {"content": "go"}}),
                record,
            ])
            .pop()
            {
                Some(Event::TurnEnded {
                    failure: Some(failure),
                    ..
                }) => (failure.kind, failure.retry_at),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            limited(
                json!({}),
                "API Error: Server is temporarily limiting requests (not your usage limit) · All 3 accounts exhausted. Retry in 3600s."
            ),
            (FailureKind::RateLimit, Some(1_788_441_976_609 + 3_600_000)),
            "the wait Claude Code named, from when it said it"
        );
        assert_eq!(
            limited(
                json!({"quotaLimits": {"status": "rejected", "resetsAt": 1_788_460_000u64, "rateLimitType": "five_hour"}}),
                "You've hit your session limit · resets 5pm"
            ),
            (FailureKind::RateLimit, Some(1_788_460_000_000)),
            "the quota that refused it outranks any wording"
        );
        assert_eq!(
            limited(json!({}), "API Error: Connection closed mid-response.").1,
            None,
            "a limit that said nothing of when it lifts says nothing"
        );
        assert_eq!(
            retry_in("Retry in 0s."),
            None,
            "a wait of nothing is no wait"
        );
        assert_eq!(retry_in("Retry in 90000s"), None, "longer than any limit");
        assert_eq!(retry_in("Retry in 60 seconds"), None, "not the form it writes");

        // A context over the limit, as Claude Code 2.1.281 wrote it: the prompt too long
        // for compaction to save, and compaction thrashing.
        let full = |extra: Value, said: &str| {
            let mut record = json!({"type":"assistant","isApiErrorMessage":true,"error":"invalid_request",
                "message":{"model":"<synthetic>","content":[{"type":"text","text":said}]}});
            record.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            match events(&[
                json!({"type": "user", "origin": {"kind": "human"}, "message": {"content": "go"}}),
                record,
            ])
            .pop()
            {
                Some(Event::TurnEnded {
                    failure: Some(failure),
                    ..
                }) => failure.kind,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            full(json!({}), "Prompt is too long · automatic compaction failed: Please run /login · API Error: 403"),
            FailureKind::ContextFull
        );
        assert_eq!(
            full(json!({"apiError": "autocompact_thrashing"}), "Autocompact is thrashing: the context refilled"),
            FailureKind::ContextFull
        );

        // Claude Code 2.1.296's weekly limit, as it wrote it: the quota that refused names
        // when it resets; a quota it took names nothing to wait for.
        let quota = |status: &str| {
            json!({"status": status, "resetsAt": 1_791_849_600_u64, "rateLimitType": "seven_day"})
        };
        let limit = |code: &str, quota: Value| {
            let mut read = events(&[
                json!({"type": "user", "origin": {"kind": "human"}, "message": {"content": "go"}}),
                json!({"type":"assistant","timestamp":"2026-10-12T12:07:15.374Z","isApiErrorMessage":true,
                    "error":code,"apiErrorStatus":429,"quotaLimits":quota,
                    "message":{"model":"<synthetic>","content":[{"type":"text","text":"You've hit your weekly limit · resets 2am (Europe/Oslo)"}]}}),
            ]);
            match read.pop() {
                Some(Event::TurnEnded {
                    failure: Some(failure),
                    ..
                }) => failure,
                other => panic!("{other:?}"),
            }
        };
        let weekly = limit("rate_limit", quota("rejected"));
        assert_eq!(weekly.kind, FailureKind::RateLimit);
        assert_eq!(weekly.retry_at, Some(1_791_849_600_000));
        assert_eq!(limit("rate_limit", quota("allowed")).retry_at, None);
        assert_eq!(limit("rate_limit", Value::Null).retry_at, None);
    }

    /// Claude Code names the model on every assistant record and the effort beside it,
    /// never on what it writes itself (the runtime's `claudes_own_messages_name_no_model`
    /// and `claude_says_what_it_runs_on_and_which_call_is_open`).
    #[test]
    fn the_model_is_said_when_it_changes_and_never_claude_code_s_own() {
        let read = events(&[
            json!({"timestamp":"2026-09-19T10:00:00.000Z","type":"assistant","effort":"xhigh","message":{"model":"claude-opus-5","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo build","description":"Build"}}]}}),
            json!({"timestamp":"2026-09-19T10:00:05.000Z","type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}),
            json!({"timestamp":"2026-09-19T10:00:06.000Z","type":"assistant","effort":"xhigh","message":{"model":"claude-opus-5","stop_reason":"tool_use","content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"/src/lib.rs"}}]}}),
            json!({"timestamp":"2026-09-19T10:00:07.000Z","type":"assistant","message":{"model":"claude-sonnet-5","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}}),
            json!({"timestamp":"2026-09-19T10:00:08.000Z","type":"assistant","isApiErrorMessage":true,"message":{"model":"<synthetic>","content":[{"type":"text","text":"Autocompact is thrashing"}]}}),
            json!({"timestamp":"2026-09-19T10:00:09.000Z","type":"assistant","message":{"model":"<synthetic>","content":[{"type":"text","text":"No response requested."}]}}),
        ]);
        let models: Vec<(&str, Option<&str>)> = read
            .iter()
            .filter_map(|event| match event {
                Event::Model {
                    model, thinking, ..
                } => Some((model.as_str(), thinking.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(
            models,
            [("claude-opus-5", Some("xhigh")), ("claude-sonnet-5", None)]
        );
        let ended: Vec<(&str, u64)> = read
            .iter()
            .filter_map(|event| match event {
                Event::CallEnded { id, at, .. } => Some((id.as_str(), *at)),
                _ => None,
            })
            .collect();
        assert_eq!(
            ended,
            [("t1", 1_789_812_005_000)],
            "t1 came back when its result was written"
        );
        assert!(!said(&read).contains(&(Voice::Agent, "No response requested.")));
    }

    /// Claude Code writes each response's count once per content block: each is that
    /// response's, under its id, a block repeating it unchanged said once (the runtime's
    /// `a_claude_count_is_each_message_once`).
    #[test]
    fn a_response_s_count_is_its_own() {
        let counted = |id: &str,
                       input: u64,
                       created: u64,
                       read: u64,
                       output: u64,
                       thinking: u64| {
            json!({"type": "assistant", "message": {"id": id, "model": "claude-opus-4", "usage": {
                "input_tokens": input, "cache_creation_input_tokens": created,
                "cache_read_input_tokens": read, "output_tokens": output,
                "output_tokens_details": {"thinking_tokens": thinking}}}})
        };
        let read = events(&[
            counted("m1", 2, 1000, 0, 0, 0),
            counted("m1", 2, 1000, 0, 382, 40),
            counted("m1", 2, 1000, 0, 382, 40),
            json!({"type": "user", "origin": {"kind": "human"}, "message": {"role": "user", "content": "usage please"}}),
            counted("m2", 3, 0, 1000, 20, 0),
        ]);
        let tokens: Vec<_> = read
            .iter()
            .filter_map(|event| match event {
                Event::Tokens {
                    response,
                    input,
                    cached,
                    cache_write,
                    output,
                    reasoning,
                    count: Count::Response,
                    ..
                } => Some((
                    response.as_deref()?,
                    *input,
                    *cached,
                    *cache_write,
                    *output,
                    *reasoning,
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            tokens,
            [
                ("m1", 1002, 0, Some(1000), 0, 0),
                ("m1", 1002, 0, Some(1000), 382, 40),
                ("m2", 1003, 1000, Some(0), 20, 0),
            ]
        );
        // What the last response held of Claude Opus 4's 200K window.
        assert_eq!(
            read.iter().rev().find_map(|event| match event {
                Event::Tokens { context_left, .. } => *context_left,
                _ => None,
            }),
            Some(1.0 - 1023.0 / 200_000.0)
        );
    }

    /// A sub-agent its lead started in the background, and the notice Claude Code posted
    /// once it failed: running from the call, failed with the reason it was told (the
    /// runtime's `a_sub_agent_reads_failed_with_its_harness_reason`, records as Claude Code
    /// 2.1 wrote them for a background `Task` that hit a rate limit, 2026-09-05).
    #[test]
    fn a_sub_agent_runs_from_its_call_and_ends_as_its_lead_was_told() {
        let home = std::env::temp_dir().join(format!("steery-cc-events-{}", std::process::id()));
        let agents = home.join("lead/subagents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("agent-f00d.jsonl"), "{}\n").unwrap();
        std::fs::write(
            agents.join("agent-f00d.meta.json"),
            r#"{"agentType":"general-purpose","description":"Slice two","toolUseId":"toolu_9"}"#,
        )
        .unwrap();
        let notice = "<task-notification>\n<task-id>f00d</task-id>\n<tool-use-id>toolu_9</tool-use-id>\n<status>failed</status>\n<summary>Agent \"Slice two\" failed: Agent terminated early due to an API error: 429 Too Many Requests</summary>\n</task-notification>";
        let shell = "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>";
        let reading = Reading {
            own: false,
            agents,
            window: Window::default(),
            fork: None,
        };
        let read = read(
            &reading,
            &records(&[
                json!({"type":"user","origin":{"kind":"human"},"message":{"role":"user","content":"Build slice two"}}),
                json!({"type":"assistant","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_9","name":"Task","input":{"description":"Slice two","prompt":"Build it","run_in_background":true}}]}}),
                json!({"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_9","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully."}]}]},"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"f00d"}}),
                json!({"type":"user","origin":{"kind":"task-notification"},"message":{"role":"user","content":notice}}),
                json!({"type":"user","origin":{"kind":"task-notification"},"message":{"role":"user","content":shell}}),
            ]),
            Turn::Closed,
            End::Still,
        );
        std::fs::remove_dir_all(&home).ok();
        let agents: Vec<_> = read
            .events
            .iter()
            .filter_map(|event| match event {
                Event::SubAgent {
                    external_id,
                    call,
                    state,
                    error,
                    ..
                } => Some((
                    external_id.as_str(),
                    call.as_deref(),
                    *state,
                    error.as_deref(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            agents,
            [
                ("f00d", Some("toolu_9"), Some(Execution::Running), None),
                (
                    "f00d",
                    Some("toolu_9"),
                    Some(Execution::Failed),
                    Some("Agent terminated early due to an API error: 429 Too Many Requests")
                ),
            ],
            "a background shell's notice names no sub-agent"
        );
    }

    /// A sub-agent's own transcript is all `isSidechain`, and all its own: its instruction
    /// begins its turn, and `end_turn` ends it.
    #[test]
    fn a_sub_agent_s_own_records_are_its_own() {
        let read = read(
            &reading(true),
            &records(&[
                json!({"type":"user","isSidechain":true,"agentId":"f00d","message":{"role":"user","content":"Build it"}}),
                json!({"type":"assistant","isSidechain":true,"agentId":"f00d","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Half done"}]}}),
            ]),
            Turn::Closed,
            End::Still,
        );
        assert_eq!(turns(&read.events), ["began", "Finished"]);
        assert_eq!(said(&read.events), [(Voice::Agent, "Half done")]);
    }

    /// Claude Code 2.1.316 files the notice of a background sub-agent that stopped as a
    /// `queued_command` attachment whose `commandMode` is `task-notification`, and repeats
    /// it in no `user` record: it ends that sub-agent, and is nobody's words.
    #[test]
    fn a_notice_filed_as_an_attachment_ends_its_sub_agent() {
        let home = std::env::temp_dir().join(format!(
            "steery-cc-notice-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let agents = home.join("lead/subagents");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("agent-a77006.jsonl"), "{}\n").unwrap();
        std::fs::write(
            agents.join("agent-a77006.meta.json"),
            r#"{"agentType":"general-purpose","description":"Read the build log","toolUseId":"toolu_01PARITY"}"#,
        )
        .unwrap();
        let notice = "<task-notification>\n<task-id>a77006</task-id>\n<tool-use-id>toolu_01PARITY</tool-use-id>\n<output-file>/tmp/steery-example/tasks/a77006.output</output-file>\n<status>completed</status>\n<summary>Agent \"Read the build log\" finished</summary>\n</task-notification>";
        let read = read(
            &Reading {
                own: false,
                agents,
                window: Window::default(),
                fork: None,
            },
            &records(&[
                json!({"uuid":"u1","type":"user","origin":{"kind":"human"},"message":{"role":"user","content":"Read the build log in the background"}}),
                json!({"uuid":"a1","type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_01PARITY","name":"Task","input":{"description":"Read the build log","prompt":"Read it","run_in_background":true}}]}}),
                json!({"uuid":"u2","type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01PARITY","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully."}]}]},"toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a77006"}}),
                json!({"uuid":"a2","type":"assistant","message":{"id":"m2","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"It reads in the background."}]}}),
                json!({"uuid":"x1","type":"attachment","isSidechain":false,
                    "attachment":{"type":"queued_command","commandMode":"task-notification","prompt":notice,
                        "origin":{"kind":"task-notification","producer":"session-task"}}}),
            ]),
            Turn::Closed,
            End::Still,
        );
        std::fs::remove_dir_all(&home).ok();
        let agents: Vec<_> = read
            .events
            .iter()
            .filter_map(|event| match event {
                Event::SubAgent {
                    external_id, state, ..
                } => Some((external_id.as_str(), *state)),
                _ => None,
            })
            .collect();
        assert_eq!(
            agents,
            [
                ("a77006", Some(Execution::Running)),
                ("a77006", Some(Execution::Finished)),
            ],
            "the notice ends the sub-agent its lead started"
        );
        assert_eq!(
            said(&read.events),
            [
                (Voice::Person, "Read the build log in the background"),
                (Voice::Tool, "Async agent launched successfully."),
                (Voice::Agent, "It reads in the background."),
            ],
            "a notice is nobody's words"
        );
        assert_eq!(
            turns(&read.events),
            ["began", "Finished"],
            "a notice begins no turn of its own"
        );
    }

    /// A sub-agent of Claude Code 2.1.316 ends its turn by handing its report back
    /// (`SubagentHandback`), and writes no answer of its own after it: the result Claude
    /// Code wrote for that call ends the turn, failed in its words where it refused it.
    #[test]
    fn a_sub_agent_s_turn_ends_at_its_handback() {
        let handed = |result: Value, error: bool| {
            read(
                &reading(true),
                &records(&[
                    json!({"uuid":"s1","type":"user","isSidechain":true,"agentId":"a77006","message":{"role":"user","content":"Read the build log"}}),
                    json!({"uuid":"s2","type":"assistant","isSidechain":true,"agentId":"a77006","message":{"id":"m1","role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_01HAND","name":"SubagentHandback","input":{"message":"The build log ends at a linker error."}}]}}),
                    json!({"uuid":"s3","type":"user","isSidechain":true,"agentId":"a77006","message":{"role":"user","content":[{"tool_use_id":"toolu_01HAND","type":"tool_result","is_error":error,"content":[{"type":"text","text":result.to_string()}]}]},"toolUseResult":result}),
                ]),
                Turn::Closed,
                End::Still,
            )
            .events
        };
        let delivered = handed(
            json!({"success": true, "message": "Report delivered to your caller."}),
            false,
        );
        assert_eq!(turns(&delivered), ["began", "Finished"]);
        let refused = handed(
            json!({"success": false, "message": "No caller is listening for this report."}),
            false,
        );
        assert_eq!(turns(&refused), ["began", "Failed"]);
        assert!(
            matches!(
                refused.last(),
                Some(Event::TurnEnded { failure: Some(failure), .. })
                    if failure.line.contains("No caller is listening") && !failure.service
            ),
            "the hand-back Claude Code refused, in its words: {:?}",
            refused.last()
        );
    }

    /// An error Claude Code wrote mid-response is not its turn's end while the turn may go
    /// on: a retry's answer after it is one it got past (Claude Code 2.1.237 wrote `API
    /// Error: The socket connection was closed unexpectedly.` and went on with the same
    /// turn), and a transcript that may still be written holds it back with everything
    /// after it, until it stood still for [`SETTLE`].
    #[test]
    fn an_error_claude_code_got_past_is_no_turn_end() {
        let dropped = json!({"uuid":"a2","type":"assistant","isApiErrorMessage":true,"error":"unknown",
            "message":{"id":"m2","model":"<synthetic>","role":"assistant","stop_reason":"stop_sequence",
                "content":[{"type":"text","text":"API Error: The socket connection was closed unexpectedly."}]}});
        let lines = [
            json!({"uuid":"u1","type":"user","origin":{"kind":"human"},"message":{"role":"user","content":"Patch the guard"}}),
            json!({"uuid":"a1","type":"assistant","message":{"id":"m1","role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"toolu_01EDIT","name":"Edit","input":{"file_path":"/tmp/steery-example/guard.rs"}}]}}),
            dropped.clone(),
            json!({"uuid":"u2","type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01EDIT","type":"tool_result","content":[{"type":"text","text":"Edited guard.rs"}]}]}}),
            json!({"uuid":"a3","type":"assistant","message":{"id":"m3","role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"Patched it."}]}}),
        ];
        assert_eq!(
            turns(&events(&lines)),
            ["began", "Finished"],
            "the model answered again: the error is one Claude Code got past"
        );
        // The same turn as far as the error, in a transcript that may still be written:
        // the error and what follows it wait for the next read.
        let waiting = read(
            &reading(false),
            &records(&lines[..3]),
            Turn::Closed,
            End::Open,
        );
        assert_eq!(turns(&waiting.events), ["began"]);
        assert_eq!(waiting.consumed, 2, "the error is left for the next read");
        assert_eq!(waiting.turn, Turn::Open);
        // Once the transcript stood still, the error is the turn's end.
        let still = read(
            &reading(false),
            &records(&lines[..3]),
            Turn::Closed,
            End::Still,
        );
        assert_eq!(turns(&still.events), ["began", "Failed"]);
        assert_eq!(still.consumed, 3);
        // A turn Claude Code gave up on says so, whatever the record that says it is over:
        // 2.1.316 writes a `turn_duration` after the error it stopped on.
        let over = events(&[
            lines[0].clone(),
            dropped,
            json!({"uuid":"s1","type":"system","subtype":"turn_duration","durationMs":1200,"messageCount":2}),
        ]);
        assert_eq!(turns(&over), ["began", "Failed"]);
        assert!(matches!(
            over.last(),
            Some(Event::TurnEnded { failure: Some(failure), .. })
                if failure.line.starts_with("API Error: The socket connection")
        ));
    }

    /// Words another session sent through a live session's inbox are `isMeta` with a
    /// `peer` origin, and begin a turn all the same; they are no person's words. Records as
    /// Claude Code 2.1.295 wrote them, signed out, 2026-10-09.
    #[test]
    fn a_peer_message_begins_a_turn() {
        let failed = |uuid: &str, parent: &str, at: &str| {
            json!({"parentUuid":parent,"type":"assistant","uuid":uuid,"timestamp":at,"isApiErrorMessage":true,"error":"authentication_failed",
                "message":{"id":uuid,"model":"<synthetic>","role":"assistant","stop_reason":"stop_sequence",
                    "usage":{"input_tokens":0,"output_tokens":0},"content":[{"type":"text","text":"Not logged in · Please run /login"}]}})
        };
        let read = events(&[
            json!({"parentUuid":null,"type":"user","message":{"role":"user","content":"say hi\n"},"uuid":"7a41","timestamp":"2026-10-09T09:50:13.972Z","origin":{"kind":"human"},"entrypoint":"cli"}),
            failed("da7c", "7a41", "2026-10-09T09:50:14.121Z"),
            json!({"parentUuid":"da7c","type":"system","subtype":"turn_duration","timestamp":"2026-10-09T09:50:14.147Z","uuid":"bb62","isMeta":false}),
            json!({"parentUuid":"bb62","type":"user","message":{"role":"user","content":"Another Claude session sent a message:\nsteery-delivery:d9 hello"},"isMeta":true,"uuid":"45f6","timestamp":"2026-10-09T09:50:31.351Z","origin":{"kind":"peer","from":"unknown"}}),
            failed("7620", "45f6", "2026-10-09T09:50:31.376Z"),
            json!({"parentUuid":"7620","type":"system","subtype":"turn_duration","timestamp":"2026-10-09T09:50:31.402Z","uuid":"49be","isMeta":false}),
        ]);
        assert_eq!(turns(&read), ["began", "Failed", "began", "Failed"]);
        assert_eq!(said(&read), [(Voice::Person, "say hi")]);
        assert!(
            read.iter()
                .all(|event| !matches!(event, Event::Tokens { .. })),
            "Claude Code's own record spent nothing"
        );
        assert!(read.iter().any(|event| matches!(
            event,
            Event::TurnEnded {
                failure: Some(Failure {
                    kind: FailureKind::SignIn,
                    http_status: None,
                    ..
                }),
                ..
            }
        )));
    }

    /// What a person types while a turn runs is a queued prompt, which no `user` record
    /// repeats: their words, in the turn that was running. Another session's message queued
    /// the same way is no person's. The records' shape as Claude Code 2.1.316 wrote them.
    #[test]
    fn words_typed_while_a_turn_runs_are_the_persons() {
        let queued = |uuid: &str, origin: &str, prompt: &str| {
            json!({"type": "attachment", "uuid": uuid, "isSidechain": false,
                "attachment": {"type": "queued_command", "commandMode": "prompt",
                    "origin": {"kind": origin}, "prompt": prompt}})
        };
        let read = events(&[
            json!({"type": "user", "origin": {"kind": "human"}, "message": {"content": "review the PR"}}),
            queued("q1", "human", "I cannot follow this PR, explain it simply"),
            queued("q2", "peer", "Another Claude session sent a message:\nhello"),
            json!({"type": "assistant", "message": {"id": "m", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "Here it is, simply."}]}}),
        ]);
        assert_eq!(turns(&read), ["began", "Finished"]);
        assert_eq!(
            said(&read),
            [
                (Voice::Person, "review the PR"),
                (Voice::Person, "I cannot follow this PR, explain it simply"),
                (Voice::Agent, "Here it is, simply."),
            ]
        );
    }

    /// A compaction is its `compact_boundary`: who made it by its `trigger`, and the
    /// context's tokens before and after; its summary is the harness's summary of earlier
    /// history. A sub-agent's compaction inlined in its lead's transcript is none of the
    /// lead's.
    #[test]
    fn a_compaction_says_who_made_it_and_is_a_summary() {
        let boundary = |uuid: &str, metadata: Value| {
            json!({"parentUuid": null, "logicalParentUuid": "735e2458", "isSidechain": false,
                "type": "system", "subtype": "compact_boundary", "content": "Conversation compacted",
                "isMeta": false, "timestamp": "2026-06-30T11:39:50.021Z", "uuid": uuid, "level": "info",
                "compactMetadata": metadata})
        };
        let read = events(&[
            boundary("147513ec", json!({"trigger": "manual", "preTokens": 165775, "durationMs": 95526,
                "preservedSegment": {"headUuid": "6d6a85c0", "anchorUuid": "d153c248", "tailUuid": "735e2458"}})),
            json!({"type": "user", "isCompactSummary": true, "isVisibleInTranscriptOnly": true,
            "message": {"role": "user", "content": "This session is being continued. Summary: built the parser."}}),
            boundary("2f0c9d1a", json!({"trigger": "auto", "preTokens": 929582, "postTokens": 41230,
                "cumulativeDroppedTokens": 888352, "durationMs": 70211})),
            boundary("5e1b7c40", json!({"preTokens": 120000})),
            json!({"isSidechain": true, "type": "system", "subtype": "compact_boundary", "uuid": "side",
                "compactMetadata": {"trigger": "auto", "preTokens": 180000}}),
        ]);
        assert_eq!(
            said(&read),
            [(
                Voice::Summary,
                "This session is being continued. Summary: built the parser."
            )]
        );
        let at = millis("2026-06-30T11:39:50.021Z").unwrap();
        let compacted = |id: &str, by: Compactor, before: u64, after: Option<u64>| Event::Compacted {
            at,
            id: id.into(),
            by,
            tokens_before: Some(before),
            tokens_after: after,
        };
        let compactions: Vec<&Event> = read
            .iter()
            .filter(|event| matches!(event, Event::Compacted { .. }))
            .collect();
        assert_eq!(
            compactions,
            [
                &compacted("147513ec", Compactor::Person, 165_775, None),
                &compacted(
                    "2f0c9d1a",
                    Compactor::Harness {
                        reason: Some("threshold".into())
                    },
                    929_582,
                    Some(41_230)
                ),
                &compacted("5e1b7c40", Compactor::Harness { reason: None }, 120_000, None),
            ]
        );
    }
}
