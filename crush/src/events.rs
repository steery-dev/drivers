//! Crush's messages, read as Steery's events. Each message is a row with a `role` and a JSON
//! array of parts, each `{type, data}` (`internal/message/message.go`): a person's words are
//! `text`, `image_url` and `binary` parts of a `user` row; an answer carries `reasoning`,
//! `text` and `tool_call` parts and ends with a `finish` part whose `reason` says how; the
//! results of its calls come back as `tool_result` parts of a `tool` row. A `text` part
//! marked `hidden` is a continuation Crush wrote itself. A `user` row of a `shell_command`
//! part is a command the person ran; one of a `sub_agent_report` part wakes the lead with a
//! sub-agent's report. A summary is an `assistant` row marked `is_summary_message`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    Clip, Compactor, Count, Event, Failure, FailureKind, Group, TurnState, Voice,
};

use crate::calls;

/// One row of `messages`.
pub(crate) struct Message {
    pub id: String,
    pub role: String,
    pub parts: Value,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// Milliseconds since 1970.
    pub at: u64,
    /// A summary that replaced the history before it.
    pub summary: bool,
}

/// One part: its type and its data.
fn parts(message: &Message) -> impl Iterator<Item = (usize, &str, &Value)> {
    message
        .parts
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, part)| Some((index, part["type"].as_str()?, &part["data"])))
}

impl Message {
    /// Crush is done writing it: every row but an answer the model still streams.
    pub fn finished(&self) -> bool {
        self.role != "assistant" || parts(self).any(|(_, kind, _)| kind == "finish")
    }

    /// The person's words in it, where it is the person's.
    pub fn said(&self) -> String {
        parts(self)
            .filter(|(_, kind, data)| *kind == "text" && data["hidden"] != true)
            .filter_map(|(_, _, data)| data["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A message that begins a turn: the person's words, or a sub-agent's report that wakes
    /// the lead.
    pub fn begins(&self) -> bool {
        self.role == "user"
            && parts(self).any(|(_, kind, data)| match kind {
                "text" => data["hidden"] != true,
                "image_url" | "binary" | "sub_agent_report" => true,
                _ => false,
            })
    }
}

/// What the rows read so far carry on to the next read.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct State {
    /// The id of the message that began the turn not yet ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn: Option<String>,
    /// The newest time read.
    #[serde(default)]
    at: u64,
    /// The model of the newest answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// Its provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    /// The id of the newest answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    response: Option<String>,
    /// The tokens last given for it.
    #[serde(default)]
    tokens: (u64, u64),
}

/// Rows folded into events.
pub(crate) struct Fold<'a> {
    pub state: State,
    pub events: Vec<Event>,
    /// The MCP servers Crush loads, to tell a server's tool by its name.
    servers: &'a [String],
    /// The ids of the session's sub-agents.
    children: &'a [String],
}

impl<'a> Fold<'a> {
    pub fn new(state: State, servers: &'a [String], children: &'a [String]) -> Self {
        Self {
            state,
            events: Vec::new(),
            servers,
            children,
        }
    }

    /// One row.
    pub fn message(&mut self, message: &Message) {
        self.state.at = self.state.at.max(message.at);
        match message.role.as_str() {
            "user" => self.user(message),
            "assistant" if message.summary => self.summary(message),
            "assistant" => self.assistant(message),
            "tool" => self.results(message),
            _ => {}
        }
    }

    fn user(&mut self, message: &Message) {
        let at = message.at;
        let id = message.id.as_str();
        if message.begins() {
            // Crush gives every answer it ends a `finish`: a turn still open never ended.
            self.end(TurnState::Interrupted, None);
            self.begin(at, id);
            let clips: Vec<Clip> = parts(message)
                .filter_map(|(_, kind, data)| match kind {
                    "image_url" => Some(Clip::Link {
                        url: data["url"].as_str()?.to_owned(),
                    }),
                    "binary" => Some(Clip::Bytes {
                        data: data["Data"].as_str()?.to_owned(),
                        media_type: data["MIMEType"].as_str().map(str::to_owned),
                        name: data["Path"]
                            .as_str()
                            .and_then(|path| path.rsplit('/').next())
                            .filter(|name| !name.is_empty())
                            .map(str::to_owned),
                    }),
                    _ => None,
                })
                .collect();
            let said = message.said();
            if !said.trim().is_empty() || !clips.is_empty() {
                self.events.push(Event::Words {
                    at,
                    id: id.to_owned(),
                    voice: Voice::Person,
                    text: said,
                    message: Some(id.to_owned()),
                    call: None,
                    clips,
                });
            }
        }
        for (index, kind, data) in parts(message) {
            match kind {
                "sub_agent_report" => {
                    let text = data["output"].as_str().unwrap_or_default();
                    self.events.push(Event::Words {
                        at,
                        id: format!("{id}:{index}"),
                        voice: Voice::Tool,
                        text: text.to_owned(),
                        message: Some(id.to_owned()),
                        call: None,
                        clips: Vec::new(),
                    });
                }
                "shell_command" => {
                    let call = format!("{id}:{index}");
                    let output = data["output"].as_str().unwrap_or_default();
                    self.events.push(Event::CallBegan {
                        at,
                        call: steery_sdk::wire::harness::Call {
                            id: call.clone(),
                            tool: "shell_command".into(),
                            input: json!({"command": data["command"]}),
                            group: Group::Shell,
                            server: None,
                            steery: None,
                            asks: None,
                            parent: None,
                            response: None,
                        },
                    });
                    self.ended(at, &call, data["exit_code"].as_i64() == Some(0), output);
                }
                _ => {}
            }
        }
    }

    fn assistant(&mut self, message: &Message) {
        let at = message.at;
        let id = message.id.as_str();
        if self.state.turn.is_none() {
            self.begin(at, id);
        }
        if message.provider.is_some() {
            self.state.provider.clone_from(&message.provider);
        }
        if let Some(model) = &message.model
            && self.state.model.as_ref() != Some(model)
        {
            self.state.model = Some(model.clone());
            self.events.push(Event::Model {
                at,
                model: model.clone(),
                provider: message.provider.clone(),
                thinking: None,
            });
        }
        self.state.response = Some(id.to_owned());
        for (index, kind, data) in parts(message) {
            match kind {
                "text" | "reasoning" => {
                    let (voice, key) = match kind {
                        "text" => (Voice::Agent, "text"),
                        _ => (Voice::Thought, "thinking"),
                    };
                    let text = data[key].as_str().unwrap_or_default();
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
                "tool_call" => {
                    let (Some(call), Some(tool)) = (data["id"].as_str(), data["name"].as_str())
                    else {
                        continue;
                    };
                    // Crush keeps a call's input as the JSON text the model wrote.
                    let input = data["input"]
                        .as_str()
                        .map(|input| serde_json::from_str(input).unwrap_or(json!(input)))
                        .unwrap_or(Value::Null);
                    let mut began = calls::call(call, tool, input, self.servers);
                    began.response = Some(id.to_owned());
                    let agents = began.group == Group::Agents;
                    self.events.push(Event::CallBegan { at, call: began });
                    if agents {
                        self.sub_agent(at, call);
                    }
                }
                "finish" => match data["reason"].as_str() {
                    Some("tool_use") => {}
                    Some("canceled") => self.end(TurnState::Interrupted, None),
                    Some("error") => {
                        let failure = self.failure(data);
                        self.end(TurnState::Failed, Some(failure));
                    }
                    _ => self.end(TurnState::Finished, None),
                },
                _ => {}
            }
        }
    }

    /// The sub-agent a call started: Crush gives it the call's id, or `<message>$$<call>`.
    fn sub_agent(&mut self, at: u64, call: &str) {
        let suffix = format!("$${call}");
        for child in self
            .children
            .iter()
            .filter(|child| *child == call || child.ends_with(&suffix))
        {
            self.events.push(Event::SubAgent {
                at,
                external_id: child.clone(),
                call: Some(call.to_owned()),
                state: None,
                error: None,
            });
        }
    }

    /// A summary: Crush compacted the history before it into these words.
    fn summary(&mut self, message: &Message) {
        let text: Vec<&str> = parts(message)
            .filter(|(_, kind, _)| *kind == "text")
            .filter_map(|(_, _, data)| data["text"].as_str())
            .collect();
        self.events.push(Event::Compacted {
            at: message.at,
            id: message.id.clone(),
            // Crush records no mark of who asked: its own threshold or the person's
            // `/compact` read the same.
            by: Compactor::Harness { reason: None },
            tokens_before: None,
            tokens_after: None,
        });
        self.events.push(Event::Words {
            at: message.at,
            id: message.id.clone(),
            voice: Voice::Summary,
            text: text.join("\n"),
            message: Some(message.id.clone()),
            call: None,
            clips: Vec::new(),
        });
    }

    fn results(&mut self, message: &Message) {
        for (_, kind, data) in parts(message) {
            if kind != "tool_result" {
                continue;
            }
            let call = data["tool_call_id"].as_str().unwrap_or_default();
            let text = data["content"].as_str().unwrap_or_default();
            self.ended(message.at, call, data["is_error"] != true, text);
        }
    }

    /// A call's end, and its output as the tool's words.
    fn ended(&mut self, at: u64, call: &str, ok: bool, text: &str) {
        self.events.push(Event::CallEnded {
            at,
            id: call.to_owned(),
            ok,
            chars: text.chars().count() as u64,
            tail: tail(text),
            waited: Vec::new(),
        });
        if !text.trim().is_empty() {
            self.events.push(Event::Words {
                at,
                id: format!("{call}:output"),
                voice: Voice::Tool,
                text: text.to_owned(),
                message: None,
                call: Some(call.to_owned()),
                clips: Vec::new(),
            });
        }
    }

    /// The tokens Crush keeps of a session: those of its newest response, given once each
    /// time they change. Crush counts what the model read, cache included, and does not
    /// say how much of it the cache served.
    pub fn tokens(&mut self, (input, output): (u64, u64)) {
        let Some(response) = self.state.response.clone() else {
            return;
        };
        if (input, output) == (0, 0) || (input, output) == self.state.tokens {
            return;
        }
        self.state.tokens = (input, output);
        self.events.push(Event::Tokens {
            at: self.state.at,
            count: Count::Response,
            response: Some(response),
            model: self.state.model.clone(),
            input,
            cached: 0,
            cache_write: None,
            output,
            reasoning: 0,
            context_left: None,
        });
    }

    fn begin(&mut self, at: u64, id: &str) {
        self.state.turn = Some(id.to_owned());
        self.events.push(Event::TurnBegan {
            at,
            turn: Some(id.to_owned()),
        });
    }

    /// The end of the open turn, if one is open.
    fn end(&mut self, state: TurnState, failure: Option<Failure>) {
        let Some(turn) = self.state.turn.take() else {
            return;
        };
        self.events.push(Event::TurnEnded {
            at: self.state.at,
            turn: Some(turn),
            state,
            failure,
        });
    }

    /// A failure as Crush showed it: the provider's status line and its details. `message`
    /// is the HTTP status text where the provider answered with one.
    fn failure(&self, data: &Value) -> Failure {
        let message = data["message"].as_str().unwrap_or_default().trim();
        let details = data["details"].as_str().unwrap_or_default().trim();
        let line = match (message, details) {
            ("", details) => details.to_owned(),
            (message, "") => message.to_owned(),
            (message, details) => format!("{message}: {details}"),
        };
        Failure {
            kind: match message {
                "Unauthorized" => FailureKind::SignIn,
                "Too Many Requests" => FailureKind::RateLimit,
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

/// The last 8 lines of a result.
fn tail(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let kept = lines[lines.len().saturating_sub(8)..].join("\n");
    (!kept.trim().is_empty()).then_some(kept)
}
