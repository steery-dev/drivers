//! OpenCode's message rows, read as Steery's events. A row of `session_message` is one
//! message: `user`, `assistant`, `idle`, `compaction`, `shell` and a few the person never
//! reads. An `idle` row ends the turn that the prompts before it began. OpenCode 1 wrote
//! no `idle` rows, so a turn that OpenCode 2 imported from it ends at its last reply.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    Call, Clip, Compactor, Count, Event, Execution, Failure, FailureKind, Group, TurnState, Voice,
};

/// One message row, its assistant parts already in `data.content`.
pub(crate) struct Row {
    pub seq: i64,
    pub id: String,
    pub kind: String,
    pub created: u64,
    pub data: Value,
    /// Its attachments' bytes from `blob`, base64, by the hash of their `media:` ref.
    pub blobs: Vec<(String, String)>,
    /// A row that OpenCode 2's first run copied from OpenCode 1's `message` table.
    pub imported: bool,
}

impl Row {
    /// A row whose message can still change: an assistant reply, a compaction or a shell
    /// command that OpenCode has not finished writing.
    pub fn open(&self) -> bool {
        let data = &self.data;
        match self.kind.as_str() {
            "assistant" => data["time"]["completed"].is_null() && data["error"].is_null(),
            "compaction" => data["status"] == "running",
            "shell" => data["time"]["completed"].is_null() && data["status"] == "running",
            _ => false,
        }
    }

    /// A row a fork copied from its parent: OpenCode names the copy `<new id>_<seq>`.
    pub fn copied(&self) -> bool {
        self.id
            .strip_prefix("msg_")
            .and_then(|rest| rest.rsplit_once('_'))
            .is_some_and(|(_, seq)| seq == self.seq.to_string())
    }
}

/// What the rows read so far carry on to the next read.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct State {
    /// A turn began and no `idle` row ended it.
    #[serde(default)]
    pub open: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The newest failure of the open turn's replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    /// Copied rows of a fork were read, and the fork was said.
    #[serde(default)]
    pub inherited: bool,
    #[serde(default)]
    pub forked: bool,
    /// When the last reply of an open imported turn completed: where that turn ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_end: Option<u64>,
}

/// Rows folded into events.
pub(crate) struct Fold<'a> {
    /// The conversation this one was forked from, and when the fork began.
    pub fork: Option<(&'a str, u64)>,
    pub state: State,
    pub events: Vec<Event>,
    /// Steery's names of the MCP servers OpenCode loads, to tell an MCP tool by its name.
    pub servers: &'a [String],
}

impl Fold<'_> {
    pub fn row(&mut self, row: &Row) {
        if let Some((parent, at)) = self.fork {
            if row.copied() {
                self.state.inherited = true;
            } else {
                self.forked(parent, at);
            }
        }
        if !row.imported || row.kind == "user" {
            self.end_imported();
        }
        let data = &row.data;
        let at = data["time"]["created"].as_u64().unwrap_or(row.created);
        match row.kind.as_str() {
            "user" => self.person(at, row),
            "assistant" => self.assistant(at, row),
            "idle" => self.idle(at, data),
            "model-switched" => self.model(at, &data["model"]),
            "compaction" if !row.copied() => self.compaction(at, row),
            "shell" => self.shell(at, row),
            _ => {}
        }
    }

    /// The end of the rows a fork copied, where they were read and the fork was not said.
    pub fn forked(&mut self, parent: &str, at: u64) {
        if self.state.inherited && !self.state.forked {
            self.state.forked = true;
            self.events.push(Event::Fork {
                at,
                parent: parent.to_owned(),
            });
        }
    }

    /// The end of an imported turn, at its last reply: failed where that reply failed.
    pub fn end_imported(&mut self) {
        if let Some(at) = self.state.imported_end.take() {
            let outcome = if self.state.failure.is_some() {
                "failed"
            } else {
                "succeeded"
            };
            self.idle(at, &json!({ "outcome": outcome }));
        }
    }

    /// What a person typed: the first prompt after an `idle` row begins a turn; one sent
    /// while the turn runs is steered into it.
    fn person(&mut self, at: u64, row: &Row) {
        if !self.state.open {
            self.state.open = true;
            self.state.failure = None;
            self.events.push(Event::TurnBegan { at, turn: None });
        }
        let clips: Vec<Clip> = row.data["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|file| clip(file, &row.blobs))
            .collect();
        let text = row.data["text"].as_str().unwrap_or_default();
        if text.trim().is_empty() && clips.is_empty() {
            return;
        }
        self.events.push(Event::Words {
            at,
            id: row.id.clone(),
            voice: Voice::Person,
            text: text.to_owned(),
            message: None,
            call: None,
            clips,
        });
    }

    fn model(&mut self, at: u64, model: &Value) {
        let Some(id) = model["id"].as_str() else {
            return;
        };
        if self.state.model.as_deref() == Some(id) {
            return;
        }
        self.state.model = Some(id.to_owned());
        self.events.push(Event::Model {
            at,
            model: id.to_owned(),
            provider: model["providerID"].as_str().map(str::to_owned),
            thinking: model["variant"]
                .as_str()
                .filter(|variant| *variant != "default")
                .map(str::to_owned),
        });
    }

    /// One reply of the model: its text, its reasoning and its calls, in order, then what
    /// it spent.
    fn assistant(&mut self, at: u64, row: &Row) {
        let data = &row.data;
        self.model(at, &data["model"]);
        for (index, part) in data["content"].as_array().into_iter().flatten().enumerate() {
            let id = format!("{}:{index}", row.id);
            let begun = part["time"]["created"].as_u64().unwrap_or(at);
            match part["type"].as_str() {
                Some("text") | Some("reasoning") => {
                    let text = part["text"].as_str().unwrap_or_default();
                    if text.trim().is_empty() {
                        continue;
                    }
                    let voice = if part["type"] == "text" {
                        Voice::Agent
                    } else {
                        Voice::Thought
                    };
                    self.events.push(Event::Words {
                        at: begun,
                        id,
                        voice,
                        text: text.to_owned(),
                        message: Some(row.id.clone()),
                        call: None,
                        clips: Vec::new(),
                    });
                }
                Some("tool") => self.tool(begun, id, &row.id, part),
                _ => {}
            }
        }
        if let Some(error) = data.get("error").filter(|e| e.is_object()) {
            self.state.failure = Some(failure(error, at, &data["model"]));
        }
        if row.imported {
            self.state.imported_end = Some(data["time"]["completed"].as_u64().unwrap_or(at));
        }
        let tokens = &data["tokens"];
        if tokens.is_object() {
            let number = |value: &Value| value.as_u64().unwrap_or(0);
            let cached = number(&tokens["cache"]["read"]);
            let written = number(&tokens["cache"]["write"]);
            let reasoning = number(&tokens["reasoning"]);
            self.events.push(Event::Tokens {
                at: data["time"]["completed"].as_u64().unwrap_or(at),
                count: Count::Response,
                response: Some(row.id.clone()),
                model: data["model"]["id"].as_str().map(str::to_owned),
                input: number(&tokens["input"]) + cached + written,
                cached,
                cache_write: Some(written),
                output: number(&tokens["output"]) + reasoning,
                reasoning,
                context_left: None,
            });
        }
    }

    /// A call of the model and, where it settled, its end and the text of its result.
    fn tool(&mut self, at: u64, id: String, response: &str, part: &Value) {
        let Some(call_id) = part["id"].as_str() else {
            return;
        };
        let name = part["name"].as_str().unwrap_or_default();
        let state = &part["state"];
        let input = match &state["input"] {
            Value::String(text) => {
                serde_json::from_str(text).unwrap_or(Value::String(text.clone()))
            }
            other => other.clone(),
        };
        self.events.push(Event::CallBegan {
            at,
            call: call(call_id, name, input, response, self.servers),
        });
        let child = (name == "subagent")
            .then(|| state["metadata"]["sessionID"].as_str())
            .flatten();
        if let Some(child) = child {
            self.events.push(Event::SubAgent {
                at,
                external_id: child.to_owned(),
                call: Some(call_id.to_owned()),
                state: Some(Execution::Running),
                error: None,
            });
        }
        let ended = part["time"]["completed"].as_u64().unwrap_or(at);
        self.inner(at, ended, call_id, response, state);
        let (ok, text) = match state["status"].as_str() {
            Some("completed") => (true, text_of(&state["content"])),
            Some("error") => {
                let text = text_of(&state["content"]);
                let text = if text.trim().is_empty() {
                    state["error"]["message"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned()
                } else {
                    text
                };
                (false, text)
            }
            _ => return,
        };
        self.events.push(Event::CallEnded {
            at: ended,
            id: call_id.to_owned(),
            ok,
            chars: text.chars().count() as u64,
            tail: tail(&text),
            waited: Vec::new(),
        });
        if let Some(child) = child {
            // A sub-agent sent to the background ends its call while it still runs.
            let running = state["metadata"]["status"] == "running";
            if !running {
                self.events.push(Event::SubAgent {
                    at: ended,
                    external_id: child.to_owned(),
                    call: Some(call_id.to_owned()),
                    state: Some(if ok {
                        Execution::Finished
                    } else {
                        Execution::Failed
                    }),
                    error: (!ok).then(|| text.lines().next().unwrap_or_default().to_owned()),
                });
            }
        }
        if !text.trim().is_empty() {
            self.events.push(Event::Words {
                at: ended,
                id,
                voice: Voice::Tool,
                text,
                message: None,
                call: Some(call_id.to_owned()),
                clips: Vec::new(),
            });
        }
    }

    /// The calls that one `execute` of Code Mode made, each with the `execute` as its parent.
    /// OpenCode keeps no id, time or result of its own for them: only the tool, its state and
    /// the input as shown. Inside Code Mode a tool is `<namespace>.<tool>`, where a direct call
    /// names it `<namespace>_<tool>`.
    fn inner(&mut self, at: u64, ended: u64, parent: &str, response: &str, state: &Value) {
        let Some(calls) = state["metadata"]["toolCalls"].as_array() else {
            return;
        };
        for (index, inner) in calls.iter().enumerate() {
            let Some(name) = inner["tool"].as_str() else {
                continue;
            };
            let id = format!("{parent}/{index}");
            let mut call = call(
                &id,
                &name.replacen('.', "_", 1),
                inner.get("input").cloned().unwrap_or(Value::Null),
                response,
                self.servers,
            );
            call.parent = Some(parent.to_owned());
            self.events.push(Event::CallBegan { at, call });
            let ok = match inner["status"].as_str() {
                Some("completed") => true,
                Some("error") => false,
                _ => continue,
            };
            self.events.push(Event::CallEnded {
                at: ended,
                id,
                ok,
                chars: 0,
                tail: None,
                waited: Vec::new(),
            });
        }
    }

    /// The end of a turn.
    fn idle(&mut self, at: u64, data: &Value) {
        if !self.state.open {
            return;
        }
        self.state.open = false;
        let failure = self.state.failure.take();
        let (state, failure) = match data["outcome"].as_str() {
            Some("succeeded") => (TurnState::Finished, None),
            Some("failed") => (
                TurnState::Failed,
                Some(failure.unwrap_or_else(|| Failure {
                    kind: FailureKind::Other,
                    line: "OpenCode ended the turn as failed".into(),
                    service: false,
                    http_status: None,
                    code: None,
                    retry_at: None,
                    provider: None,
                    model: None,
                    thinking: None,
                })),
            ),
            _ => (TurnState::Interrupted, None),
        };
        self.events.push(Event::TurnEnded {
            at,
            turn: None,
            state,
            failure,
        });
    }

    /// A compaction that finished: OpenCode's summary replaced the context before it.
    fn compaction(&mut self, at: u64, row: &Row) {
        let data = &row.data;
        if data["status"] != "completed" {
            return;
        }
        let by = match data["reason"].as_str() {
            Some("manual") => Compactor::Person,
            _ => Compactor::Harness { reason: None },
        };
        self.events.push(Event::Compacted {
            at,
            id: row.id.clone(),
            by,
            tokens_before: None,
            tokens_after: None,
        });
        if let Some(summary) = data["summary"].as_str().filter(|s| !s.trim().is_empty()) {
            self.events.push(Event::Words {
                at,
                id: format!("{}:summary", row.id),
                voice: Voice::Summary,
                text: summary.to_owned(),
                message: None,
                call: None,
                clips: Vec::new(),
            });
        }
    }

    /// A command the person ran with `!`: a call of the shell, outside any reply.
    fn shell(&mut self, at: u64, row: &Row) {
        let data = &row.data;
        let id = data["shellID"].as_str().unwrap_or(&row.id).to_owned();
        self.events.push(Event::CallBegan {
            at,
            call: Call {
                id: id.clone(),
                tool: "shell".into(),
                input: serde_json::json!({"command": data["command"]}),
                group: Group::Shell,
                server: None,
                steery: None,
                asks: None,
                parent: None,
                response: None,
            },
        });
        let Some(ended) = data["time"]["completed"].as_u64() else {
            return;
        };
        let text = data["output"]["output"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.events.push(Event::CallEnded {
            at: ended,
            id: id.clone(),
            ok: data["status"] == "exited" && data["exit"].as_i64().unwrap_or(0) == 0,
            chars: text.chars().count() as u64,
            tail: tail(&text),
            waited: Vec::new(),
        });
        if !text.trim().is_empty() {
            self.events.push(Event::Words {
                at: ended,
                id: format!("{}:output", row.id),
                voice: Voice::Tool,
                text,
                message: None,
                call: Some(id),
                clips: Vec::new(),
            });
        }
    }
}

/// A call by OpenCode's name for its tool. OpenCode names an MCP tool
/// `<server>_<tool>`, each part with every character outside `[A-Za-z0-9_-]` made `_`.
pub(crate) fn call(id: &str, name: &str, input: Value, response: &str, servers: &[String]) -> Call {
    let served = servers.iter().find_map(|server| {
        let prefix = format!("{}_", normalized(server));
        name.strip_prefix(&prefix)
            .map(|tool| (server.as_str(), tool))
    });
    let (tool, group, server, steery) = match served {
        Some(("steery", tool)) => (
            format!("mcp__steery__{tool}"),
            Group::Steery,
            None,
            Some(tool.to_owned()),
        ),
        Some((server, tool)) => (
            format!("mcp__{server}__{tool}"),
            Group::Mcp,
            Some(server.to_owned()),
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
        response: Some(response.to_owned()),
    }
}

/// OpenCode's spelling of a name inside a tool's name.
pub(crate) fn normalized(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The group of one of OpenCode's own tools.
fn group(name: &str) -> Group {
    match name {
        "read" | "list_mcp_resources" | "read_mcp_resource" => Group::FilesRead,
        "edit" | "write" | "patch" => Group::FilesEdit,
        "glob" | "grep" => Group::Search,
        "shell" => Group::Shell,
        "webfetch" | "websearch" => Group::Web,
        "subagent" => Group::Agents,
        "todowrite" => Group::Plan,
        "question" => Group::Ask,
        "experimental_image_generate" => Group::Media,
        "execute" => Group::Code,
        // Code Mode's own functions inside an `execute`: an HTTP request, and its catalog's search.
        "fetch" => Group::Web,
        "search" => Group::Harness,
        "skill" | "opencode" | "models" | "session_move" | "session_rename" | "schedule"
        | "schedule_create" | "schedule_delete" | "schedule_list" | "schedule_pause"
        | "schedule_resume" | "worktree_create" | "worktree_list" | "worktree_remove"
        | "artifact_create" | "artifact_delete" | "artifact_read" | "artifact_update" => {
            Group::Harness
        }
        _ => Group::Custom,
    }
}

/// The text parts of a tool's result.
fn text_of(content: &Value) -> String {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The last 8 lines of a result.
fn tail(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let kept = lines[lines.len().saturating_sub(8)..].join("\n");
    (!kept.trim().is_empty()).then_some(kept)
}

/// A file a person attached: its bytes in the row, or in `blob` by its `media:` ref.
fn clip(file: &Value, blobs: &[(String, String)]) -> Option<Clip> {
    let name = file["name"].as_str().map(str::to_owned);
    let media_type = file["mime"].as_str().map(str::to_owned);
    let data = match file["data"].as_str() {
        Some(data) => data.to_owned(),
        None => {
            let hash = file["uri"].as_str()?.strip_prefix("media:")?;
            blobs.iter().find(|(h, _)| h == hash)?.1.clone()
        }
    };
    Some(Clip::Bytes {
        data,
        media_type,
        name,
    })
}

/// Why a reply failed, from OpenCode's structured error.
fn failure(error: &Value, at: u64, model: &Value) -> Failure {
    let kind = error["type"].as_str().unwrap_or_default();
    let status = error["status"].as_u64().and_then(|s| u16::try_from(s).ok());
    let kind_of = match kind {
        "provider.auth" => FailureKind::SignIn,
        "provider.rate-limit"
        | "provider.quota"
        | "provider.free-limit"
        | "provider.go-limit"
        | "provider.budget-limit"
        | "provider.model-budget-limit"
        | "provider.chatgpt-limit" => FailureKind::RateLimit,
        _ => match status {
            Some(401) => FailureKind::SignIn,
            Some(429 | 503 | 529) => FailureKind::RateLimit,
            _ => FailureKind::Other,
        },
    };
    Failure {
        kind: kind_of,
        line: error["message"]
            .as_str()
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
        service: kind.starts_with("provider."),
        http_status: status,
        code: (!kind.is_empty()).then(|| kind.to_owned()),
        retry_at: error["retryAfterMs"].as_u64().map(|wait| at + wait),
        provider: model["providerID"].as_str().map(str::to_owned),
        model: model["id"].as_str().map(str::to_owned),
        thinking: model["variant"]
            .as_str()
            .filter(|variant| *variant != "default")
            .map(str::to_owned),
    }
}
