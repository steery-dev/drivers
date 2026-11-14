//! One OpenHands conversation, its events read as Steery's events.
//!
//! OpenHands writes each event of a conversation as a file of its own,
//! `events/event-<NNNNN>-<id>.json`, numbered from 0 in the order it appended them, and keeps
//! the conversation's state in `base_state.json` beside them. The events Steery reads, by
//! their `kind`:
//! - `MessageEvent`: words. `source: user` is a person's prompt, which begins a turn, unless
//!   it is OpenHands' own nudge to a model that answered with neither a call nor words;
//!   `source: agent` is the agent's answer, its `reasoning_content` the agent's thinking.
//!   `source: environment` is a `Stop` hook's feedback, no person's.
//! - `ActionEvent`: a call, `tool_call_id` and `tool_name`, its input the arguments of
//!   `tool_call`; its `thought` is the agent's words beside the call.
//! - `ObservationEvent`, `UserRejectObservation` and `AgentErrorEvent`: the end of a call.
//!   An `AgentErrorEvent` is the model's call that OpenHands could not run, and goes back to
//!   the model; the turn goes on.
//! - `ConversationErrorEvent`: the turn failed, with the model service's `code` and `detail`.
//! - `PauseEvent`: the person paused the conversation.
//!
//! OpenHands writes no event where a turn ends. A turn ends where the next prompt begins:
//! answered, where its agent's last word was an answer or `finish`; failed, where a
//! `ConversationErrorEvent` came; interrupted, else. The last turn ends as `base_state.json`'s
//! `execution_status` says, or stays open while it runs.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use steery_sdk::wire::harness::{
    Call, Compactor, Count, Event, Failure, FailureKind, Group, TurnState, Voice,
};

/// Steery's MCP server, by the name setup gives it.
pub(crate) const STEERY: &str = "steery";

/// How many of a result's last lines `call_ended` carries.
const TAIL_LINES: usize = 8;

/// The words OpenHands sends as `user` where the model answered with neither a call nor
/// words (`sdk/agent/response_dispatch.py`).
const NUDGE: &str = "Your last response did not include a function call or a message.";

/// The kind of an action that calls an MCP server's tool.
const MCP_ACTION: &str = "MCPToolAction";

/// What `base_state.json` says of the conversation, and the MCP servers it can call.
#[derive(Default)]
pub struct Facts {
    pub model: Option<String>,
    pub thinking: Option<String>,
    /// The names of the MCP servers in the person's `mcp.json`: OpenHands names a tool of
    /// one server as the server does, and a tool of one of several `<server>_<tool>`.
    pub servers: Vec<String>,
    /// Each model response's token usage, by its id: `stats.usage_to_metrics.agent.
    /// token_usages`.
    pub usages: HashMap<String, Value>,
}

impl Facts {
    /// The facts of a `base_state.json` document.
    pub fn of(state: &Value, servers: Vec<String>) -> Self {
        let llm = &state["agent"]["llm"];
        let text = |value: &Value| {
            value
                .as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let usages = state["stats"]["usage_to_metrics"]["agent"]["token_usages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|usage| Some((text(&usage["response_id"])?, usage.clone())))
            .collect();
        Self {
            model: text(&llm["model"]),
            thinking: text(&llm["reasoning_effort"]),
            servers,
            usages,
        }
    }
}

/// One event file: its number, the bytes it holds, and its document.
pub struct Record {
    pub index: u64,
    pub len: u64,
    pub value: Value,
}

/// The conversation's events, in order, each with the number of its record.
#[derive(Default)]
pub struct Parsed {
    pub said: Vec<(u64, Event)>,
    /// The turn that has begun and not ended: its prompt's id.
    pub open: Option<String>,
    /// When the open turn began.
    pub began: Option<u64>,
    /// The id of the newest record.
    pub newest: Option<String>,
    /// When the first event was written.
    pub first: Option<u64>,
    /// When the newest event was written.
    pub at: u64,
    /// The open turn's failure, where one came.
    failure: Option<Failure>,
    /// The open turn's person paused it.
    paused: bool,
    /// The agent's last word in the open turn was an answer.
    answered: bool,
}

impl Parsed {
    /// The turn's end, as `base_state.json`'s `execution_status` (`status`) says it, where
    /// it is about the open turn: OpenHands sets a finished or stuck conversation idle as a
    /// prompt comes, but leaves a failed or paused one so until its next run starts.
    pub fn settle(&mut self, status: &str, index: u64) {
        let state = match status {
            "finished" => TurnState::Finished,
            "stuck" => {
                self.failure
                    .get_or_insert_with(|| other("OpenHands found the agent stuck"));
                TurnState::Failed
            }
            "error" if self.failure.is_some() => TurnState::Failed,
            "paused" if self.paused => TurnState::Interrupted,
            _ => return,
        };
        if let Some(turn) = self.open.take() {
            self.ended(index, turn, state);
        }
    }

    /// The open turn, cut off: nothing runs it.
    pub fn interrupt(&mut self, index: u64) {
        if let Some(turn) = self.open.take() {
            self.failure = None;
            self.ended(index, turn, TurnState::Interrupted);
        }
    }

    fn ended(&mut self, index: u64, turn: String, state: TurnState) {
        let failure = self.failure.take().filter(|_| state == TurnState::Failed);
        self.said.push((
            index,
            Event::TurnEnded {
                at: self.at,
                turn: Some(turn),
                state,
                failure,
            },
        ));
    }
}

/// The records, in order, as events.
pub fn parse(records: &[Record], facts: &Facts) -> Parsed {
    let mut reading = Reading {
        facts,
        parsed: Parsed::default(),
        index: 0,
        responses: HashSet::new(),
        requested: false,
    };
    for record in records {
        reading.index = record.index;
        reading.record(&record.value);
    }
    reading.parsed
}

/// Whether a record is a person's prompt.
pub fn prompt(value: &Value) -> Option<String> {
    if value["kind"] != "MessageEvent" || value["source"] != "user" {
        return None;
    }
    let text = texts(&value["llm_message"]["content"]);
    (!text.is_empty() && !text.starts_with(NUDGE)).then_some(text)
}

struct Reading<'a> {
    facts: &'a Facts,
    parsed: Parsed,
    index: u64,
    /// The model responses whose tokens were counted.
    responses: HashSet<String>,
    /// A `CondensationRequest` waits for its condensation.
    requested: bool,
}

impl Reading<'_> {
    fn push(&mut self, event: Event) {
        self.parsed.said.push((self.index, event));
    }

    fn record(&mut self, value: &Value) {
        let Some(id) = value["id"].as_str() else {
            return;
        };
        if let Some(at) = value["timestamp"].as_str().and_then(millis) {
            self.parsed.at = at;
            if self.parsed.first.is_none() {
                self.parsed.first = Some(at);
                if let Some(model) = self.facts.model.clone() {
                    self.push(Event::Model {
                        at,
                        model,
                        provider: None,
                        thinking: self.facts.thinking.clone(),
                    });
                }
            }
        }
        self.parsed.newest = Some(id.to_owned());
        match value["kind"].as_str() {
            Some("MessageEvent") => self.message(id, value),
            Some("ActionEvent") => self.action(id, value),
            Some("ObservationEvent") => {
                let observation = &value["observation"];
                self.call_ended(
                    value,
                    observation["is_error"] != true,
                    &texts(&observation["content"]),
                );
            }
            Some("UserRejectObservation") => {
                let reason = value["rejection_reason"].as_str().unwrap_or("rejected");
                self.call_ended(value, false, reason);
            }
            Some("AgentErrorEvent") => {
                let error = value["error"].as_str().unwrap_or("error");
                self.call_ended(value, false, error);
            }
            Some("ConversationErrorEvent") => {
                if self.parsed.open.is_some() {
                    self.parsed.failure = Some(self.failure(value));
                }
            }
            Some("PauseEvent") => self.parsed.paused = true,
            Some("CondensationRequest") => self.requested = true,
            Some("Condensation") => {
                // Without a request, OpenHands' condenser found the context over its limit.
                let reason = (!std::mem::take(&mut self.requested)).then(|| "threshold".to_owned());
                let at = self.parsed.at;
                self.push(Event::Compacted {
                    at,
                    id: id.to_owned(),
                    by: Compactor::Harness { reason },
                    tokens_before: None,
                    tokens_after: None,
                });
            }
            _ => {}
        }
        if let Some(response) = value["llm_response_id"].as_str() {
            self.tokens(response);
        }
    }

    /// The tokens of model response `response`, once, where `base_state.json` counts them.
    /// LiteLLM's `prompt_tokens` holds the cached tokens too.
    fn tokens(&mut self, response: &str) {
        let Some(usage) = self.facts.usages.get(response) else {
            return;
        };
        if !self.responses.insert(response.to_owned()) {
            return;
        }
        let count = |key: &str| usage[key].as_u64().unwrap_or(0);
        let input = count("prompt_tokens");
        let window = count("context_window");
        let at = self.parsed.at;
        self.push(Event::Tokens {
            at,
            count: Count::Response,
            response: Some(response.to_owned()),
            model: usage["model"].as_str().map(str::to_owned),
            input,
            cached: count("cache_read_tokens"),
            cache_write: Some(count("cache_write_tokens")),
            output: count("completion_tokens"),
            reasoning: count("reasoning_tokens"),
            context_left: (window > 0)
                .then(|| (1.0 - input as f64 / window as f64).clamp(0.0, 1.0)),
        });
    }

    fn message(&mut self, id: &str, value: &Value) {
        let at = self.parsed.at;
        match value["source"].as_str() {
            Some("user") => {
                let Some(text) = prompt(value) else {
                    return;
                };
                if let Some(turn) = self.parsed.open.take() {
                    let state = match (&self.parsed.failure, self.parsed.answered) {
                        (Some(_), _) => TurnState::Failed,
                        (None, true) => TurnState::Finished,
                        (None, false) => TurnState::Interrupted,
                    };
                    self.parsed.ended(self.index, turn, state);
                }
                self.parsed.failure = None;
                self.parsed.paused = false;
                self.parsed.answered = false;
                self.push(Event::TurnBegan {
                    at,
                    turn: Some(id.to_owned()),
                });
                self.push(Event::Words {
                    at,
                    id: id.to_owned(),
                    voice: Voice::Person,
                    text,
                    message: None,
                    call: None,
                    clips: Vec::new(),
                });
                self.parsed.open = Some(id.to_owned());
                self.parsed.began = Some(at);
            }
            Some("agent") => {
                self.thought(id, value);
                let text = texts(&value["llm_message"]["content"]);
                if !text.is_empty() {
                    self.words(id, "", Voice::Agent, text, value);
                }
                self.parsed.answered = true;
            }
            _ => {}
        }
    }

    fn action(&mut self, id: &str, value: &Value) {
        self.thought(id, value);
        let thought = texts(&value["thought"]);
        if !thought.is_empty() {
            self.words(id, ":said", Voice::Agent, thought, value);
        }
        let (Some(call_id), Some(tool)) =
            (value["tool_call_id"].as_str(), value["tool_name"].as_str())
        else {
            return;
        };
        let at = self.parsed.at;
        let call = call(
            call_id,
            tool,
            input(value),
            value["action"]["kind"].as_str(),
            value["llm_response_id"].as_str(),
            &self.facts.servers,
        );
        self.push(Event::CallBegan { at, call });
        self.parsed.answered = tool == "finish";
    }

    /// The model's reasoning beside an answer or a call.
    fn thought(&mut self, id: &str, value: &Value) {
        if let Some(text) = value["reasoning_content"]
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            self.words(id, ":thought", Voice::Thought, text.to_owned(), value);
        }
    }

    fn words(&mut self, id: &str, part: &str, voice: Voice, text: String, value: &Value) {
        let at = self.parsed.at;
        self.push(Event::Words {
            at,
            id: format!("{id}{part}"),
            voice,
            text,
            message: value["llm_response_id"].as_str().map(str::to_owned),
            call: None,
            clips: Vec::new(),
        });
    }

    fn call_ended(&mut self, value: &Value, ok: bool, text: &str) {
        let Some(id) = value["tool_call_id"].as_str() else {
            return;
        };
        let at = self.parsed.at;
        self.push(call_ended(at, id, ok, text));
    }

    /// A `ConversationErrorEvent`: the model service's exception (`code`), as LiteLLM names
    /// it, and its words (`detail`).
    fn failure(&self, value: &Value) -> Failure {
        let code = value["code"].as_str().unwrap_or_default();
        let detail = value["detail"].as_str().unwrap_or(code);
        let kind = match code {
            "AuthenticationError" | "PermissionDeniedError" => FailureKind::SignIn,
            "RateLimitError" => FailureKind::RateLimit,
            "ContextWindowExceededError" => FailureKind::ContextFull,
            _ => FailureKind::Other,
        };
        Failure {
            kind,
            line: detail.lines().next().unwrap_or(code).to_owned(),
            service: detail.starts_with("litellm."),
            http_status: None,
            code: (!code.is_empty()).then(|| code.to_owned()),
            retry_at: None,
            provider: None,
            model: self.facts.model.clone(),
            thinking: self.facts.thinking.clone(),
        }
    }
}

fn other(line: &str) -> Failure {
    Failure {
        kind: FailureKind::Other,
        line: line.to_owned(),
        service: false,
        http_status: None,
        code: None,
        retry_at: None,
        provider: None,
        model: None,
        thinking: None,
    }
}

/// An action's input: the arguments the model wrote, else the action OpenHands made of
/// them.
pub(crate) fn input(value: &Value) -> Value {
    value["tool_call"]["arguments"]
        .as_str()
        .and_then(|arguments| serde_json::from_str(arguments).ok())
        .unwrap_or_else(|| value["action"].clone())
}

/// The end of call `id`, with what it gave back.
pub(crate) fn call_ended(at: u64, id: &str, ok: bool, text: &str) -> Event {
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    Event::CallEnded {
        at,
        id: id.to_owned(),
        ok,
        chars: text.chars().count() as u64,
        tail: (!tail.is_empty()).then_some(tail),
        waited: Vec::new(),
    }
}

/// A call to `tool`, of an action of `kind`. An MCP tool is named as its server names it
/// where one server is loaded, else `<server>_<tool>`.
pub(crate) fn call(
    id: &str,
    tool: &str,
    input: Value,
    kind: Option<&str>,
    response: Option<&str>,
    servers: &[String],
) -> Call {
    let (server, steery) = if kind == Some(MCP_ACTION) {
        let prefixed = servers.iter().find_map(|server| {
            let rest = tool.strip_prefix(server.as_str())?.strip_prefix('_')?;
            Some((server.clone(), rest))
        });
        match (prefixed, servers) {
            (Some((server, rest)), _) => {
                let steery = (server == STEERY).then(|| rest.to_owned());
                (Some(server), steery)
            }
            (None, [only]) => {
                let steery = (only == STEERY).then(|| tool.to_owned());
                (Some(only.clone()), steery)
            }
            (None, _) => (None, None),
        }
    } else {
        (None, None)
    };
    let group = match (kind, &steery) {
        (_, Some(_)) => Group::Steery,
        (Some(MCP_ACTION), None) => Group::Mcp,
        _ => group(tool),
    };
    Call {
        id: id.to_owned(),
        tool: tool.to_owned(),
        input,
        group,
        server,
        steery,
        asks: None,
        parent: None,
        response: response.map(str::to_owned),
    }
}

/// OpenHands' own tools, by the names its 1.16.0 tools take.
fn group(tool: &str) -> Group {
    match tool {
        "terminal" => Group::Shell,
        "read_file" | "list_directory" => Group::FilesRead,
        "file_editor" | "planning_file_editor" | "edit" | "write_file" | "apply_patch" => {
            Group::FilesEdit
        }
        "glob" | "grep" => Group::Search,
        "task" | "delegate" => Group::Agents,
        "task_tracker" | "think" => Group::Plan,
        "finish" | "invoke_skill" => Group::Harness,
        tool if tool.starts_with("browser_") => Group::Browser,
        _ => Group::Custom,
    }
}

/// The text blocks of a content list, joined.
pub(crate) fn texts(content: &Value) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// Milliseconds since 1970 of the times OpenHands writes: the machine's local time, with no
/// offset (`datetime.now().isoformat()`).
pub(crate) fn millis(at: &str) -> Option<u64> {
    let naive = chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    use chrono::{Datelike, Timelike};
    // SAFETY: `tm` is plain data, and `mktime` reads and normalizes only it.
    let seconds = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = naive.year() - 1900;
        tm.tm_mon = naive.month0() as i32;
        tm.tm_mday = naive.day() as i32;
        tm.tm_hour = naive.hour() as i32;
        tm.tm_min = naive.minute() as i32;
        tm.tm_sec = naive.second() as i32;
        tm.tm_isdst = -1;
        libc::mktime(&mut tm)
    };
    let millis = seconds * 1_000 + i64::from(naive.nanosecond() / 1_000_000);
    u64::try_from(millis).ok()
}
