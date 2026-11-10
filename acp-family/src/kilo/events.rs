//! What Kilo's messages say, as Steery's events. A person's message (`role: user`) starts a
//! turn; each model response is an assistant message, whose parts are its reasoning, its
//! text, its tool calls and its `step-finish` count; the turn ends at the first assistant
//! message that finishes for any reason but `tool-calls`, or fails. A message is read once
//! it is whole: a person's at once, an assistant's once it completed or failed. Reading
//! stops before the first assistant message still running.

use std::path::PathBuf;

use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    Clip, Compactor, Count, Event, Execution, Failure, FailureKind, TurnState, Voice,
};

use super::calls;

/// One message with its parts, as Kilo keeps them.
pub struct Message {
    pub id: String,
    pub created: u64,
    pub info: Value,
    pub parts: Vec<(String, Value)>,
}

/// Where a read ended: the last message it read, the turn still open there and the model
/// it last said.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub message: Option<String>,
    turn: Option<String>,
    model: Option<String>,
}

impl Cursor {
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = || format!("not a cursor of this driver: {text}");
        let fields: [Option<String>; 3] = serde_json::from_str(text).map_err(|_| bad())?;
        let [message, turn, model] = fields;
        Ok(Self {
            message,
            turn,
            model,
        })
    }
}

impl std::fmt::Display for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", json!([self.message, self.turn, self.model]))
    }
}

/// The events of `messages`, from `cursor` on, which moves past each message read; `servers`
/// are the MCP servers Kilo's configuration names. How many messages it read: it stops
/// before an assistant message that is still running.
pub fn fold(
    cursor: &mut Cursor,
    messages: Vec<Message>,
    servers: &[String],
) -> (Vec<Event>, usize) {
    let mut out = Vec::new();
    let mut read = 0;
    for message in messages {
        match message.info.get("role").and_then(Value::as_str) {
            Some("user") => person(cursor, &message, &mut out),
            Some("assistant") => {
                if !ended(&message.info) {
                    break;
                }
                agent(cursor, &message, servers, &mut out);
            }
            _ => {}
        }
        cursor.message = Some(message.id);
        read += 1;
    }
    (out, read)
}

/// Whether an assistant message is whole: it completed or failed.
fn ended(info: &Value) -> bool {
    info.pointer("/time/completed")
        .is_some_and(|at| !at.is_null())
        || info.get("error").is_some_and(|error| !error.is_null())
}

fn person(cursor: &mut Cursor, message: &Message, out: &mut Vec<Event>) {
    let at = message
        .info
        .pointer("/time/created")
        .and_then(Value::as_u64)
        .unwrap_or(message.created);
    for (id, part) in &message.parts {
        if kind(part) == Some("compaction") {
            let by = match part.get("auto").and_then(Value::as_bool) {
                Some(true) => Compactor::Harness {
                    reason: Some(
                        match part.get("overflow").and_then(Value::as_bool) {
                            Some(true) => "overflow",
                            _ => "threshold",
                        }
                        .to_owned(),
                    ),
                },
                _ => Compactor::Person,
            };
            out.push(Event::Compacted {
                at,
                id: id.clone(),
                by,
                tokens_before: None,
                tokens_after: None,
            });
        }
    }
    // What the person sent: the text and files Kilo did not add itself.
    let typed: Vec<(&String, &str)> = message
        .parts
        .iter()
        .filter(|(_, part)| {
            kind(part) == Some("text") && !flag(part, "synthetic") && !flag(part, "ignored")
        })
        .filter_map(|(id, part)| Some((id, part.get("text")?.as_str()?)))
        .collect();
    let clips: Vec<Clip> = message
        .parts
        .iter()
        .filter(|(_, part)| kind(part) == Some("file"))
        .filter_map(|(_, part)| clip(part))
        .collect();
    if typed.is_empty() && clips.is_empty() {
        return;
    }
    if cursor.turn.is_none() {
        out.push(Event::TurnBegan {
            at,
            turn: Some(message.id.clone()),
        });
        cursor.turn = Some(message.id.clone());
    }
    model(cursor, at, message.info.get("model"), out);
    let mut clips = Some(clips);
    match typed.as_slice() {
        [] => out.push(words(
            at,
            message.id.clone(),
            Voice::Person,
            String::new(),
            &message.id,
            None,
            clips.take(),
        )),
        typed => {
            for (id, text) in typed {
                out.push(words(
                    at,
                    (*id).clone(),
                    Voice::Person,
                    (*text).to_owned(),
                    &message.id,
                    None,
                    clips.take(),
                ));
            }
        }
    }
}

fn agent(cursor: &mut Cursor, message: &Message, servers: &[String], out: &mut Vec<Event>) {
    let info = &message.info;
    let created = info
        .pointer("/time/created")
        .and_then(Value::as_u64)
        .unwrap_or(message.created);
    let completed = info
        .pointer("/time/completed")
        .and_then(Value::as_u64)
        .unwrap_or(created);
    let summary = flag(info, "summary");
    model(
        cursor,
        created,
        Some(&json!({
            "providerID": info.get("providerID"),
            "modelID": info.get("modelID"),
        })),
        out,
    );
    let selector = selector(info.get("providerID"), info.get("modelID"));
    for (id, part) in &message.parts {
        let at = part
            .pointer("/time/start")
            .and_then(Value::as_u64)
            .unwrap_or(created);
        match kind(part) {
            Some("reasoning") => {
                if let Some(text) = said(part.get("text")) {
                    out.push(words(
                        at,
                        id.clone(),
                        Voice::Thought,
                        text,
                        &message.id,
                        None,
                        None,
                    ));
                }
            }
            Some("text") if !flag(part, "ignored") => {
                if let Some(text) = said(part.get("text")) {
                    let voice = if summary {
                        Voice::Summary
                    } else {
                        Voice::Agent
                    };
                    out.push(words(at, id.clone(), voice, text, &message.id, None, None));
                }
            }
            Some("tool") => call(id, part, &message.id, (created, completed), servers, out),
            Some("step-finish") => {
                let Some(tokens) = part.get("tokens") else {
                    continue;
                };
                let count = |pointer: &str| {
                    tokens
                        .pointer(pointer)
                        .and_then(Value::as_f64)
                        .map_or(0, |count| count.max(0.0) as u64)
                };
                let (read, written) = (count("/cache/read"), count("/cache/write"));
                out.push(Event::Tokens {
                    at: part
                        .pointer("/time/end")
                        .and_then(Value::as_u64)
                        .unwrap_or(completed),
                    count: Count::Response,
                    response: Some(message.id.clone()),
                    model: part
                        .get("model")
                        .and_then(selector_of)
                        .or_else(|| selector.clone()),
                    input: count("/input") + read + written,
                    cached: read,
                    cache_write: Some(written),
                    output: count("/output"),
                    reasoning: count("/reasoning"),
                    context_left: None,
                });
            }
            _ => {}
        }
    }
    let error = info.get("error").filter(|error| !error.is_null());
    let finish = info.get("finish").and_then(Value::as_str);
    if error.is_none() && matches!(finish, Some("tool-calls") | None) {
        return;
    }
    let Some(turn) = cursor.turn.take() else {
        return;
    };
    let (state, failure) = match error {
        None => (TurnState::Finished, None),
        Some(error) if name(error) == Some("MessageAbortedError") => (TurnState::Interrupted, None),
        Some(error) => (TurnState::Failed, Some(failure(error, info, &selector))),
    };
    out.push(Event::TurnEnded {
        at: completed,
        turn: Some(turn),
        state,
        failure,
    });
}

/// A tool part: the call, and its end where it ended.
fn call(
    id: &str,
    part: &Value,
    message: &str,
    (created, completed): (u64, u64),
    servers: &[String],
    out: &mut Vec<Event>,
) {
    let Some(call_id) = part.get("callID").and_then(Value::as_str) else {
        return;
    };
    let tool = part.get("tool").and_then(Value::as_str).unwrap_or_default();
    let state = part.get("state").unwrap_or(&Value::Null);
    let input = state.get("input").cloned().unwrap_or(Value::Null);
    let began = state
        .pointer("/time/start")
        .and_then(Value::as_u64)
        .unwrap_or(created);
    let mut call = calls::call(call_id, tool, input, servers);
    call.response = Some(message.to_owned());
    out.push(Event::CallBegan { at: began, call });
    let status = state.get("status").and_then(Value::as_str);
    let ok = status == Some("completed");
    // A call still pending or running in a message that ended was cut off with it.
    let result = match status {
        Some("completed") => said(state.get("output")).unwrap_or_default(),
        _ => said(state.get("error")).unwrap_or_default(),
    };
    let ended = state
        .pointer("/time/end")
        .and_then(Value::as_u64)
        .unwrap_or(completed);
    let lines: Vec<&str> = result.lines().collect();
    let tail = lines[lines.len().saturating_sub(8)..].join("\n");
    out.push(Event::CallEnded {
        at: ended,
        id: call_id.to_owned(),
        ok,
        chars: result.chars().count() as u64,
        tail: (!tail.trim().is_empty()).then_some(tail),
        waited: Vec::new(),
    });
    if tool == "task"
        && let Some(sub) = state.pointer("/metadata/sessionId").and_then(Value::as_str)
    {
        out.push(Event::SubAgent {
            at: ended,
            external_id: sub.to_owned(),
            call: Some(call_id.to_owned()),
            state: Some(match status {
                Some("completed") => Execution::Finished,
                Some("error") => Execution::Failed,
                Some("pending" | "running") => Execution::Interrupted,
                _ => Execution::Interrupted,
            }),
            error: (!ok)
                .then(|| result.clone())
                .filter(|error| !error.is_empty()),
        });
    }
    if !result.trim().is_empty() {
        out.push(words(
            ended,
            id.to_owned(),
            Voice::Tool,
            result,
            message,
            Some(call_id),
            None,
        ));
    }
}

/// The model a message names, said where it differs from the last one said.
fn model(cursor: &mut Cursor, at: u64, model: Option<&Value>, out: &mut Vec<Event>) {
    let Some(model) = model else {
        return;
    };
    let Some(selector) = selector_of(model) else {
        return;
    };
    if cursor.model.as_deref() == Some(selector.as_str()) {
        return;
    }
    cursor.model = Some(selector.clone());
    out.push(Event::Model {
        at,
        model: selector,
        provider: model
            .get("providerID")
            .and_then(Value::as_str)
            .filter(|provider| !provider.is_empty())
            .map(str::to_owned),
        thinking: model
            .get("variant")
            .and_then(Value::as_str)
            .filter(|variant| !variant.is_empty() && *variant != "default")
            .map(str::to_owned),
    });
}

/// `{providerID, modelID}` as `<provider>/<model>`, Kilo's own spelling of a model.
fn selector_of(model: &Value) -> Option<String> {
    selector(model.get("providerID"), model.get("modelID"))
}

fn selector(provider: Option<&Value>, model: Option<&Value>) -> Option<String> {
    let model = model?.as_str().filter(|model| !model.is_empty())?;
    Some(
        match provider
            .and_then(Value::as_str)
            .filter(|provider| !provider.is_empty())
        {
            Some(provider) => format!("{provider}/{model}"),
            None => model.to_owned(),
        },
    )
}

/// Why a turn failed, from the error Kilo kept on its message: its `name`, and the HTTP
/// status and retry of an `APIError`. Never read from wording.
fn failure(error: &Value, info: &Value, model: &Option<String>) -> Failure {
    let data = error.get("data").unwrap_or(&Value::Null);
    let http_status = data
        .get("statusCode")
        .and_then(Value::as_u64)
        .filter(|status| (100..=599).contains(status))
        .and_then(|status| u16::try_from(status).ok());
    let code = name(error).map(str::to_owned);
    let kind = match (code.as_deref(), http_status) {
        (Some("ContextOverflowError"), _) => FailureKind::ContextFull,
        (Some("ProviderAuthError"), _) | (_, Some(401)) => FailureKind::SignIn,
        (_, Some(429 | 503 | 529)) => FailureKind::RateLimit,
        _ => FailureKind::Other,
    };
    let line = data
        .get("message")
        .and_then(Value::as_str)
        .or(code.as_deref())
        .unwrap_or_default();
    Failure {
        kind,
        line: line.lines().next().unwrap_or_default().trim().to_owned(),
        service: http_status.is_some() || code.as_deref() == Some("APIError"),
        http_status,
        code,
        retry_at: None,
        provider: info
            .get("providerID")
            .and_then(Value::as_str)
            .filter(|provider| !provider.is_empty())
            .map(str::to_owned),
        model: model.clone(),
        thinking: None,
    }
}

fn words(
    at: u64,
    id: String,
    voice: Voice,
    text: String,
    message: &str,
    call: Option<&str>,
    clips: Option<Vec<Clip>>,
) -> Event {
    Event::Words {
        at,
        message: (id != message).then(|| message.to_owned()),
        id,
        voice,
        text,
        call: call.map(str::to_owned),
        clips: clips.unwrap_or_default(),
    }
}

/// A file part: bytes inline as a `data:` URL, a file on this host as a `file:` URL, or
/// any other address.
fn clip(part: &Value) -> Option<Clip> {
    let url = part.get("url")?.as_str()?;
    let media_type = part
        .get("mime")
        .and_then(Value::as_str)
        .filter(|mime| !mime.is_empty())
        .map(str::to_owned);
    let name = part
        .get("filename")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    if let Some(inline) = url.strip_prefix("data:") {
        let (_, data) = inline.split_once(";base64,")?;
        return Some(Clip::Bytes {
            data: data.to_owned(),
            media_type,
            name,
        });
    }
    if let Some(path) = url.strip_prefix("file://") {
        return Some(Clip::File {
            path: PathBuf::from(percent_decoded(path)),
            media_type,
            name,
        });
    }
    Some(Clip::Link {
        url: url.to_owned(),
    })
}

/// A `file:` URL's path with its `%XX` escapes decoded.
fn percent_decoded(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escaped = (bytes[at] == b'%')
            .then(|| bytes.get(at + 1..at + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn kind(part: &Value) -> Option<&str> {
    part.get("type").and_then(Value::as_str)
}

fn name(error: &Value) -> Option<&str> {
    error.get("name").and_then(Value::as_str)
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool) == Some(true)
}

/// A text that says something, else none.
fn said(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
}
