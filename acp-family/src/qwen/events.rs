//! One Qwen chat file, `<id>.jsonl`, read as Steery's events.
//!
//! Qwen appends one JSON record a line, each naming the record before it in the
//! conversation by `uuid` and `parentUuid`: a tree, whose path from the last record back
//! to a root is the conversation (`steery_driver_common::tree`). A rewind writes on from
//! an earlier record and leaves the other branch in the file; a resume (`-r`) appends to
//! the same file. Records:
//! - `user`: a person's prompt (`provenance: real_user`), its `message.parts`;
//! - `assistant`: one response, its thoughts (`thought: true`), text and `functionCall`s,
//!   with `usageMetadata`;
//! - `tool_result`: a call's result, `toolCallResult` and its `functionResponse`;
//! - `system`: Qwen's own, by `subtype`: a compression (`chat_compression`), its
//!   telemetry (`ui_telemetry`, which carries a request's error), and snapshots.
//!
//! The file says no turn's end (Qwen 0.27.2 writes no `turn_result` in a terminal, `-p`
//! or `--acp` run). A turn ends `finished` at a response with no calls, `failed` at an
//! error of the conversation's own request, and `interrupted` where a prompt follows a
//! turn that did not end.

use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde_json::Value;
use steery_driver_common::tree::{Cursor, Parent, Shape, Tree, Trees};
use steery_sdk::wire::harness::{
    Call, Clip, Compactor, Count, Event, Events, Failure, FailureKind, Group, TurnState, Voice,
};
use steery_sdk::{Error, refusal};

/// `1.<device>.<inode>.<tip>.<at>.<aside>.<open>` (`steery_driver_common::tree::Cursor`):
/// `open` is 1 where a turn began and did not end.
const CURSOR: &str = "1";

/// What separates a conversation's id from its prompt's count in a `prompt_id` of its own;
/// a sub-agent's or Qwen's background work's is `<id>#<agent>#<n>`.
const OWN_PROMPT: &str = "########";

/// How a Qwen chat file's records make its tree: by `uuid` and `parentUuid`.
#[derive(Default)]
pub struct Records;

#[derive(Deserialize)]
struct Head {
    uuid: Option<String>,
    #[serde(rename = "parentUuid")]
    parent: Option<String>,
}

impl Shape for Records {
    fn read(&mut self, tree: &mut Tree, line: &[u8], start: u64, end: u64) -> Result<(), String> {
        let Ok(head) = serde_json::from_slice::<Head>(line) else {
            return Ok(());
        };
        let Some(id) = head.uuid.filter(|id| !id.is_empty()) else {
            return Ok(());
        };
        let parent = head.parent.as_deref().map_or(Parent::Root, Parent::Named);
        let at = tree.add(start, end, parent).ok_or("full")?;
        tree.name(&id, at);
        Ok(())
    }
}

/// The events of `file`'s conversation after `cursor`, or of its records within `tail`
/// bytes of its end. A cursor whose record is no longer on the path (a rewind) reads again
/// from the beginning, and says so.
pub fn read(
    trees: &Trees<Records>,
    file: &Path,
    cursor: Option<&str>,
    tail: Option<u64>,
) -> Result<Events, Error> {
    let given = cursor.map(parse_cursor).transpose()?;
    trees
        .walk(file, Records, |walk| {
            let path = walk.tree.path()?;
            let read = walk.read(&path, given.as_ref(), tail)?;
            let mut fold = Fold {
                open: read.mark == Some(1),
                ..Fold::default()
            };
            for record in &read.records {
                if let Some(value) = &record.value {
                    fold.record(value);
                }
            }
            Ok(Events {
                cursor: walk
                    .cursor(
                        &path,
                        read.from + read.records.len(),
                        read.through,
                        u64::from(fold.open),
                    )
                    .said(CURSOR),
                events: fold.events,
                more: read.more,
                reset: read.reset,
                whole: read.from == 0,
                after: None,
            })
        })
        .map_err(|why| Error::refused(refusal::REFUSED, why))
}

fn parse_cursor(cursor: &str) -> Result<Cursor, Error> {
    Cursor::parse(CURSOR, cursor)
        .filter(|cursor| cursor.mark <= 1)
        .ok_or_else(|| Error::invalid(format!("this driver did not give the cursor `{cursor}`")))
}

/// The records of one read, folded into events.
#[derive(Default)]
struct Fold {
    events: Vec<Event>,
    /// A turn began and did not end.
    open: bool,
    /// The prompt whose turn is open, where this read saw it.
    turn: Option<String>,
    /// The model of the last response this read saw.
    model: Option<String>,
}

impl Fold {
    fn record(&mut self, record: &Value) {
        let id = record["uuid"].as_str().unwrap_or_default();
        let at = record["timestamp"].as_str().and_then(millis).unwrap_or(0);
        match record["type"].as_str() {
            Some("user") => self.user(id, at, record),
            Some("assistant") => self.assistant(id, at, record),
            Some("tool_result") => self.result(at, record),
            Some("system") => self.system(id, at, record),
            _ => {}
        }
    }

    fn user(&mut self, id: &str, at: u64, record: &Value) {
        let Some((said, clips)) = person(record) else {
            return;
        };
        if self.open {
            self.end(at, TurnState::Interrupted, None);
        }
        self.begin(at, id);
        self.events.push(Event::Words {
            at,
            id: id.to_owned(),
            voice: Voice::Person,
            text: said,
            message: None,
            call: None,
            clips,
        });
    }

    fn assistant(&mut self, id: &str, at: u64, record: &Value) {
        // A response after its turn's end was said opens a turn of its own.
        if !self.open {
            self.begin(at, id);
        }
        let model = record["model"].as_str().filter(|model| !model.is_empty());
        if let Some(model) = model
            && self.model.as_deref() != Some(model)
        {
            self.model = Some(model.to_owned());
            self.events.push(Event::Model {
                at,
                model: model.to_owned(),
                provider: None,
                thinking: None,
            });
        }
        let parts = parts(record);
        let mut text = String::new();
        let mut calls = 0;
        for (index, part) in parts.iter().enumerate() {
            if let Some(call) = part.get("functionCall") {
                calls += 1;
                let name = call["name"].as_str().unwrap_or_default();
                let call_id = call["id"]
                    .as_str()
                    .map_or_else(|| format!("{id}#call-{index}"), str::to_owned);
                self.events.push(Event::CallBegan {
                    at,
                    call: self::call(&call_id, name, call["args"].clone()),
                });
            } else if let Some(said) = part["text"].as_str() {
                if part["thought"] == true {
                    if !said.trim().is_empty() {
                        self.events.push(Event::Words {
                            at,
                            id: format!("{id}#thought-{index}"),
                            voice: Voice::Thought,
                            text: said.trim().to_owned(),
                            message: None,
                            call: None,
                            clips: Vec::new(),
                        });
                    }
                } else {
                    text.push_str(said);
                }
            }
        }
        if !text.trim().is_empty() {
            self.events.push(Event::Words {
                at,
                id: id.to_owned(),
                voice: Voice::Agent,
                text: text.trim().to_owned(),
                message: None,
                call: None,
                clips: Vec::new(),
            });
        }
        let usage = &record["usageMetadata"];
        if usage.is_object() {
            let count = |key: &str| usage[key].as_u64().unwrap_or(0);
            let input = count("promptTokenCount");
            let window = record["contextWindowSize"].as_u64().unwrap_or(0);
            // Qwen counts an OpenAI-shaped provider's `completion_tokens`, thinking with
            // them, as `candidatesTokenCount` [INFERENCE from a qwen3 run through Ollama,
            // whose thinking and candidates counts were equal].
            self.events.push(Event::Tokens {
                at,
                count: Count::Response,
                response: Some(id.to_owned()),
                model: model.map(str::to_owned),
                input,
                cached: count("cachedContentTokenCount"),
                cache_write: None,
                output: count("candidatesTokenCount"),
                reasoning: count("thoughtsTokenCount"),
                context_left: (window > 0)
                    .then(|| (1.0 - input as f64 / window as f64).clamp(0.0, 1.0)),
            });
        }
        if calls == 0 && !text.trim().is_empty() {
            self.end(at, TurnState::Finished, None);
        }
    }

    /// A call's result: Qwen writes it once the call is over.
    fn result(&mut self, at: u64, record: &Value) {
        let result = &record["toolCallResult"];
        let response = parts(record)
            .iter()
            .find_map(|part| part.get("functionResponse").cloned());
        let Some(id) = result["callId"]
            .as_str()
            .or_else(|| {
                response
                    .as_ref()
                    .and_then(|response| response["id"].as_str())
            })
            .map(str::to_owned)
        else {
            return;
        };
        let output = response
            .as_ref()
            .map(|response| output(&response["response"]))
            .unwrap_or_default();
        self.events.push(Event::CallEnded {
            at,
            id: id.clone(),
            ok: result["status"] == "success",
            chars: output.chars().count() as u64,
            tail: tail(&output),
            waited: Vec::new(),
        });
        if !output.trim().is_empty() {
            self.events.push(Event::Words {
                at,
                id: format!("{id}#result"),
                voice: Voice::Tool,
                text: output,
                message: None,
                call: Some(id),
                clips: Vec::new(),
            });
        }
    }

    fn system(&mut self, id: &str, at: u64, record: &Value) {
        let payload = &record["systemPayload"];
        match record["subtype"].as_str() {
            Some("chat_compression") => {
                let info = &payload["info"];
                let by = match info["triggerReason"].as_str() {
                    Some("manual") => Compactor::Person,
                    reason => Compactor::Harness {
                        reason: reason.map(str::to_owned),
                    },
                };
                self.events.push(Event::Compacted {
                    at,
                    id: id.to_owned(),
                    by,
                    tokens_before: info["originalTokenCount"].as_u64(),
                    tokens_after: info["newTokenCount"].as_u64(),
                });
            }
            Some("ui_telemetry") => {
                let event = &payload["uiEvent"];
                let own = event["prompt_id"]
                    .as_str()
                    .is_some_and(|prompt| prompt.contains(OWN_PROMPT));
                if self.open && own && event["event.name"] == "qwen-code.api_error" {
                    let line = event["error_message"]
                        .as_str()
                        .unwrap_or("the model's provider answered with an error")
                        .to_owned();
                    let failure = failure(line, event);
                    self.end(at, TurnState::Failed, Some(failure));
                }
            }
            _ => {}
        }
    }

    fn begin(&mut self, at: u64, id: &str) {
        self.open = true;
        self.turn = Some(id.to_owned());
        self.events.push(Event::TurnBegan {
            at,
            turn: Some(id.to_owned()),
        });
    }

    fn end(&mut self, at: u64, state: TurnState, failure: Option<Failure>) {
        self.open = false;
        self.events.push(Event::TurnEnded {
            at,
            turn: self.turn.take(),
            state,
            failure,
        });
    }
}

/// What a person said in a `user` record, and what it carried inline; none where the
/// record is not a person's prompt.
fn person(record: &Value) -> Option<(String, Vec<Clip>)> {
    if record["provenance"]
        .as_str()
        .is_some_and(|provenance| provenance != "real_user")
    {
        return None;
    }
    let parts = parts(record);
    if parts
        .iter()
        .any(|part| part.get("functionResponse").is_some())
    {
        return None;
    }
    let said: String = parts
        .iter()
        .filter(|part| part["thought"] != true)
        .filter_map(|part| part["text"].as_str())
        .collect();
    let clips: Vec<Clip> = parts.iter().filter_map(clip).collect();
    let said = said.trim();
    (!said.is_empty() || !clips.is_empty()).then(|| (said.to_owned(), clips))
}

/// The parts of a record's message.
fn parts(record: &Value) -> &[Value] {
    record["message"]["parts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// An image, audio or file that a person's message carried inline.
fn clip(part: &Value) -> Option<Clip> {
    let inline = part.get("inlineData")?;
    Some(Clip::Bytes {
        data: inline["data"].as_str()?.to_owned(),
        media_type: inline["mimeType"].as_str().map(str::to_owned),
        name: inline["displayName"].as_str().map(str::to_owned),
    })
}

/// The text of a call's result, as the model was given it.
fn output(response: &Value) -> String {
    response["output"]
        .as_str()
        .or_else(|| response["error"].as_str())
        .or_else(|| response["error"]["message"].as_str())
        .map(str::to_owned)
        .or_else(|| {
            (!response.is_null())
                .then(|| response.to_string())
                .filter(|text| text != "{}")
        })
        .unwrap_or_default()
}

/// A request's error, as Qwen's telemetry says it: its HTTP status and its error's type.
fn failure(line: String, event: &Value) -> Failure {
    let http_status = event["status_code"]
        .as_u64()
        .and_then(|status| u16::try_from(status).ok());
    let code = event["error_type"].as_str().map(str::to_owned);
    let kind = match (http_status, code.as_deref()) {
        (Some(429), _) | (_, Some("RateLimitError")) => FailureKind::RateLimit,
        (Some(401 | 403), _) | (_, Some("AuthenticationError" | "PermissionDeniedError")) => {
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
        provider: event["auth_type"].as_str().map(str::to_owned),
        model: event["model"].as_str().map(str::to_owned),
        thinking: None,
        line,
    }
}

/// The call named `name`, with its group. Qwen names an MCP tool `mcp__<server>__<tool>`.
pub(super) fn call(id: &str, name: &str, input: Value) -> Call {
    let served = name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"));
    let (group, server, steery) = match served {
        Some(("steery", tool)) => (Group::Steery, None, Some(tool.to_owned())),
        Some((server, _)) => (Group::Mcp, Some(server.to_owned()), None),
        None => (group(name), None, None),
    };
    Call {
        id: id.to_owned(),
        tool: name.to_owned(),
        input,
        group,
        server,
        steery,
        asks: None,
        parent: None,
        response: None,
    }
}

/// The group of one of Qwen's own tools (`TOOL_NAME_ALIASES`).
fn group(name: &str) -> Group {
    match name {
        "run_shell_command" | "exec" | "monitor" => Group::Shell,
        "read_file" | "list_directory" | "glob" | "zoom_image" => Group::FilesRead,
        "edit" | "write_file" | "notebook_edit" => Group::FilesEdit,
        "grep_search" | "tool_search" => Group::Search,
        "web_fetch" | "web_search" => Group::Web,
        "lsp" => Group::Code,
        "agent" | "send_message" | "list_agents" | "create_sub_session" | "team_create"
        | "team_delete" | "team_plan_approval" | "request_shutdown" | "task_stop" => Group::Agents,
        "todo_write" | "enter_plan_mode" | "exit_plan_mode" | "task_create" | "task_list"
        | "task_update" | "get_goal" | "propose_goal" | "update_goal" | "cron_create"
        | "cron_delete" | "cron_list" | "loop_wakeup" => Group::Plan,
        "ask_user_question" => Group::Ask,
        "save_memory" | "search_memory" | "manage_memory" => Group::Memory,
        "skill" | "read_mcp_resource" | "tool_call" | "workflow" | "structured_output" => {
            Group::Harness
        }
        "image_gen" | "display_image" => Group::Media,
        name if name.starts_with("omni_") => Group::Media,
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
