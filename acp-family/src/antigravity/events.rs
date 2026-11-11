//! One Antigravity transcript, `brain/<id>/.system_generated/logs/transcript_full.jsonl`,
//! read as Steery's events. Each line is one step of the conversation, written once it is
//! done and never again: `step_index`, `source` (`USER_EXPLICIT`, `MODEL`, `SYSTEM`,
//! `SYSTEM_SDK`), `type`, `status`, `created_at` (whole seconds), and the step's own
//! fields:
//!
//! - `USER_INPUT`: `content`, the person's prompt inside `<USER_REQUEST>`, then metadata.
//! - `PLANNER_RESPONSE`: the model's `thinking`, its `content` and `tool_calls`
//!   (`[{name, args}]`, no ids), and `input_tokens` (the cached part counted in it),
//!   `cache_read_tokens` and `output_tokens`.
//! - `GENERIC`: a call's result, one step per call in the order the response named them;
//!   `status` `ERROR` with its `error` where the call failed.
//! - `ERROR_MESSAGE`: the model service's `error`, once per attempt.
//! - `SYSTEM_MESSAGE`: what a hook, a sub-agent or Antigravity itself put in the context.
//!
//! The file appends in place as a conversation goes on, a resume too. The transcript gives
//! a call no id, and a hook's `toolCall.id` is not in it; both name the step of the
//! call's result (the hook's `stepIdx`), so that step's index is the call's id.
//!
//! No step says that a turn ended: a response without calls may be followed by a `Stop`
//! hook's `continue` and another response, and the terminal holds the conversation open
//! between a person's prompts. A turn ends where the next prompt begins; where
//! Antigravity no longer runs the conversation ([`super::store`]), with its newest step;
//! and where its newest step is an answer and the file stood still for [`SETTLE`] since.

use std::fs::File;
use std::io::{BufReader, Read as _, Seek as _, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_driver_common::lines::{self, BATCH, Line, RECORD};
use steery_sdk::wire::harness::{
    Call, Count, Event, Events, Failure, FailureKind, Group, TurnState, Voice,
};
use steery_sdk::{Error, refusal};

/// How long the file stands still after an answer, in a conversation Antigravity still
/// runs, before that answer is its turn's end.
pub const SETTLE: Duration = Duration::from_secs(60);

/// The tool that starts a sub-agent.
pub(super) const INVOKE_SUBAGENT: &str = "invoke_subagent";
/// The tool that calls an MCP server's tool that was not loaded up front.
const CALL_MCP_TOOL: &str = "call_mcp_tool";

/// Where a read stands: the file it was, by device and inode, the offset after the last
/// whole line read, and what the lines so far carry on.
#[derive(Serialize, Deserialize)]
struct Cursor {
    v: u8,
    dev: u64,
    ino: u64,
    off: u64,
    #[serde(default)]
    st: State,
}

impl Cursor {
    fn parse(cursor: &str) -> Result<Self, Error> {
        serde_json::from_str::<Self>(cursor)
            .ok()
            .filter(|cursor| cursor.v == 1)
            .ok_or_else(|| {
                Error::invalid(format!("this driver did not give the cursor `{cursor}`"))
            })
    }

    fn said(&self) -> String {
        serde_json::to_string(self).expect("a cursor is JSON")
    }
}

/// What the steps read so far carry on to the next.
#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    /// The turn that began and did not end, by the step that began it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    open: Option<String>,
    /// The calls of the newest response whose results have not come, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    calls: Vec<Pending>,
    /// What the newest step that matters to a turn's end was.
    #[serde(default)]
    last: Last,
    /// When the newest step was written.
    #[serde(default)]
    at: u64,
}

/// A call waiting for its result.
#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    id: String,
    tool: String,
}

/// The newest step, as a turn would end with it.
#[derive(Clone, Default, Serialize, Deserialize)]
enum Last {
    /// A response with no calls: the turn finished with it.
    Answer,
    /// The model service's error.
    Error(String),
    #[default]
    Other,
}

/// The events of `path` after `cursor`, or of its newest `tail` bytes. `running` says
/// whether Antigravity still runs the conversation: where it does not, a turn left open
/// at the end of the file ended.
pub fn read(
    path: &Path,
    cursor: Option<&str>,
    tail: Option<u64>,
    running: bool,
) -> Result<Events, Error> {
    let unreadable = |e: std::io::Error| {
        Error::refused(
            refusal::REFUSED,
            format!("cannot read {}: {e}", path.display()),
        )
    };
    let open = File::open(path).map_err(unreadable)?;
    let meta = open.metadata().map_err(unreadable)?;
    let (dev, ino) = identity(&meta);
    let length = meta.len();
    let mut reset = false;
    let (start, state) = match (tail, cursor) {
        (Some(tail), _) => {
            let from = length.saturating_sub(tail.min(BATCH));
            (
                lines::first_whole(&open, from, length).map_err(unreadable)?,
                State::default(),
            )
        }
        (None, Some(cursor)) => {
            let cursor = Cursor::parse(cursor)?;
            if cursor.dev == dev && cursor.ino == ino && cursor.off <= length {
                (cursor.off, cursor.st)
            } else {
                reset = true;
                (0, State::default())
            }
        }
        (None, None) => (0, State::default()),
    };
    let mut fold = Fold {
        state,
        events: Vec::new(),
    };
    let mut at = start;
    let mut more = false;
    (&open).seek(SeekFrom::Start(start)).map_err(unreadable)?;
    let total = length - start;
    let mut reader = BufReader::new((&open).take(total));
    let mut line = Vec::new();
    loop {
        if at - start >= BATCH {
            more = true;
            break;
        }
        let kind = lines::capped(&mut reader, &mut line, RECORD).map_err(unreadable)?;
        let end = start + lines::consumed(&reader, total);
        match kind {
            Line::Partial => break,
            Line::Long => {}
            Line::Whole => {
                if let Ok(step) = serde_json::from_slice::<Value>(&line) {
                    fold.step(&step);
                }
            }
        }
        at = end;
    }
    let still = meta
        .modified()
        .ok()
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .is_some_and(|still| still >= SETTLE);
    if !more && at == length && (!running || still && matches!(fold.state.last, Last::Answer)) {
        fold.settle();
    }
    Ok(Events {
        cursor: Cursor {
            v: 1,
            dev,
            ino,
            off: at,
            st: fold.state,
        }
        .said(),
        events: fold.events,
        more,
        reset,
        whole: start == 0,
        after: None,
    })
}

#[cfg(unix)]
fn identity(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt as _;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn identity(_: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// The steps of one read, folded into events.
struct Fold {
    state: State,
    events: Vec<Event>,
}

impl Fold {
    fn step(&mut self, step: &Value) {
        let Some(index) = step["step_index"].as_u64() else {
            return;
        };
        let id = index.to_string();
        let at = step["created_at"].as_str().and_then(millis).unwrap_or(0);
        self.state.at = at;
        match step["type"].as_str().unwrap_or_default() {
            "USER_INPUT" => self.prompt(id, at, step),
            "PLANNER_RESPONSE" => self.response(id, at, step),
            "GENERIC" if step["source"] == "MODEL" => self.result(at, step),
            "ERROR_MESSAGE" => {
                self.begin(&id, at);
                let error = step["error"].as_str().unwrap_or_default();
                self.state.last = Last::Error(first_line(error).to_owned());
            }
            // What a hook or Antigravity put in the context, and a sub-agent's task.
            _ => {}
        }
    }

    /// A person's prompt: it ends the turn before it, which no `Stop` closed, and begins one.
    fn prompt(&mut self, id: String, at: u64, step: &Value) {
        self.settle();
        self.begin(&id, at);
        let text = prompt(step["content"].as_str().unwrap_or_default());
        if !text.is_empty() {
            self.events.push(Event::Words {
                at,
                id,
                voice: Voice::Person,
                text: text.to_owned(),
                message: None,
                call: None,
                clips: Vec::new(),
            });
        }
        self.state.last = Last::Other;
    }

    /// The turn that `id` is in, begun where none is open: a sub-agent's run has no
    /// prompt of a person's, and its first step begins it.
    fn begin(&mut self, id: &str, at: u64) {
        if self.state.open.is_none() {
            self.state.open = Some(id.to_owned());
            self.events.push(Event::TurnBegan {
                at,
                turn: Some(id.to_owned()),
            });
        }
    }

    fn response(&mut self, id: String, at: u64, step: &Value) {
        self.begin(&id, at);
        let said = |key: &str| {
            step[key]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
        };
        if let Some(thinking) = said("thinking") {
            self.events.push(Event::Words {
                at,
                id: format!("{id}.thinking"),
                voice: Voice::Thought,
                text: thinking.to_owned(),
                message: Some(id.clone()),
                call: None,
                clips: Vec::new(),
            });
        }
        if let Some(text) = said("content") {
            self.events.push(Event::Words {
                at,
                id: id.clone(),
                voice: Voice::Agent,
                text: text.to_owned(),
                message: Some(id.clone()),
                call: None,
                clips: Vec::new(),
            });
        }
        let count = |key: &str| step[key].as_u64().unwrap_or(0);
        if step.get("input_tokens").is_some() {
            self.events.push(Event::Tokens {
                at,
                count: Count::Response,
                response: Some(id.clone()),
                model: None,
                input: count("input_tokens"),
                cached: count("cache_read_tokens"),
                cache_write: None,
                output: count("output_tokens"),
                reasoning: 0,
                context_left: None,
            });
        }
        let calls = step["tool_calls"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let first = step["step_index"].as_u64().unwrap_or_default() + 1;
        for (place, recorded) in (first..).zip(calls) {
            let tool = recorded["name"].as_str().unwrap_or_default();
            let call = call(
                &place.to_string(),
                tool,
                recorded.get("args").cloned().unwrap_or(Value::Null),
            );
            self.state.calls.push(Pending {
                id: call.id.clone(),
                tool: tool.to_owned(),
            });
            self.events.push(Event::CallBegan { at, call });
        }
        self.state.last = if calls.is_empty() {
            Last::Answer
        } else {
            Last::Other
        };
    }

    /// A call's result: its own step, whose index is the call's id.
    fn result(&mut self, at: u64, step: &Value) {
        let id = step["step_index"].as_u64().unwrap_or_default().to_string();
        let Some(place) = self.state.calls.iter().position(|call| call.id == id) else {
            return;
        };
        let pending = self.state.calls.remove(place);
        let ok = step["status"] != "ERROR";
        let text = result(step);
        if pending.tool == INVOKE_SUBAGENT
            && let Some(child) = spawned(&text)
        {
            self.events.push(Event::SubAgent {
                at,
                external_id: child.to_owned(),
                call: Some(pending.id.clone()),
                state: None,
                error: None,
            });
        }
        if !text.is_empty() {
            self.events.push(Event::Words {
                at,
                id: format!("{}.result", pending.id),
                voice: Voice::Tool,
                text: text.clone(),
                message: None,
                call: Some(pending.id.clone()),
                clips: Vec::new(),
            });
        }
        self.events.push(Event::CallEnded {
            at,
            id: pending.id,
            ok,
            chars: text.chars().count() as u64,
            tail: tail(&text),
            waited: Vec::new(),
        });
        self.state.last = Last::Other;
    }

    /// The open turn ends with the newest step: finished on an answer, failed on the
    /// service's error, interrupted on anything else (a call that never returned, a
    /// process stopped mid-step).
    fn settle(&mut self) {
        let Some(turn) = self.state.open.take() else {
            return;
        };
        let (state, failure) = match std::mem::take(&mut self.state.last) {
            Last::Answer => (TurnState::Finished, None),
            Last::Error(line) => (TurnState::Failed, Some(failure(line))),
            Last::Other => (TurnState::Interrupted, None),
        };
        self.state.calls.clear();
        self.events.push(Event::TurnEnded {
            at: self.state.at,
            turn: Some(turn),
            state,
            failure,
        });
    }
}

/// The prompt a person wrote: the text inside `<USER_REQUEST>`, else the whole content.
fn prompt(content: &str) -> &str {
    content
        .split_once("<USER_REQUEST>")
        .and_then(|(_, rest)| rest.split_once("</USER_REQUEST>"))
        .map_or(content, |(request, _)| request)
        .trim()
}

/// The first line of a person's prompt, as a title.
pub(super) fn title(content: &str) -> Option<String> {
    prompt(content)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// The text of a call's result, without the `Created At:` and `Completed At:` lines that
/// Antigravity writes before it.
fn result(step: &Value) -> String {
    let content = step["content"].as_str().unwrap_or_default();
    let text: Vec<&str> = content
        .lines()
        .skip_while(|line| line.starts_with("Created At: ") || line.starts_with("Completed At: "))
        .collect();
    let text = text.join("\n");
    if text.trim().is_empty() {
        step["error"].as_str().unwrap_or_default().to_owned()
    } else {
        text
    }
}

/// The id of the conversation an `invoke_subagent` result says it created:
/// `"conversationId": "<id>"`.
fn spawned(text: &str) -> Option<&str> {
    let (_, rest) = text.split_once("\"conversationId\"")?;
    let rest = rest
        .trim_start()
        .strip_prefix(':')?
        .trim_start()
        .strip_prefix('"')?;
    let (id, _) = rest.split_once('"')?;
    (!id.is_empty()).then_some(id)
}

/// The service's error, as Antigravity wrote it: `API error (attempt N): Error 429,
/// Message: ..., Status: RESOURCE_EXHAUSTED, ...`.
pub(super) fn failure(line: String) -> Failure {
    let http_status = line
        .split_once("Error ")
        .and_then(|(_, rest)| rest.get(..3))
        .and_then(|code| code.parse::<u16>().ok());
    let code = line
        .split_once("Status: ")
        .and_then(|(_, rest)| rest.split([',', ' ']).next())
        .filter(|code| !code.is_empty())
        .map(str::to_owned);
    let kind = match (http_status, code.as_deref()) {
        (Some(429), _) | (_, Some("RESOURCE_EXHAUSTED")) => FailureKind::RateLimit,
        (Some(401 | 403), _) | (_, Some("UNAUTHENTICATED" | "PERMISSION_DENIED")) => {
            FailureKind::SignIn
        }
        _ => FailureKind::Other,
    };
    Failure {
        kind,
        service: http_status.is_some(),
        http_status,
        code,
        retry_at: None,
        provider: None,
        model: None,
        thinking: None,
        line,
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default().trim()
}

/// The call named `name`, with its group. Antigravity names a loaded MCP tool
/// `mcp_<server>_<tool>`, and calls one it did not load through `call_mcp_tool` with its
/// `ServerName` and `ToolName`.
pub(super) fn call(id: &str, name: &str, input: Value) -> Call {
    let served = if name == CALL_MCP_TOOL {
        input["ServerName"]
            .as_str()
            .zip(input["ToolName"].as_str())
            .map(|(server, tool)| (server.to_owned(), tool.to_owned()))
    } else {
        name.strip_prefix("mcp_")
            .and_then(|rest| rest.split_once('_'))
            .map(|(server, tool)| (server.to_owned(), tool.to_owned()))
    };
    let (tool, group, server, steery) = match served {
        Some((server, tool)) if server == "steery" => (
            format!("mcp__steery__{tool}"),
            Group::Steery,
            None,
            Some(tool),
        ),
        Some((server, tool)) => (
            format!("mcp__{server}__{tool}"),
            Group::Mcp,
            Some(server),
            None,
        ),
        None => (name.to_owned(), group(name), None, None),
    };
    Call {
        id: id.to_owned(),
        tool,
        input,
        group,
        server,
        steery,
        asks: None,
        parent: None,
        response: None,
    }
}

/// The group of one of Antigravity's own tools, as 1.5.5's stream-json `init` lists them.
fn group(name: &str) -> Group {
    match name {
        "run_command" | "command_status" | "send_command_input" => Group::Shell,
        "view_file" | "read_file" | "list_dir" | "find_by_name" => Group::FilesRead,
        "write_to_file"
        | "replace_file_content"
        | "multi_replace_file_content"
        | "sed_file"
        | "notebook_edit" => Group::FilesEdit,
        "grep_search" => Group::Search,
        "search_web" | "read_url_content" => Group::Web,
        "invoke_subagent" | "define_subagent" | "manage_subagents" | "send_message" => {
            Group::Agents
        }
        "ask_question" | "ask_permission" | "ask_custom_permission" => Group::Ask,
        "delete_knowledge" => Group::Memory,
        "generate_image" => Group::Media,
        "notebook_execution" => Group::Code,
        "manage_task" | "schedule" | "run_workflow" => Group::Plan,
        "list_resources"
        | "read_resource"
        | "list_permissions"
        | "list_plugin_accounts"
        | "search_marketplace"
        | "manage_inbox"
        | "wait"
        | "wait_5_seconds"
        | "finish" => Group::Harness,
        name if name.starts_with("browser_")
            || name.contains("_browser_")
            || name.ends_with("_browser_pixel") =>
        {
            Group::Browser
        }
        _ => Group::Custom,
    }
}

/// The last 8 lines of a result.
fn tail(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let kept = lines[lines.len().saturating_sub(8)..].join("\n");
    (!kept.trim().is_empty()).then_some(kept)
}

/// Milliseconds since 1970 of an RFC 3339 time.
pub fn millis(at: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}

/// Milliseconds since 1970 of a file's last change.
pub fn changed(path: &Path) -> Option<u64> {
    let at = std::fs::metadata(path).ok()?.modified().ok()?;
    u64::try_from(at.duration_since(UNIX_EPOCH).ok()?.as_millis()).ok()
}
