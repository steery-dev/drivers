//! Warp's task messages, read as Steery's events. A turn is Warp's own
//! (`turn_exchange_ids` in `app/src/ai/agent/conversation.rs`): it begins at a person's
//! query and runs to the next. Each request of a turn ends with Warp's server's record of it
//! (`RequestMetadata`, since Warp's protocol of 2026-09-09): its tokens and how it ended. A
//! request that completed with calls still to answer goes on in the next request, which
//! carries their results.

use serde::{Deserialize, Serialize};
use steery_sdk::wire::harness::{
    Call, Compactor, Count, Event, Failure, FailureKind, Group, TurnState, Voice,
};

use crate::task::{Body, Ended, Message, Outcome, Request};

/// What the messages read so far carry on to the next read.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct State {
    /// The id of the message that began the turn not yet ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn: Option<String>,
    /// The newest time read.
    #[serde(default)]
    at: u64,
    /// The model last named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// The calls of the open turn still without a result.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending: Vec<String>,
}

/// Messages folded into events.
pub(crate) struct Fold {
    pub state: State,
    pub events: Vec<Event>,
}

impl Fold {
    pub fn new(state: State) -> Self {
        Self {
            state,
            events: Vec::new(),
        }
    }

    pub fn message(&mut self, message: &Message) {
        let at = message.at.unwrap_or(self.state.at).max(self.state.at);
        self.state.at = at;
        let id = message.id.as_str();
        match &message.body {
            Body::Query { text, clips, .. } => {
                self.end_open();
                self.begin(at, id);
                if !text.trim().is_empty() || !clips.is_empty() {
                    self.words(at, id, Voice::Person, text, clips.clone());
                }
            }
            Body::System { .. } => {
                if self.state.turn.is_none() {
                    self.begin(at, id);
                }
            }
            Body::Output(text) => self.words(at, id, Voice::Agent, text, Vec::new()),
            Body::Reasoning(text) => self.words(at, id, Voice::Thought, text, Vec::new()),
            Body::Call(call) => self.call(at, message, call),
            Body::Result(outcome) => self.ended(at, outcome),
            Body::Summary(Some(text)) => {
                self.events.push(Event::Compacted {
                    at,
                    id: id.to_owned(),
                    // Warp records no mark of who asked for it.
                    by: Compactor::Harness { reason: None },
                    tokens_before: None,
                    tokens_after: None,
                });
                self.words(at, id, Voice::Summary, text, Vec::new());
            }
            Body::Model(model) => {
                if !model.is_empty() && self.state.model.as_ref() != Some(model) {
                    self.state.model = Some(model.clone());
                    self.events.push(Event::Model {
                        at,
                        model: model.clone(),
                        provider: None,
                        thinking: None,
                    });
                }
            }
            Body::Request(request) => self.request(at, &message.request, request),
            Body::Summary(None) | Body::Other => {}
        }
    }

    fn words(
        &mut self,
        at: u64,
        id: &str,
        voice: Voice,
        text: &str,
        clips: Vec<steery_sdk::wire::harness::Clip>,
    ) {
        if text.trim().is_empty() && clips.is_empty() {
            return;
        }
        self.events.push(Event::Words {
            at,
            id: id.to_owned(),
            voice,
            text: text.to_owned(),
            message: None,
            call: None,
            clips,
        });
    }

    fn call(&mut self, at: u64, message: &Message, call: &crate::task::Call) {
        if self.state.turn.is_none() {
            self.begin(at, &message.id);
        }
        // Steery's own server, by the name its entry gives it.
        let steery = call.server.as_deref() == Some("steery");
        let tool = match (steery, call.tool.rsplit_once("__")) {
            (true, Some((_, tool))) => tool.to_owned(),
            _ => call.tool.clone(),
        };
        self.state.pending.push(call.id.clone());
        self.events.push(Event::CallBegan {
            at,
            call: Call {
                id: call.id.clone(),
                tool: tool.clone(),
                input: call.input.clone(),
                group: if steery { Group::Steery } else { call.group },
                server: call.server.clone().filter(|_| !steery),
                steery: steery.then_some(tool),
                asks: call.asks.clone(),
                parent: None,
                response: Some(message.request.clone()).filter(|r| !r.is_empty()),
            },
        });
        if let Some(task) = &call.task {
            self.events.push(Event::SubAgent {
                at,
                external_id: task.clone(),
                call: Some(call.id.clone()),
                state: None,
                error: None,
            });
        }
    }

    /// A call's end, and its output as the tool's words.
    fn ended(&mut self, at: u64, outcome: &Outcome) {
        self.state.pending.retain(|call| *call != outcome.call);
        self.events.push(Event::CallEnded {
            at,
            id: outcome.call.clone(),
            ok: outcome.ok,
            chars: outcome.text.chars().count() as u64,
            tail: tail(&outcome.text),
            waited: Vec::new(),
        });
        if !outcome.text.trim().is_empty() {
            self.events.push(Event::Words {
                at,
                id: format!("{}:output", outcome.call),
                voice: Voice::Tool,
                text: outcome.text.clone(),
                message: None,
                call: Some(outcome.call.clone()),
                clips: Vec::new(),
            });
        }
    }

    /// The server's record of a request: its tokens by model, and how it ended.
    fn request(&mut self, at: u64, id: &str, request: &Request) {
        for (model, [input, cached, cache_write, output]) in &request.tokens {
            self.events.push(Event::Tokens {
                at,
                count: Count::Response,
                response: Some(id.to_owned()).filter(|id| !id.is_empty()),
                model: Some(model.clone()).filter(|m| !m.is_empty()),
                input: *input,
                cached: *cached,
                cache_write: Some(*cache_write),
                output: *output,
                reasoning: 0,
                context_left: None,
            });
        }
        match request.outcome {
            Ended::Completed if self.state.pending.is_empty() => {
                self.end(TurnState::Finished, None);
            }
            Ended::Canceled => self.end(TurnState::Interrupted, None),
            Ended::Errored => {
                let failure = Failure {
                    kind: FailureKind::Other,
                    line: "Warp's server ended the request with an error".into(),
                    service: true,
                    http_status: None,
                    code: None,
                    retry_at: None,
                    provider: None,
                    model: self.state.model.clone(),
                    thinking: None,
                };
                self.end(TurnState::Failed, Some(failure));
            }
            Ended::Completed | Ended::Unspecified => {}
        }
    }

    fn begin(&mut self, at: u64, id: &str) {
        self.state.turn = Some(id.to_owned());
        self.state.pending.clear();
        self.events.push(Event::TurnBegan {
            at,
            turn: Some(id.to_owned()),
        });
    }

    /// The end of a turn still open where the next begins: calls left without a result
    /// were stopped; else the turn's last request kept no record of its end, as before
    /// Warp's protocol of 2026-09-09, and the person went on.
    fn end_open(&mut self) {
        let state = if self.state.pending.is_empty() {
            TurnState::Finished
        } else {
            TurnState::Interrupted
        };
        self.end(state, None);
    }

    fn end(&mut self, state: TurnState, failure: Option<Failure>) {
        let Some(turn) = self.state.turn.take() else {
            return;
        };
        self.state.pending.clear();
        self.events.push(Event::TurnEnded {
            at: self.state.at,
            turn: Some(turn),
            state,
            failure,
        });
    }
}

/// The last 8 lines of a result.
fn tail(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let kept = lines[lines.len().saturating_sub(8)..].join("\n");
    (!kept.trim().is_empty()).then_some(kept)
}
