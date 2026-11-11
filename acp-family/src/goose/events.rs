//! Goose's messages, read as Steery's events. Each message is a row with a `role` and a JSON
//! array of blocks tagged `type`: a person's words are `text` and `image` blocks of a `user`
//! row, an assistant's carry `text`, `thinking` and `toolRequest` blocks, and the results of
//! its calls come back as `toolResponse` blocks of a `user` row. An `error` block is a failure
//! Goose shows. A row whose metadata says `userVisible: false` is Goose's alone, such as the
//! `<turn-context>` it adds to each prompt. Words told while a turn runs carry
//! `steer: true` and go into that turn. No row ends a turn: the next prompt does, or a read
//! that finds the assistant's answer last.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_sdk::wire::harness::{
    Call, Clip, Count, Event, Failure, FailureKind, Group, TurnState, Voice,
};

/// The model that Goose names where the provider picks it.
const CURRENT: &str = "current";

/// One row of `messages`.
pub(crate) struct Message {
    pub id: String,
    pub role: String,
    /// Milliseconds since 1970.
    pub at: u64,
    pub content: Value,
    pub metadata: Value,
}

impl Message {
    /// Goose's alone, never shown to its person.
    pub fn hidden(&self) -> bool {
        self.metadata["userVisible"] == false
    }

    fn blocks(&self) -> &[Value] {
        self.content.as_array().map_or(&[], Vec::as_slice)
    }

    /// The person's words in it, where it is the person's.
    pub fn said(&self) -> Option<String> {
        if self.role != "user" || self.hidden() {
            return None;
        }
        let text: Vec<&str> = self
            .blocks()
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect();
        (!text.is_empty()).then(|| text.join("\n"))
    }

    /// A person's message, not the results of calls.
    pub fn begins(&self) -> bool {
        self.role == "user"
            && !self.hidden()
            && self
                .blocks()
                .iter()
                .any(|block| matches!(block["type"].as_str(), Some("text" | "image")))
    }
}

/// What the rows read so far carry on to the next read.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct State {
    /// The id of the person's message that began the turn not yet ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn: Option<String>,
    /// The newest time read.
    #[serde(default)]
    at: u64,
    /// Whether the newest row of the open turn is the assistant's answer, not words or
    /// results it has yet to answer.
    #[serde(default)]
    answered: bool,
    /// The failure Goose showed in the open turn: its kind and words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<(String, String)>,
    /// The model of the newest answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// Its provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
}

impl State {
    /// A turn began and has not ended.
    pub fn open(&self) -> bool {
        self.turn.is_some()
    }
}

/// Rows folded into events.
pub(crate) struct Fold<'a> {
    pub state: State,
    pub events: Vec<Event>,
    /// The extensions Goose loads, to tell an extension's tool by its name.
    servers: &'a [String],
}

impl<'a> Fold<'a> {
    pub fn new(state: State, servers: &'a [String]) -> Self {
        Self {
            state,
            events: Vec::new(),
            servers,
        }
    }

    /// One row.
    pub fn message(&mut self, message: &Message) {
        self.state.at = self.state.at.max(message.at);
        if message.hidden() {
            return;
        }
        match message.role.as_str() {
            "user" => self.user(message),
            "assistant" => self.assistant(message),
            _ => {}
        }
    }

    fn user(&mut self, message: &Message) {
        let at = message.at;
        let id = message.id.as_str();
        if message.begins() {
            let steered = message.metadata["steer"] == true && self.state.turn.is_some();
            if !steered {
                self.end(self.closing());
                self.state.turn = Some(id.to_owned());
                self.events.push(Event::TurnBegan {
                    at,
                    turn: Some(id.to_owned()),
                });
            }
            self.state.answered = false;
            let clips = message
                .blocks()
                .iter()
                .filter(|block| block["type"] == "image")
                .filter_map(|block| {
                    Some(Clip::Bytes {
                        data: block["data"].as_str()?.to_owned(),
                        media_type: block["mimeType"].as_str().map(str::to_owned),
                        name: None,
                    })
                })
                .collect();
            self.events.push(Event::Words {
                at,
                id: id.to_owned(),
                voice: Voice::Person,
                text: message.said().unwrap_or_default(),
                message: Some(id.to_owned()),
                call: None,
                clips,
            });
        }
        for block in message
            .blocks()
            .iter()
            .filter(|b| b["type"] == "toolResponse")
        {
            self.state.answered = false;
            let call = block["id"].as_str().unwrap_or_default();
            let result = &block["toolResult"];
            let (ok, text) = match result["status"].as_str() {
                Some("success") => (
                    result.pointer("/value/isError") != Some(&Value::Bool(true)),
                    text_of(&result["value"]["content"]),
                ),
                _ => (
                    false,
                    result["error"].as_str().unwrap_or_default().to_owned(),
                ),
            };
            self.events.push(Event::CallEnded {
                at,
                id: call.to_owned(),
                ok,
                chars: text.chars().count() as u64,
                tail: tail(&text),
                waited: Vec::new(),
            });
            if !text.trim().is_empty() {
                self.events.push(Event::Words {
                    at,
                    id: format!("{call}:output"),
                    voice: Voice::Tool,
                    text,
                    message: None,
                    call: Some(call.to_owned()),
                    clips: Vec::new(),
                });
            }
        }
    }

    fn assistant(&mut self, message: &Message) {
        let at = message.at;
        let id = message.id.as_str();
        let inference = &message.metadata["inference"];
        let provider = inference["provider"].as_str();
        let model = inference["resolvedModel"]
            .as_str()
            .or_else(|| inference["requestedModel"].as_str())
            .filter(|model| *model != CURRENT);
        if provider.is_some() {
            self.state.provider = provider.map(str::to_owned);
        }
        if let Some(model) = model
            && self.state.model.as_deref() != Some(model)
        {
            self.state.model = Some(model.to_owned());
            self.events.push(Event::Model {
                at,
                model: model.to_owned(),
                provider: provider.map(str::to_owned),
                thinking: None,
            });
        }
        let mut asks = false;
        for (index, block) in message.blocks().iter().enumerate() {
            match block["type"].as_str() {
                Some(kind @ ("text" | "thinking")) => {
                    let (voice, key) = match kind {
                        "text" => (Voice::Agent, "text"),
                        _ => (Voice::Thought, "thinking"),
                    };
                    let text = block[key].as_str().unwrap_or_default();
                    if text.trim().is_empty() {
                        continue;
                    }
                    self.events.push(Event::Words {
                        at,
                        id: format!("{id}:{index}"),
                        voice,
                        text: text.to_owned(),
                        message: Some(id.to_owned()),
                        call: None,
                        clips: Vec::new(),
                    });
                }
                Some("toolRequest") => {
                    asks = true;
                    let requested = block.pointer("/toolCall/value");
                    let Some(name) = requested.and_then(|value| value["name"].as_str()) else {
                        continue;
                    };
                    let call = call(
                        block["id"].as_str().unwrap_or_default(),
                        name,
                        requested.map_or(Value::Null, |value| value["arguments"].clone()),
                        block
                            .pointer("/_meta/goose.acp.kind")
                            .and_then(Value::as_str),
                        id,
                        self.servers,
                    );
                    self.events.push(Event::CallBegan { at, call });
                }
                Some("error") => {
                    self.state.error = Some((
                        block["kind"].as_str().unwrap_or_default().to_owned(),
                        block["message"].as_str().unwrap_or_default().to_owned(),
                    ));
                }
                _ => {}
            }
        }
        self.state.answered = !asks;
        let usage = &message.metadata["usage"];
        if usage.is_object() {
            let count = |key: &str| usage[key].as_u64().unwrap_or(0);
            self.events.push(Event::Tokens {
                at,
                count: Count::Response,
                response: Some(id.to_owned()),
                model: model.map(str::to_owned),
                input: count("inputTokens"),
                cached: count("cacheReadTokens"),
                cache_write: usage["cacheWriteTokens"].as_u64(),
                output: count("outputTokens"),
                reasoning: 0,
                context_left: None,
            });
        }
    }

    /// How the open turn ended where the next prompt begins: finished where the assistant
    /// answered last, else interrupted.
    fn closing(&self) -> TurnState {
        if self.state.answered || self.state.error.is_some() {
            TurnState::Finished
        } else {
            TurnState::Interrupted
        }
    }

    /// The end of a read: the open turn ends where the assistant's answer or a failure is
    /// its newest row. Goose keeps no mark of a turn that runs, so words or results not yet
    /// answered leave it open.
    pub fn settle(&mut self) {
        if self.state.answered || self.state.error.is_some() {
            self.end(TurnState::Finished);
        }
    }

    /// The end of the open turn, if one is open: failed where Goose showed a failure in it.
    fn end(&mut self, state: TurnState) {
        let Some(turn) = self.state.turn.take() else {
            return;
        };
        self.state.answered = false;
        let error = self.state.error.take();
        let (state, failure) = match (state, error) {
            (TurnState::Interrupted, _) => (TurnState::Interrupted, None),
            (_, Some((kind, line))) => (TurnState::Failed, Some(self.failure(&kind, &line))),
            (state, None) => (state, None),
        };
        self.events.push(Event::TurnEnded {
            at: self.state.at,
            turn: Some(turn),
            state,
            failure,
        });
    }

    /// A failure as Goose showed it: its kind, and the first line of its words.
    fn failure(&self, kind: &str, line: &str) -> Failure {
        Failure {
            kind: match kind {
                "authentication" => FailureKind::SignIn,
                "contextLengthExceeded" => FailureKind::ContextFull,
                _ => FailureKind::Other,
            },
            line: line.lines().next().unwrap_or_default().to_owned(),
            service: true,
            http_status: None,
            code: None,
            retry_at: None,
            provider: self.state.provider.clone(),
            model: self.state.model.clone(),
            thinking: None,
        }
    }
}

/// A call by Goose's name for its tool. Goose names an extension's tool
/// `<extension key>__<tool>`; a call that an ACP provider ran itself carries its ACP kind
/// under `_meta`, and its title as the name.
pub(crate) fn call(
    id: &str,
    name: &str,
    input: Value,
    kind: Option<&str>,
    response: &str,
    servers: &[String],
) -> Call {
    let served = servers.iter().find_map(|server| {
        let prefix = format!("{}__", key(server));
        name.strip_prefix(&prefix)
            .map(|tool| (server.as_str(), tool))
    });
    let (tool, group, server, steery) = match (served, kind) {
        (Some(("steery", tool)), _) => (
            format!("mcp__steery__{tool}"),
            Group::Steery,
            None,
            Some(tool.to_owned()),
        ),
        (Some((server, tool)), _) => (
            format!("mcp__{server}__{tool}"),
            Group::Mcp,
            Some(server.to_owned()),
            None,
        ),
        (None, Some(kind)) => (name.to_owned(), acp_group(kind), None, None),
        (None, None) => (name.to_owned(), group(name), None, None),
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

/// An extension's key, as Goose makes it from its name (`config/extensions.rs:22-32`).
fn key(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .to_lowercase()
}

/// The group of one of Goose's own tools.
fn group(name: &str) -> Group {
    match name {
        "shell" => Group::Shell,
        "tree" | "read_image" => Group::FilesRead,
        "edit" | "write" => Group::FilesEdit,
        "analyze" => Group::Code,
        "delegate" => Group::Agents,
        "load" | "load_skill" => Group::Harness,
        _ if name.starts_with("todo__") => Group::Plan,
        _ => Group::Custom,
    }
}

/// The group of a call by its ACP kind.
fn acp_group(kind: &str) -> Group {
    match kind {
        "read" => Group::FilesRead,
        "edit" | "delete" | "move" => Group::FilesEdit,
        "search" => Group::Search,
        "execute" => Group::Shell,
        "fetch" => Group::Web,
        "think" => Group::Plan,
        "switch_mode" => Group::Harness,
        _ => Group::Custom,
    }
}

/// The text of a result's content blocks.
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
