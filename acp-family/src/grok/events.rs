//! One Grok transcript, `updates.jsonl`, read as Steery's events. Each line is an ACP
//! `session/update` notification (or xAI's `_x.ai/session/update`) with its time:
//! `{"timestamp", "method", "params": {"sessionId", "update", "_meta"}}`, where
//! `_meta.eventId` names the record and `_meta.agentTimestampMs` dates it.
//!
//! Grok writes each streamed chunk of a message as its own line, so the driver joins a run
//! of chunks into one `words` event, named by the run's first record. A run that reaches
//! the end of a transcript still being written stays for the next read.

use std::fs::File;
use std::io::{BufReader, Read as _, Seek as _, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_driver_common::lines::{self, BATCH, Line, RECORD};
use steery_sdk::wire::harness::{
    Call, Clip, Compactor, Count, Event, Events, Execution, Failure, FailureKind, Group, TurnState,
    Voice,
};
use steery_sdk::{Error, refusal};

/// How long a transcript stays new: a run of chunks at its end may still grow.
const FRESH: Duration = Duration::from_secs(10);

/// Where a read stands: the file it was, by device and inode, the offset after the last
/// whole record read, and what the records so far carry on.
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

/// What the records read so far carry on to the next.
#[derive(Clone, Default, Serialize, Deserialize)]
struct State {
    /// The `promptIndex` of the newest prompt on the current history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt: Option<u64>,
    /// Some prompt carried a `promptIndex`: from then on only marked prompts are a person's.
    #[serde(default)]
    marked: bool,
    /// A turn began and did not end.
    #[serde(default)]
    open: bool,
    /// The id of the next turn, as a hook's `user_prompt_submit` named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// Why the open turn's requests failed, as the latest `retry_state` said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<Failure>,
    /// An automatic compaction began, and the reason it gave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compacting: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<u64>,
    /// Records copied from the parent of a fork were read, and the fork was said.
    #[serde(default)]
    inherited: bool,
    #[serde(default)]
    forked: bool,
}

/// A run of chunks of one message, not said yet.
struct Run {
    at: u64,
    id: String,
    voice: Voice,
    /// What tells its chunks from another message's: the turn, or the prompt's index.
    key: Option<String>,
    text: String,
    clips: Vec<Clip>,
    /// Where its first record starts, and the state before it.
    start: u64,
    state: State,
    newest: Option<String>,
    /// How many events came before it: a run held back takes back what came after.
    events: usize,
}

/// The events of `path` after `cursor`, or of its newest `tail` bytes. `parent` is the
/// conversation that this one was forked from: records it copied from there come first.
pub fn read(
    path: &Path,
    cursor: Option<&str>,
    tail: Option<u64>,
    parent: Option<&str>,
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
    let fresh = meta
        .modified()
        .ok()
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .is_none_or(|age| age < FRESH);
    let mut reset = false;
    let (start, state, tailed) = match (tail, cursor) {
        (Some(tail), _) => {
            let from = length.saturating_sub(tail.min(BATCH));
            (
                lines::first_whole(&open, from, length).map_err(unreadable)?,
                State::default(),
                true,
            )
        }
        (None, Some(cursor)) => {
            let cursor = Cursor::parse(cursor)?;
            if cursor.dev == dev && cursor.ino == ino && cursor.off <= length {
                (cursor.off, cursor.st, false)
            } else {
                reset = true;
                (0, State::default(), false)
            }
        }
        (None, None) => (0, State::default(), false),
    };
    let mut fold = Fold {
        parent,
        state,
        events: Vec::new(),
        run: None,
        newest: None,
        last: None,
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
                if let Ok(record) = serde_json::from_slice::<Value>(&line) {
                    fold.record(at, &record);
                }
            }
        }
        at = end;
    }
    // A run at the end of a transcript still written may grow: it stays for the next read,
    // unless it alone makes up a read that stopped at its bound.
    if let Some(run) = fold.run.take() {
        if (fresh && !more) || (more && run.start > start) {
            at = run.start;
            fold.state = run.state;
            fold.newest = run.newest;
            fold.events.truncate(run.events);
        } else {
            fold.say(run);
        }
    }
    let after = if tailed { None } else { fold.newest };
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
        after,
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

/// The records of one read, folded into events.
struct Fold<'a> {
    parent: Option<&'a str>,
    state: State,
    events: Vec<Event>,
    run: Option<Run>,
    /// The id of the newest record read, and of the one before the record being read.
    newest: Option<String>,
    last: Option<String>,
}

impl Fold<'_> {
    fn record(&mut self, start: u64, record: &Value) {
        let params = &record["params"];
        let meta = &params["_meta"];
        let update = &params["update"];
        let at = meta["agentTimestampMs"]
            .as_u64()
            .or_else(|| record["timestamp"].as_u64().map(|seconds| seconds * 1000))
            .unwrap_or(0);
        self.last = self.newest.clone();
        let id = match meta["eventId"].as_str() {
            Some(id) => {
                self.newest = Some(id.to_owned());
                id.to_owned()
            }
            None => format!("@{start}"),
        };
        if let Some(parent) = self.parent
            && let Some(event) = meta["eventId"].as_str()
        {
            if event
                .strip_prefix(parent)
                .is_some_and(|rest| rest.starts_with('-'))
            {
                self.state.inherited = true;
            } else if self.state.inherited && !self.state.forked {
                self.flush();
                self.state.forked = true;
                self.events.push(Event::Fork {
                    at,
                    parent: parent.to_owned(),
                });
            }
        }
        let Some(kind) = update["sessionUpdate"].as_str() else {
            return;
        };
        match kind {
            "user_message_chunk" => self.person(start, at, id, update),
            "agent_message_chunk" | "agent_thought_chunk" => {
                let voice = if kind == "agent_message_chunk" {
                    Voice::Agent
                } else {
                    Voice::Thought
                };
                let key = meta["promptId"].as_str().map(str::to_owned);
                let text = update["content"]["text"].as_str().unwrap_or_default();
                self.chunk(start, at, id, voice, key, text, None);
            }
            "tool_call" => {
                self.flush();
                self.events.push(Event::CallBegan {
                    at,
                    call: call(update),
                });
            }
            "tool_call_update" => self.ended(at, id, update),
            "turn_completed" => self.turn_completed(at, update),
            "hook_execution" => {
                if update["event_name"] == "user_prompt_submit"
                    && let Some(prompt) = update["prompt_id"].as_str()
                {
                    self.state.next = Some(prompt.to_owned());
                }
            }
            "model_changed" => {
                self.flush();
                if let Some(model) = update["model_id"].as_str() {
                    self.state.model = Some(model.to_owned());
                    self.events.push(Event::Model {
                        at,
                        model: model.to_owned(),
                        provider: None,
                        thinking: update["reasoning_effort"].as_str().map(str::to_owned),
                    });
                }
            }
            "retry_state" => self.state.failure = failure(update).or(self.state.failure.take()),
            "auto_compact_started" => {
                self.state.compacting = Some(update["reason"].as_str().map(str::to_owned));
            }
            "auto_compact_completed" => {
                self.state.before = update["tokens_before"].as_u64();
                self.state.after = update["tokens_after"].as_u64();
            }
            "compaction_checkpoint" => {
                self.flush();
                let by = match self.state.compacting.take() {
                    Some(reason) => Compactor::Harness { reason },
                    None => Compactor::Person,
                };
                self.events.push(Event::Compacted {
                    at,
                    id: update["checkpoint_id"].as_str().map_or(id, str::to_owned),
                    by,
                    tokens_before: self.state.before.take(),
                    tokens_after: self.state.after.take(),
                });
            }
            "rewind_marker" => {
                self.flush();
                let Some(target) = update["target_prompt_index"].as_u64() else {
                    return;
                };
                let turns = self
                    .state
                    .prompt
                    .map_or(0, |prompt| (prompt + 1).saturating_sub(target));
                self.state.prompt = target.checked_sub(1);
                if turns > 0 {
                    self.events.push(Event::Rollback {
                        at,
                        turns: u32::try_from(turns).unwrap_or(u32::MAX),
                    });
                }
            }
            "subagent_spawned" => {
                self.flush();
                if let Some(child) = update["subagent_id"].as_str() {
                    self.events.push(Event::SubAgent {
                        at,
                        external_id: child.to_owned(),
                        call: None,
                        state: Some(Execution::Running),
                        error: None,
                    });
                }
            }
            "subagent_finished" => {
                self.flush();
                let child = update["child_session_id"]
                    .as_str()
                    .or_else(|| update["subagent_id"].as_str());
                if let Some(child) = child {
                    self.events.push(Event::SubAgent {
                        at,
                        external_id: child.to_owned(),
                        call: None,
                        state: execution(update["status"].as_str().unwrap_or_default()),
                        error: update["error"].as_str().map(str::to_owned),
                    });
                }
            }
            _ => {}
        }
    }

    /// A chunk of what a person typed: the first of a prompt begins a turn.
    fn person(&mut self, start: u64, at: u64, id: String, update: &Value) {
        let index = update["_meta"]["promptIndex"].as_u64();
        // After the first marked prompt, an unmarked user chunk is not a person's prompt.
        if index.is_none() && self.state.marked {
            self.flush();
            return;
        }
        let key = index.map(|index| index.to_string());
        let continues = self
            .run
            .as_ref()
            .is_some_and(|run| matches!(run.voice, Voice::Person) && run.key == key);
        // What a held run takes back: the turn it began too.
        let before = (!continues).then(|| {
            self.flush();
            (self.state.clone(), self.events.len())
        });
        if before.is_some() {
            if self.state.open {
                self.events.push(Event::TurnEnded {
                    at,
                    turn: None,
                    state: TurnState::Interrupted,
                    failure: None,
                });
            }
            if let Some(model) = update["_meta"]["modelId"].as_str()
                && self.state.model.as_deref() != Some(model)
            {
                self.state.model = Some(model.to_owned());
                self.events.push(Event::Model {
                    at,
                    model: model.to_owned(),
                    provider: None,
                    thinking: None,
                });
            }
            self.state.marked |= index.is_some();
            self.state.prompt =
                index.or_else(|| self.state.prompt.map_or(Some(0), |p| Some(p + 1)));
            self.state.open = true;
            self.state.failure = None;
            self.events.push(Event::TurnBegan {
                at,
                turn: self.state.next.take(),
            });
        }
        let content = &update["content"];
        let clip = clip(content);
        let text = match content["type"].as_str() {
            Some("text") => content["text"].as_str().unwrap_or_default(),
            Some("resource") => content["resource"]["text"].as_str().unwrap_or_default(),
            _ => "",
        };
        self.chunk(start, at, id, Voice::Person, key, text, clip);
        if let Some((state, events)) = before
            && let Some(run) = &mut self.run
        {
            run.state = state;
            run.events = events;
        }
    }

    /// One chunk of a message: it continues the run of its message, or begins one.
    #[allow(clippy::too_many_arguments)]
    fn chunk(
        &mut self,
        start: u64,
        at: u64,
        id: String,
        voice: Voice,
        key: Option<String>,
        text: &str,
        clip: Option<Clip>,
    ) {
        if let Some(run) = &mut self.run
            && run.voice == voice
            && run.key == key
        {
            run.text.push_str(text);
            run.clips.extend(clip);
            return;
        }
        self.flush();
        self.run = Some(Run {
            at,
            id,
            voice,
            key,
            text: text.to_owned(),
            clips: clip.into_iter().collect(),
            start,
            state: self.state.clone(),
            newest: self.last.clone(),
            events: self.events.len(),
        });
    }

    /// Say the run of chunks that the record before ended, if any.
    fn flush(&mut self) {
        if let Some(run) = self.run.take() {
            self.say(run);
        }
    }

    fn say(&mut self, run: Run) {
        if run.text.trim().is_empty() && run.clips.is_empty() {
            return;
        }
        self.events.push(Event::Words {
            at: run.at,
            id: run.id,
            voice: run.voice,
            text: run.text,
            message: None,
            call: None,
            clips: run.clips,
        });
    }

    /// A call's update: where it finished, its end and the text of its result.
    fn ended(&mut self, at: u64, id: String, update: &Value) {
        let ok = match update["status"].as_str() {
            Some("completed") => true,
            Some("failed") => false,
            _ => return,
        };
        let Some(call) = update["toolCallId"].as_str() else {
            return;
        };
        self.flush();
        let text = result(update);
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
                id,
                voice: Voice::Tool,
                text,
                message: None,
                call: Some(call.to_owned()),
                clips: Vec::new(),
            });
        }
    }

    /// The end of a turn, with what its responses spent.
    fn turn_completed(&mut self, at: u64, update: &Value) {
        self.flush();
        let turn = update["prompt_id"].as_str().map(str::to_owned);
        let usage = &update["usage"];
        if usage.is_object() {
            let number = |key: &str| usage[key].as_u64().unwrap_or(0);
            let model = usage["modelUsage"]
                .as_object()
                .and_then(|models| models.keys().next().cloned())
                .or_else(|| self.state.model.clone());
            self.events.push(Event::Tokens {
                at,
                count: Count::Response,
                response: turn.clone(),
                model,
                input: number("inputTokens"),
                cached: number("cachedReadTokens"),
                cache_write: usage["cacheCreationTokens"].as_u64(),
                output: number("outputTokens"),
                reasoning: number("reasoningTokens"),
                context_left: None,
            });
        }
        let stop = update["stop_reason"].as_str().unwrap_or_default();
        let (state, failure) = match stop {
            "cancelled" => (TurnState::Interrupted, None),
            "error" => {
                let line = update["error_kind"].as_str().unwrap_or("error").to_owned();
                let failure = self.state.failure.take().unwrap_or(Failure {
                    kind: FailureKind::Other,
                    line,
                    service: false,
                    http_status: None,
                    code: None,
                    retry_at: None,
                    provider: None,
                    model: None,
                    thinking: None,
                });
                (TurnState::Failed, Some(failure))
            }
            _ => (TurnState::Finished, None),
        };
        self.state.open = false;
        self.state.failure = None;
        self.events.push(Event::TurnEnded {
            at,
            turn,
            state,
            failure,
        });
    }
}

/// What a person's chunk carried besides its text.
fn clip(content: &Value) -> Option<Clip> {
    let text = |key: &str| content[key].as_str().map(str::to_owned);
    match content["type"].as_str()? {
        "image" | "audio" => Some(Clip::Bytes {
            data: text("data")?,
            media_type: text("mimeType"),
            name: None,
        }),
        "resource_link" => {
            let uri = content["uri"].as_str()?;
            Some(match uri.strip_prefix("file://") {
                Some(path) => Clip::Path {
                    path: path.into(),
                    name: text("name"),
                },
                None => Clip::Link {
                    url: uri.to_owned(),
                },
            })
        }
        _ => None,
    }
}

/// The call a `tool_call` began. Grok names the tool and its kind in `_meta["x.ai/tool"]`;
/// an MCP tool is `<server>__<tool>` there.
fn call(update: &Value) -> Call {
    let tool = &update["_meta"]["x.ai/tool"];
    let name = tool["name"]
        .as_str()
        .or_else(|| update["title"].as_str())
        .unwrap_or_default();
    let mcp = tool["namespace"] == "mcp" || name.starts_with("mcp__");
    let served = mcp
        .then(|| name.strip_prefix("mcp__").unwrap_or(name).split_once("__"))
        .flatten();
    let (tool_name, group, server, steery) = match served {
        Some((server, inner)) => {
            let spelled = format!("mcp__{server}__{inner}");
            if server == "steery" {
                (spelled, Group::Steery, None, Some(inner.to_owned()))
            } else {
                (spelled, Group::Mcp, Some(server.to_owned()), None)
            }
        }
        None => (
            name.to_owned(),
            group(tool["kind"].as_str().unwrap_or_default()),
            None,
            None,
        ),
    };
    Call {
        id: update["toolCallId"].as_str().unwrap_or_default().to_owned(),
        tool: tool_name,
        input: update.get("rawInput").cloned().unwrap_or(Value::Null),
        group,
        server,
        steery,
        asks: None,
        parent: None,
        response: None,
    }
}

/// The group of a Grok tool by its kind (`ToolKind`, snake case).
fn group(kind: &str) -> Group {
    match kind {
        "read" | "list_dir" | "list" => Group::FilesRead,
        "edit" | "delete" | "write" | "move" => Group::FilesEdit,
        "search" => Group::Search,
        "lsp" => Group::Code,
        "execute" | "background_task_action" | "wait_tasks" | "kill_task_action" | "monitor" => {
            Group::Shell
        }
        "plan" | "enter_plan" | "exit_plan" | "goal_update" => Group::Plan,
        "web_search" | "web_fetch" => Group::Web,
        "memory_search" | "memory_get" => Group::Memory,
        "task" | "active_agent_message" => Group::Agents,
        "ask_user" => Group::Ask,
        "image_gen" | "video_gen" | "image_to_video" | "reference_to_video" => Group::Media,
        "skill" | "search_tool" | "use_tool" | "workflow" | "feedback" => Group::Harness,
        _ => Group::Custom,
    }
}

/// The text of a call's result: its content, else what the model was given of it.
fn result(update: &Value) -> String {
    let parts: Vec<&str> = update["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["content"]["text"].as_str())
        .collect();
    if !parts.is_empty() {
        return parts.join("\n");
    }
    let raw = &update["rawOutput"];
    raw["output_for_prompt"]
        .as_str()
        .or_else(|| raw.as_str())
        .unwrap_or_default()
        .to_owned()
}

/// The last 8 lines of a result.
fn tail(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let kept = lines[lines.len().saturating_sub(8)..].join("\n");
    (!kept.trim().is_empty()).then_some(kept)
}

/// Why a turn's requests failed, from a `retry_state` that gave up.
fn failure(update: &Value) -> Option<Failure> {
    let (kind, line) = match update["type"].as_str()? {
        "failed" => {
            let kind = match update["error_type"].as_str().unwrap_or_default() {
                "auth" => FailureKind::SignIn,
                "context_length" => FailureKind::ContextFull,
                "rate_limit" | "rate_limited" => FailureKind::RateLimit,
                _ => FailureKind::Other,
            };
            (kind, update["message"].as_str())
        }
        "exhausted" => {
            let kind = if update["is_rate_limited"] == true {
                FailureKind::RateLimit
            } else {
                FailureKind::Other
            };
            (kind, update["reason"].as_str())
        }
        _ => return None,
    };
    Some(Failure {
        kind,
        line: line
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
        service: true,
        http_status: None,
        code: None,
        retry_at: None,
        provider: None,
        model: None,
        thinking: None,
    })
}

/// A sub-agent's state, by the status Grok gave its end.
fn execution(status: &str) -> Option<Execution> {
    match status {
        "completed" | "finished" | "success" => Some(Execution::Finished),
        "failed" | "error" => Some(Execution::Failed),
        "cancelled" | "interrupted" | "killed" => Some(Execution::Interrupted),
        "running" => Some(Execution::Running),
        _ => None,
    }
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
