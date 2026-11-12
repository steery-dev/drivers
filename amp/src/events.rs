//! `sessions/read`: an Amp thread, as `amp threads export` gives it whole, folded into
//! Steery's events.
//!
//! A thread is a list of messages. A person's message that holds text begins a turn; one
//! that holds only the results of calls continues the turn. The agent's message ends the turn
//! where Amp says why it stopped (`end_turn`, `cancelled`, `error`). Amp writes no time on
//! the agent's messages: an event takes the time of the person's newest message, else the
//! thread's start.
//!
//! The cursor counts the messages read and holds a hash of the last one. Amp lets a person
//! edit an earlier message, which cuts the thread there: a thread that no longer holds the
//! message the cursor stopped after is read again from its start, `reset`. A message Amp
//! still writes (the agent's, streaming, or calls still running) is held back: the cursor
//! stops before it.

use serde_json::Value;
use steery_sdk::wire::harness::{
    Clip, Count, Event, Events, Failure, FailureKind, Read, TurnState, Voice,
};
use steery_sdk::{Error, refusal};

use crate::calls;
use crate::home;
use crate::threads::{self, millis, text};

/// The most bytes of events one answer carries; the rest wait for the next read (`more`).
const ANSWER_CAP: usize = 8 << 20;
/// Amp's site, which a thread's attachments are relative to.
const SITE: &str = "https://ampcode.com";
/// The lines of a call's result kept as its `tail`.
const TAIL_LINES: usize = 8;

/// `sessions/read`.
pub async fn read(read: Read) -> Result<Events, Error> {
    if read.sub_agent.is_some() {
        return Err(Error::refused(
            refusal::NOT_FOUND,
            "the driver reads no sub-agent of an Amp thread",
        ));
    }
    let binary = home::binary().ok_or_else(|| {
        Error::refused(
            refusal::NOT_FOUND,
            "Amp is not installed here, so no thread is",
        )
    })?;
    let thread = threads::export(&binary, &read.external_id).await?;
    Ok(fold(&thread, read.cursor.as_deref(), read.tail))
}

/// Where a cursor stopped: after `n` messages, the last of which hashed to `h`.
#[derive(serde::Serialize, serde::Deserialize)]
struct Cursor {
    n: usize,
    h: String,
}

/// `thread`'s events after `cursor`, or its newest within `tail` bytes.
pub(crate) fn fold(thread: &Value, cursor: Option<&str>, tail: Option<u64>) -> Events {
    let empty = Vec::new();
    let messages = thread
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let end = messages
        .iter()
        .position(unfinished)
        .unwrap_or(messages.len());
    let messages = &messages[..end];
    let (mut from, reset) = match cursor.and_then(|c| serde_json::from_str::<Cursor>(c).ok()) {
        Some(c) if c.n == 0 => (0, false),
        Some(c) if c.n <= messages.len() && hash(&messages[c.n - 1]) == c.h => (c.n, false),
        Some(_) => (0, true),
        None => (0, false),
    };
    let tailed = cursor.is_none() && tail.is_some();
    if let Some(bytes) = tail.filter(|_| cursor.is_none()) {
        from = messages.len();
        let mut size = 0u64;
        while from > 0 {
            let more = serde_json::to_string(&messages[from - 1]).map_or(0, |s| s.len()) as u64;
            if size + more > bytes {
                break;
            }
            size += more;
            from -= 1;
        }
    }
    let mut folder = Folder::new(thread);
    let mut events = Vec::new();
    let mut size = 0;
    let mut upto = from;
    let mut after = None;
    for (index, message) in messages.iter().enumerate() {
        let at = events.len();
        folder.message(index, message, &mut events);
        if index < from {
            events.truncate(at);
            continue;
        }
        let added: usize = events[at..]
            .iter()
            .map(|e| serde_json::to_string(e).map_or(0, |s| s.len()))
            .sum();
        if size + added > ANSWER_CAP && upto > from {
            events.truncate(at);
            break;
        }
        size += added;
        upto = index + 1;
        if let Some(id) = message.get("messageId").and_then(Value::as_u64) {
            after = Some(id.to_string());
        }
    }
    let cursor = Cursor {
        n: upto,
        h: upto
            .checked_sub(1)
            .map(|i| hash(&messages[i]))
            .unwrap_or_default(),
    };
    Events {
        cursor: serde_json::to_string(&cursor).unwrap_or_default(),
        events,
        more: upto < messages.len(),
        reset,
        whole: from == 0,
        after: if tailed { None } else { after },
    }
}

/// Whether Amp still writes `message`: the agent's, streaming, or results of calls that run.
fn unfinished(message: &Value) -> bool {
    if message.pointer("/state/type").and_then(Value::as_str) == Some("streaming") {
        return true;
    }
    blocks(message).any(|block| {
        text(block, "type") == Some("tool_result")
            && !matches!(
                block.pointer("/run/status").and_then(Value::as_str),
                Some("done" | "error" | "cancelled" | "rejected-by-user")
            )
    })
}

/// The content blocks of `message`.
fn blocks(message: &Value) -> impl Iterator<Item = &Value> {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// What the fold knows between messages: the time, the turn that runs, and the model.
struct Folder {
    at: u64,
    turn: Option<String>,
    model: Option<String>,
}

impl Folder {
    /// The fold of `thread`, at its start: its model is the one its tags name.
    fn new(thread: &Value) -> Self {
        let at = thread
            .get("created")
            .and_then(Value::as_u64)
            .or_else(|| text(thread, "updatedAt").and_then(millis))
            .unwrap_or(0);
        let model = thread
            .pointer("/env/initial/tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .find_map(|tag| tag.strip_prefix("model:"))
            .map(str::to_owned);
        Self {
            at,
            turn: None,
            model,
        }
    }

    /// The events of the message at `index`, appended to `events`.
    fn message(&mut self, index: usize, message: &Value, events: &mut Vec<Event>) {
        let key = message
            .get("messageId")
            .and_then(Value::as_u64)
            .map_or_else(|| format!("m{index}"), |id| id.to_string());
        if index == 0
            && let Some(model) = &self.model
        {
            events.push(Event::Model {
                at: self.at,
                model: model.clone(),
                provider: None,
                thinking: None,
            });
        }
        match text(message, "role") {
            Some("user") => self.person(&key, message, events),
            Some("assistant") => self.agent(&key, message, events),
            _ => {}
        }
    }

    /// A person's message: results of calls, then their words, which begin a turn.
    fn person(&mut self, key: &str, message: &Value, events: &mut Vec<Event>) {
        if let Some(sent) = message.pointer("/meta/sentAt").and_then(Value::as_u64) {
            self.at = sent;
        }
        let at = self.at;
        let mut words = String::new();
        let mut clips = Vec::new();
        for block in blocks(message) {
            match text(block, "type") {
                Some("tool_result") => result(at, block, events),
                Some("text") => {
                    if !words.is_empty() {
                        words.push('\n');
                    }
                    words.push_str(text(block, "text").unwrap_or_default());
                }
                Some("image") => clips.extend(clip(block)),
                _ => {}
            }
        }
        if words.is_empty() && clips.is_empty() {
            return;
        }
        if let Some(turn) = self.turn.take() {
            events.push(ended(at, turn, TurnState::Interrupted, None));
        }
        self.turn = Some(key.to_owned());
        events.push(Event::TurnBegan {
            at,
            turn: Some(key.to_owned()),
        });
        events.push(Event::Words {
            at,
            id: key.to_owned(),
            voice: Voice::Person,
            text: words,
            message: None,
            call: None,
            clips,
        });
    }

    /// The agent's message: its words, thoughts and calls, its tokens, and the end of the
    /// turn where Amp says the agent stopped.
    fn agent(&mut self, key: &str, message: &Value, events: &mut Vec<Event>) {
        let at = self.at;
        let usage = message.get("usage");
        if let Some(model) = usage.and_then(|u| text(u, "model"))
            && self.model.as_deref() != Some(model)
        {
            self.model = Some(model.to_owned());
            events.push(Event::Model {
                at,
                model: model.to_owned(),
                provider: None,
                thinking: None,
            });
        }
        for (i, block) in blocks(message).enumerate() {
            let words = |voice, said: &str| Event::Words {
                at,
                id: format!("{key}:{i}"),
                voice,
                text: said.to_owned(),
                message: Some(key.to_owned()),
                call: None,
                clips: Vec::new(),
            };
            match text(block, "type") {
                Some("text") => {
                    let said = text(block, "text").unwrap_or_default();
                    if !said.trim().is_empty() {
                        events.push(words(Voice::Agent, said));
                    }
                }
                Some("thinking") => {
                    let said = text(block, "thinking").unwrap_or_default();
                    if !said.trim().is_empty() {
                        events.push(words(Voice::Thought, said));
                    }
                }
                Some("tool_use") => events.push(Event::CallBegan {
                    at,
                    call: calls::call(
                        text(block, "id").unwrap_or_default(),
                        text(block, "name").unwrap_or_default(),
                        block.get("input").cloned().unwrap_or(Value::Null),
                    ),
                }),
                _ => {}
            }
        }
        if let Some(usage) = usage {
            events.push(tokens(at, key, usage, self.model.clone()));
        }
        let state = message.get("state");
        let ending = match state.and_then(|s| text(s, "type")) {
            Some("complete") if state.and_then(|s| text(s, "stopReason")) == Some("end_turn") => {
                Some((TurnState::Finished, None))
            }
            Some("cancelled") => Some((TurnState::Interrupted, None)),
            Some("error") => Some((TurnState::Failed, Some(failure(state, &self.model)))),
            _ => None,
        };
        if let Some((state, failure)) = ending
            && let Some(turn) = self.turn.take()
        {
            events.push(ended(at, turn, state, failure));
        }
    }
}

fn ended(at: u64, turn: String, state: TurnState, failure: Option<Failure>) -> Event {
    Event::TurnEnded {
        at,
        turn: Some(turn),
        state,
        failure,
    }
}

/// A call's result: its end, then its text as the tool's words.
fn result(at: u64, block: &Value, events: &mut Vec<Event>) {
    let id = text(block, "toolUseID").unwrap_or_default().to_owned();
    let run = block.get("run");
    let status = run.and_then(|r| text(r, "status"));
    let said = run
        .and_then(|r| r.get("result").or_else(|| r.get("error")))
        .map(output)
        .unwrap_or_default();
    events.push(Event::CallEnded {
        at,
        id: id.clone(),
        ok: status == Some("done"),
        chars: said.chars().count() as u64,
        tail: last_lines(&said),
        waited: Vec::new(),
    });
    if !said.is_empty() {
        events.push(Event::Words {
            at,
            id: format!("{id}:result"),
            voice: Voice::Tool,
            text: said,
            message: None,
            call: Some(id),
            clips: Vec::new(),
        });
    }
}

/// The text of a call's result: itself where it is text, its `output` or `message` where it
/// holds one as text, else its JSON.
fn output(result: &Value) -> String {
    match result {
        Value::String(said) => said.clone(),
        Value::Null => String::new(),
        other => ["output", "message"]
            .iter()
            .find_map(|key| text(other, key))
            .map_or_else(|| other.to_string(), str::to_owned),
    }
}

/// The last lines of a result, where it holds any.
fn last_lines(said: &str) -> Option<String> {
    let lines: Vec<&str> = said.lines().collect();
    let kept = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    (!kept.trim().is_empty()).then_some(kept)
}

/// An image a person gave: its bytes where the thread holds them, else its address.
fn clip(block: &Value) -> Option<Clip> {
    let source = block.get("source")?;
    match text(source, "type")? {
        "base64" => Some(Clip::Bytes {
            data: text(source, "data")?.to_owned(),
            media_type: text(source, "mediaType")
                .or_else(|| text(source, "media_type"))
                .map(str::to_owned),
            name: None,
        }),
        "url" => {
            let url = text(source, "url")?;
            Some(Clip::Link {
                url: if url.starts_with('/') {
                    format!("{SITE}{url}")
                } else {
                    url.to_owned()
                },
            })
        }
        _ => None,
    }
}

/// One response's tokens, as Amp counts them.
fn tokens(at: u64, key: &str, usage: &Value, model: Option<String>) -> Event {
    let count = |name| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    let cached = count("cacheReadInputTokens");
    let written = count("cacheCreationInputTokens");
    let input = usage
        .get("totalInputTokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| count("inputTokens") + cached + written);
    let window = count("maxInputTokens");
    Event::Tokens {
        at,
        count: Count::Response,
        response: Some(key.to_owned()),
        model,
        input,
        cached,
        cache_write: Some(written),
        output: count("outputTokens"),
        reasoning: 0,
        context_left: (window > 0).then(|| (1.0 - input as f64 / window as f64).clamp(0.0, 1.0)),
    }
}

/// The failure an agent's message ended in, from Amp's own words for it.
fn failure(state: Option<&Value>, model: &Option<String>) -> Failure {
    let error = state.and_then(|s| s.get("error"));
    let said = error.map(output).unwrap_or_default();
    let line = said
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Amp's turn ended in an error")
        .to_owned();
    let status = error
        .and_then(|e| e.get("status").or_else(|| e.get("statusCode")))
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok());
    let lowered = said.to_lowercase();
    let kind = if status == Some(401) || lowered.contains("unauthorized") {
        FailureKind::SignIn
    } else if matches!(status, Some(429 | 529)) || lowered.contains("rate") {
        FailureKind::RateLimit
    } else if lowered.contains("context") || lowered.contains("too long") {
        FailureKind::ContextFull
    } else {
        FailureKind::Other
    };
    Failure {
        kind,
        line,
        service: status.is_some(),
        http_status: status,
        code: None,
        retry_at: None,
        provider: None,
        model: model.clone(),
        thinking: None,
    }
}

/// A stable hash of `message`, as hex: FNV-1a over its JSON.
fn hash(message: &Value) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in serde_json::to_string(message).unwrap_or_default().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}
