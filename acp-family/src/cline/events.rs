//! Cline's messages, read as Steery's events. A conversation's messages file holds its
//! messages in Anthropic's block shape: a person's message begins a turn, an assistant's
//! carries text, `thinking` and `tool_use` blocks, and the results of its calls come back as
//! `tool_result` blocks of a `user` message. An assistant message whose `metadata` has
//! `displayRole: "error"` is a failure Cline shows, not words of the model's. No message
//! ends a turn: the next prompt does, or the conversation's status once no turn runs.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_sdk::wire::harness::{
    Call, Clip, Count, Event, Failure, FailureKind, Group, TurnState, Voice,
};

/// What the messages read so far carry on to the next read.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct State {
    /// The id of the person's message that began the turn not yet ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn: Option<String>,
    /// The newest time read.
    #[serde(default)]
    at: u64,
    /// The failure Cline showed in the open turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// The model of the newest answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// Its provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
}

/// Messages folded into events.
pub(crate) struct Fold<'a> {
    pub state: State,
    pub events: Vec<Event>,
    /// The MCP servers Cline loads, to tell an MCP tool by its name.
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

    /// One message.
    pub fn message(&mut self, message: &Value) {
        let at = message["ts"].as_u64().unwrap_or(self.state.at);
        self.state.at = self.state.at.max(at);
        let id = message["id"].as_str().unwrap_or_default();
        let blocks: &[Value] = message["content"].as_array().map_or(&[], Vec::as_slice);
        match message["role"].as_str() {
            Some("user") => self.user(id, at, blocks),
            Some("assistant") => self.assistant(id, at, message, blocks),
            _ => {}
        }
    }

    fn user(&mut self, id: &str, at: u64, blocks: &[Value]) {
        let said: Vec<&Value> = blocks
            .iter()
            .filter(|b| b["type"] != "tool_result")
            .collect();
        if !said.is_empty() {
            self.end(TurnState::Finished);
            self.state.turn = Some(id.to_owned());
            self.events.push(Event::TurnBegan {
                at,
                turn: Some(id.to_owned()),
            });
            let text = said
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .map(person)
                .collect::<Vec<_>>()
                .join("\n");
            let clips = said
                .iter()
                .filter(|block| block["type"] == "image")
                .filter_map(|block| {
                    Some(Clip::Bytes {
                        data: block["data"].as_str()?.to_owned(),
                        media_type: block["mediaType"].as_str().map(str::to_owned),
                        name: None,
                    })
                })
                .collect();
            self.events.push(Event::Words {
                at,
                id: id.to_owned(),
                voice: Voice::Person,
                text,
                message: Some(id.to_owned()),
                call: None,
                clips,
            });
        }
        for block in blocks.iter().filter(|b| b["type"] == "tool_result") {
            let call = block["tool_use_id"].as_str().unwrap_or_default();
            let text = text_of(&block["content"]);
            self.events.push(Event::CallEnded {
                at,
                id: call.to_owned(),
                ok: block["is_error"].as_bool() != Some(true),
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

    fn assistant(&mut self, id: &str, at: u64, message: &Value, blocks: &[Value]) {
        let model = message.pointer("/modelInfo/id").and_then(Value::as_str);
        let provider = message
            .pointer("/modelInfo/provider")
            .and_then(Value::as_str);
        if message.pointer("/metadata/displayRole") == Some(&Value::from("error")) {
            if model.is_some() {
                self.state.model = model.map(str::to_owned);
                self.state.provider = provider.map(str::to_owned);
            }
            self.state.error = Some(text_of(&message["content"]));
            return;
        }
        if let Some(model) = model
            && self.state.model.as_deref() != Some(model)
        {
            self.state.model = Some(model.to_owned());
            self.state.provider = provider.map(str::to_owned);
            self.events.push(Event::Model {
                at,
                model: model.to_owned(),
                provider: provider.map(str::to_owned),
                thinking: None,
            });
        }
        for (index, block) in blocks.iter().enumerate() {
            match block["type"].as_str() {
                Some("text") | Some("thinking") => {
                    let (voice, key) = match block["type"].as_str() {
                        Some("text") => (Voice::Agent, "text"),
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
                Some("tool_use") => {
                    let call = call(
                        block["id"].as_str().unwrap_or_default(),
                        block["name"].as_str().unwrap_or_default(),
                        block["input"].clone(),
                        id,
                        self.servers,
                    );
                    self.events.push(Event::CallBegan { at, call });
                }
                _ => {}
            }
        }
        let metrics = &message["metrics"];
        if metrics.is_object() {
            let count = |key: &str| metrics[key].as_u64().unwrap_or(0);
            self.events.push(Event::Tokens {
                at,
                count: Count::Response,
                response: Some(id.to_owned()),
                model: model.map(str::to_owned),
                input: count("inputTokens"),
                cached: count("cacheReadTokens"),
                cache_write: metrics["cacheWriteTokens"].as_u64(),
                output: count("outputTokens"),
                reasoning: 0,
                context_left: None,
            });
        }
    }

    /// The end of the open turn, if one is open: failed where Cline showed a failure in it.
    pub fn end(&mut self, state: TurnState) {
        let Some(turn) = self.state.turn.take() else {
            return;
        };
        let error = self.state.error.take();
        let (state, failure) = match (state, error) {
            (TurnState::Interrupted, _) => (TurnState::Interrupted, None),
            (_, Some(error)) => (TurnState::Failed, Some(self.failure(&error))),
            (state, None) => (state, None),
        };
        self.events.push(Event::TurnEnded {
            at: self.state.at,
            turn: Some(turn),
            state,
            failure,
        });
    }

    /// A failure as Cline showed it: one line of words, no status code.
    fn failure(&self, error: &str) -> Failure {
        let lower = error.to_lowercase();
        let kind = if lower.contains("unauthorized") || lower.contains("authenticat") {
            FailureKind::SignIn
        } else if lower.contains("rate limit") || lower.contains("429") {
            FailureKind::RateLimit
        } else if lower.contains("context window") || lower.contains("context length") {
            FailureKind::ContextFull
        } else {
            FailureKind::Other
        };
        Failure {
            kind,
            line: error.lines().next().unwrap_or_default().to_owned(),
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

/// A person's words without the `<user_input mode="…">` wrapper Cline stores them in.
pub(crate) fn person(text: &str) -> &str {
    let trimmed = text.trim();
    trimmed
        .strip_prefix("<user_input")
        .and_then(|rest| rest.split_once('>'))
        .and_then(|(_, rest)| rest.strip_suffix("</user_input>"))
        .map_or(text, str::trim)
}

/// A call by Cline's name for its tool. Cline names an MCP tool `<server>__<tool>`, every
/// character outside `[A-Za-z0-9_-]` made `_`.
pub(crate) fn call(id: &str, name: &str, input: Value, response: &str, servers: &[String]) -> Call {
    let served = servers.iter().find_map(|server| {
        let prefix = format!("{}__", normalized(server));
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

/// Cline's spelling of a server's name inside a tool's name.
fn normalized(name: &str) -> String {
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

/// The group of one of Cline's own tools.
fn group(name: &str) -> Group {
    match name {
        "read_files" => Group::FilesRead,
        "apply_patch" | "editor" | "edit" => Group::FilesEdit,
        "search_codebase" => Group::Search,
        "run_commands" | "bash" | "execute_command" | "exec_command" => Group::Shell,
        "fetch_web_content" => Group::Web,
        "ask_question" => Group::Ask,
        "spawn_agent" => Group::Agents,
        "skills" | "submit_and_exit" => Group::Harness,
        _ if name.starts_with("team_") => Group::Agents,
        _ => Group::Custom,
    }
}

/// The text of a block's content: a string, or its text blocks.
fn text_of(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_owned();
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_failed_turn_ends_with_cline_s_words() {
        let mut fold = Fold::new(State::default(), &[]);
        fold.message(&json!({"id": "u", "role": "user", "ts": 1,
            "content": [{"type": "text", "text": "<user_input mode=\"act\">say hi</user_input>"}]}));
        fold.message(&json!({"id": "e", "role": "assistant", "ts": 2,
            "content": [{"type": "text", "text": "Unauthorized: sign in"}],
            "metadata": {"displayOnly": true, "displayRole": "error"},
            "modelInfo": {"id": "m", "provider": "cline"}}));
        fold.end(TurnState::Finished);
        let events = serde_json::to_value(&fold.events).unwrap();
        assert_eq!(events[1]["text"], "say hi");
        assert_eq!(events[2]["type"], "turn_ended");
        assert_eq!(events[2]["state"], "failed");
        assert_eq!(events[2]["failure"]["kind"], "sign_in");
        assert_eq!(events[2]["at"], 2);
    }

    #[test]
    fn steery_s_tools_read_as_steery() {
        let servers = vec!["steery".to_owned()];
        let call = call("c", "steery__sessions_describe", json!({}), "m", &servers);
        assert_eq!(call.tool, "mcp__steery__sessions_describe");
        assert_eq!(call.group, Group::Steery);
        assert_eq!(group("run_commands"), Group::Shell);
    }
}
