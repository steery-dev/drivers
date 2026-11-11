//! Copilot's own door (PROTOCOL.md §4.3): a Copilot window started with `--ui-server`
//! ([`super::window`]) takes words (`session.send`), a model (`session.model.switchTo`) and
//! a stop (`session.abort`) through its server. Its event log (`session.eventLog.read`, which
//! also carries the events Copilot never writes to disk) says what runs, and the driver sends
//! that as `session/changed`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    self, Dialog, DialogKind, Holder, HolderStop, LiveCall, LiveChild, LiveEnded, LiveRetry,
    LiveSettled, LiveState, LiveTurn, Pick, Picked, Said, Say, SayMode, SessionChanged, Stop,
    Stopped,
};
use steery_sdk::{Error, Notifier, method, refusal};

use super::events::millis;
use super::transcripts::Store;
use super::{calls, rpc, window};

/// How long a window has to answer a request.
const WITHIN: Duration = Duration::from_secs(10);
/// How long one read of the event log waits for new events. Copilot logs no event when
/// a turn's work goes idle (`session.idle` is never written), so while a turn runs the
/// read waits briefly and `isProcessing` is asked again.
const WAIT_MS: u64 = 25_000;
const WAIT_RUNNING_MS: u64 = 1_000;
/// How many of the newest events a door reads first to learn the turn that runs.
const SEED: u64 = 400;
/// How many events one read takes at most.
const BATCH: u64 = 500;
/// How many event ids a door keeps to read each event once.
const SEEN: usize = 4096;
/// How many say ids a door keeps as Steery's.
const SENT: usize = 64;
/// How many ended calls and settled turns a state carries (PROTOCOL.md §4.3).
const ENDED: usize = 64;
const SETTLED: usize = 16;
/// The longest retry error and dialog title a state carries.
const ERROR_AT_MOST: usize = 600;
const TITLE_AT_MOST: usize = 200;

/// Every door the driver follows, by conversation.
pub struct Live {
    notifier: Notifier,
    doors: Mutex<HashMap<String, Arc<Followed>>>,
}

/// One window that shows a conversation, and what its event log said.
pub struct Followed {
    id: String,
    client: rpc::Client,
    pid: u32,
    port: u16,
    fold: Mutex<Fold>,
}

impl Live {
    pub fn new(notifier: Notifier) -> Self {
        Self {
            notifier,
            doors: Mutex::default(),
        }
    }

    /// The door of conversation `id`: the one followed already while its window answers,
    /// else the window found now, which the driver then follows.
    async fn door(self: &Arc<Self>, store: &Store, id: &str) -> Option<Arc<Followed>> {
        if let Some(door) = lock(&self.doors)
            .get(id)
            .filter(|door| !door.client.ended())
        {
            return Some(Arc::clone(door));
        }
        let transcript = store.transcript(id).ok()?;
        let window = window::find(transcript.parent()?, id).await?;
        let found = Arc::new(Followed {
            id: id.to_owned(),
            client: window.client,
            pid: window.pid,
            port: window.port,
            fold: Mutex::default(),
        });
        // Two asks that raced keep the first door; the other's connection closes with it.
        let door = Arc::clone(
            lock(&self.doors)
                .entry(id.to_owned())
                .and_modify(|held| {
                    if held.client.ended() {
                        *held = Arc::clone(&found);
                    }
                })
                .or_insert_with(|| Arc::clone(&found)),
        );
        if Arc::ptr_eq(&door, &found) {
            let live = Arc::clone(self);
            let followed = Arc::clone(&door);
            tokio::spawn(async move { live.follow(followed).await });
        }
        Some(door)
    }

    /// `held`: the window's owner, with the turn that runs as its token; none where no
    /// window of Copilot's own server shows the conversation.
    pub async fn held(self: &Arc<Self>, store: &Store, id: &str) -> Option<Holder> {
        let door = self.door(store, id).await?;
        let running = door.processing().await.ok()?;
        let token = running.then(|| door.token()).flatten();
        Some(Holder::Person {
            door: Some(harness::Door::Picks),
            stop: HolderStop::Owner(token),
        })
    }

    /// `say`: the words into the window's conversation, named by the say's id so its turn
    /// can be told.
    pub async fn say(self: &Arc<Self>, store: &Store, say: Say) -> Result<Said, Error> {
        let Some(door) = self.door(store, &say.external_id).await else {
            return Err(Error::refused(
                refusal::NO_DOOR,
                "no Copilot window started with `--ui-server` shows this conversation",
            ));
        };
        let running = door.processing().await.map_err(no_answer)?;
        let mode = match say.mode {
            // Copilot's `immediate` goes in at the running turn's next model call.
            SayMode::Steer if running => "immediate",
            SayMode::Steer | SayMode::Queue => "enqueue",
            SayMode::Immediate => {
                if running || door.pending().await.map_err(no_answer)? {
                    return Err(Error::refused(
                        refusal::HARNESS_BUSY,
                        "Copilot runs a turn in this conversation",
                    ));
                }
                "enqueue"
            }
        };
        let mut params = json!({
            "sessionId": say.external_id,
            "prompt": say.words,
            "mode": mode,
        });
        if let Some(id) = &say.id {
            params["clientTurnId"] = json!(id);
            lock(&door.fold).sent(id);
        }
        let sent = door
            .client
            .ask("session.send", params, WITHIN)
            .await
            .map_err(no_answer)?;
        Ok(Said {
            took: sent.get("messageId").and_then(Value::as_str).is_some(),
        })
    }

    /// `pick`: Copilot switches the window's model and effort, and the driver reads back
    /// what it runs on.
    pub async fn pick(self: &Arc<Self>, store: &Store, pick: Pick) -> Result<Picked, Error> {
        let Some(door) = self.door(store, &pick.external_id).await else {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "no Copilot window started with `--ui-server` shows this conversation",
            ));
        };
        let session = pick.external_id.as_str();
        // Copilot applies any model name and fails only at the next turn, so the pick is
        // checked against the models it lists, as `models` answers them.
        let model = match &pick.model {
            Some(model) => Some(model.clone()),
            None => door
                .ask("session.model.getCurrent", json!({}))
                .await
                .ok()
                .and_then(|now| text(&now, "modelId").map(str::to_owned)),
        };
        let listed = door
            .client
            .ask("models.list", json!({}), WITHIN)
            .await
            .map_err(no_answer)?;
        let entry = listed
            .get("models")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|entry| model.is_some() && text(entry, "id") == model.as_deref());
        if let (Some(model), None) = (&pick.model, entry) {
            return Err(Error::refused(
                refusal::MODEL_MISSING,
                format!("Copilot lists no model `{model}`"),
            ));
        }
        if let Some(thinking) = &pick.thinking {
            let efforts = entry
                .and_then(|entry| entry.get("supportedReasoningEfforts"))
                .and_then(Value::as_array);
            if !efforts.is_some_and(|efforts| efforts.iter().any(|e| e.as_str() == Some(thinking)))
            {
                return Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    format!("Copilot's model takes no effort `{thinking}`"),
                ));
            }
        }
        match (&pick.model, &pick.thinking) {
            (Some(model), thinking) => {
                let mut params = json!({"sessionId": session, "modelId": model});
                if let Some(thinking) = thinking {
                    params["reasoningEffort"] = json!(thinking);
                }
                let switched = door
                    .client
                    .ask("session.model.switchTo", params, WITHIN)
                    .await
                    .map_err(|why| Error::refused(refusal::MODEL_MISSING, why))?;
                if text(&switched, "status") != Some("applied") {
                    let why = text(&switched, "message").unwrap_or("Copilot did not switch");
                    return Err(Error::refused(refusal::MODEL_MISSING, why));
                }
            }
            (None, Some(thinking)) => {
                door.client
                    .ask(
                        "session.model.setReasoningEffort",
                        json!({"sessionId": session, "reasoningEffort": thinking}),
                        WITHIN,
                    )
                    .await
                    .map_err(|why| Error::refused(refusal::SELECTION_UNSUPPORTED, why))?;
            }
            (None, None) => {}
        }
        let now = door
            .client
            .ask(
                "session.model.getCurrent",
                json!({"sessionId": session}),
                WITHIN,
            )
            .await
            .map_err(|why| Error::refused(refusal::SELECTION_UNSUPPORTED, why))?;
        Ok(Picked {
            model: text(&now, "modelId").map(str::to_owned),
            thinking: text(&now, "reasoningEffort").map(str::to_owned),
        })
    }

    /// `stop` of a window's owner: Copilot aborts the turn that runs, where it is still the
    /// one `held` named, and where a say is named, the one its words went into.
    pub async fn stop(self: &Arc<Self>, store: &Store, stop: &Stop) -> Result<Stopped, Error> {
        let Holder::Person {
            stop: HolderStop::Owner(token),
            ..
        } = &stop.holder
        else {
            return Ok(not_stopped("no window of Copilot's server holds it"));
        };
        let Some(door) = self.door(store, &stop.external_id).await else {
            return Ok(not_stopped("the Copilot window that held it is gone"));
        };
        let now = door.token();
        let says = lock(&door.fold).says();
        if !door.processing().await.map_err(no_answer)? {
            return Ok(not_stopped("no turn runs in the Copilot window"));
        }
        if let Some(say) = &stop.say {
            if !says.contains(say) {
                return Err(Error::refused(
                    refusal::OTHER_TURN,
                    "the turn that runs did not take these words",
                ));
            }
        } else if token.is_some() && *token != now {
            return Ok(not_stopped("the turn it named ended; another runs"));
        }
        let aborted = door
            .client
            .ask(
                "session.abort",
                json!({"sessionId": stop.external_id, "reason": "Steery stopped the turn"}),
                WITHIN,
            )
            .await
            .map_err(no_answer)?;
        let stopped = aborted.get("success").and_then(Value::as_bool) == Some(true);
        Ok(Stopped {
            stopped,
            proof: Some(if stopped {
                format!("Copilot (pid {}) aborted the turn", door.pid)
            } else {
                text(&aborted, "error")
                    .unwrap_or("Copilot did not abort the turn")
                    .to_owned()
            }),
        })
    }

    /// Read the door's event log until its window closes, and send each new state.
    async fn follow(self: Arc<Self>, door: Arc<Followed>) {
        let mut last = None;
        let mut cursor = door.seed().await;
        while let Some(at) = cursor {
            if let Ok((processing, pending)) = door.snapshot().await {
                let state = {
                    let mut fold = lock(&door.fold);
                    fold.refresh(processing, pending, now());
                    fold.state()
                };
                if last.as_ref() != Some(&state) {
                    self.changed(&door.id, state.clone());
                    last = Some(state);
                }
            }
            cursor = match door.read(&at).await {
                Ok(Some(next)) => Some(next),
                // The cursor fell out of the history: start again from its end.
                Ok(None) => door.seed().await,
                Err(_) => None,
            };
        }
        let mut doors = lock(&self.doors);
        if doors
            .get(&door.id)
            .is_some_and(|held| Arc::ptr_eq(held, &door))
        {
            doors.remove(&door.id);
        }
        drop(doors);
        // The window closed: nothing it ran is known to run.
        if last.is_some() {
            self.changed(&door.id, LiveState::default());
        }
    }

    fn changed(&self, id: &str, state: LiveState) {
        let changed = SessionChanged {
            external_id: id.to_owned(),
            state,
        };
        if let Ok(params) = serde_json::to_value(changed) {
            self.notifier.notify(method::SESSION_CHANGED, params);
        }
    }
}

impl Followed {
    /// The token of the turn that runs: the window and when the turn began.
    fn token(&self) -> Option<String> {
        let began = lock(&self.fold).began?;
        Some(format!("{}:{}:{began}", self.pid, self.port))
    }

    async fn ask(&self, method: &str, mut params: Value) -> Result<Value, String> {
        params["sessionId"] = json!(self.id);
        self.client.ask(method, params, WITHIN).await
    }

    async fn processing(&self) -> Result<bool, String> {
        let answer = self.ask("session.metadata.isProcessing", json!({})).await?;
        Ok(answer.get("processing").and_then(Value::as_bool) == Some(true))
    }

    async fn pending(&self) -> Result<bool, String> {
        let answer = self.ask("session.queue.hasPending", json!({})).await?;
        Ok(answer.get("hasPending").and_then(Value::as_bool) == Some(true))
    }

    async fn snapshot(&self) -> Result<(bool, bool), String> {
        Ok((self.processing().await?, self.pending().await?))
    }

    /// The log's end, after the newest events are read into the fold with the model the
    /// window runs on now; none where the window does not answer.
    async fn seed(&self) -> Option<String> {
        let tail = self.ask("session.eventLog.tail", json!({})).await.ok()?;
        let cursor = text(&tail, "cursor")?.to_owned();
        let newest = self
            .ask(
                "session.eventLog.read",
                json!({"direction": "backward", "max": SEED}),
            )
            .await
            .ok()?;
        let current = self.ask("session.model.getCurrent", json!({})).await.ok();
        let mut fold = lock(&self.fold);
        for event in events(&newest) {
            fold.apply(event);
        }
        if let Some(current) = current {
            fold.model = text(&current, "modelId").map(str::to_owned);
            fold.thinking = text(&current, "reasoningEffort").map(str::to_owned);
        }
        Some(cursor)
    }

    /// The events past `cursor`, waiting for new ones: the next cursor, or none where the
    /// log no longer has `cursor`.
    async fn read(&self, cursor: &str) -> Result<Option<String>, String> {
        let running = {
            let fold = lock(&self.fold);
            fold.processing || fold.began.is_some()
        };
        let wait = if running { WAIT_RUNNING_MS } else { WAIT_MS };
        let read = self
            .client
            .ask(
                "session.eventLog.read",
                json!({"sessionId": self.id, "cursor": cursor, "waitMs": wait, "max": BATCH}),
                WITHIN + Duration::from_millis(wait),
            )
            .await?;
        if text(&read, "cursorStatus").is_some_and(|status| status != "ok") {
            return Ok(None);
        }
        {
            let mut fold = lock(&self.fold);
            for event in events(&read) {
                fold.apply(event);
            }
        }
        Ok(Some(text(&read, "cursor").unwrap_or(cursor).to_owned()))
    }
}

fn events(read: &Value) -> impl Iterator<Item = &Value> {
    read.get("events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// What the event log said of the conversation.
#[derive(Default)]
struct Fold {
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
    /// The say ids Steery sent, newest last.
    sent: VecDeque<String>,
    /// When the turn that runs began, and the says it took.
    began: Option<u64>,
    says: Vec<String>,
    /// The error the turn stopped on, where no answer came after it.
    error: Option<String>,
    calls: Vec<LiveCall>,
    ended: VecDeque<LiveEnded>,
    settled: VecDeque<LiveSettled>,
    compacting: bool,
    retries: u32,
    retry: Option<LiveRetry>,
    model: Option<String>,
    thinking: Option<String>,
    context_left: Option<f64>,
    /// Dialogs open in front of the person, by request.
    dialogs: Vec<(String, Dialog)>,
    processing: bool,
    /// When the newest event read happened.
    last: Option<u64>,
    pending: bool,
    /// Whether the log has shown the turn that runs, not only the window's answer.
    logged: bool,
    /// Whether the newest turn was aborted: the window still says it runs while it winds
    /// down, which opens no new turn.
    aborted: bool,
}

impl Fold {
    fn sent(&mut self, id: &str) {
        self.sent.push_back(id.to_owned());
        if self.sent.len() > SENT {
            self.sent.pop_front();
        }
    }

    fn says(&self) -> Vec<String> {
        self.says.clone()
    }

    /// One event of the log; an event read before is skipped.
    fn apply(&mut self, event: &Value) {
        let Some(id) = text(event, "id") else {
            return;
        };
        if !self.seen.insert(id.to_owned()) {
            return;
        }
        self.seen_order.push_back(id.to_owned());
        if self.seen_order.len() > SEEN
            && let Some(oldest) = self.seen_order.pop_front()
        {
            self.seen.remove(&oldest);
        }
        let at = text(event, "timestamp")
            .and_then(millis)
            .unwrap_or_else(now);
        let null = Value::Null;
        let data = event.get("data").unwrap_or(&null);
        let parent = text(data, "parentToolCallId");
        let last = self.last.replace(at);
        match (text(event, "type").unwrap_or_default(), parent) {
            ("tool.execution_start", Some(parent)) => self.child(parent, data),
            ("tool.execution_complete", Some(_)) => {
                let id = text(data, "toolCallId").unwrap_or_default();
                for call in &mut self.calls {
                    call.children.retain(|child| child.id != id);
                }
            }
            // A sub-agent's own records say nothing of the lead's state.
            (_, Some(_)) => {}
            ("session.start" | "session.resume", None) => {
                self.close(at);
                if let Some(model) = text(data, "selectedModel") {
                    self.model = Some(model.to_owned());
                    self.thinking = text(data, "reasoningEffort").map(str::to_owned);
                }
            }
            // Copilot writes no `session.idle` to its log, so a turn is known to have
            // ended by an abort, or by the next message that starts a turn from idle.
            ("session.idle", None) => self.close(at),
            ("abort", None) => {
                self.close(at);
                self.aborted = true;
            }
            ("user.message", None) => {
                if text(data, "delivery") == Some("idle") {
                    if self.logged {
                        self.close(last.unwrap_or(at));
                    }
                    // A turn the window said it runs before its log showed it keeps the
                    // start already given, so its stop token stays the same.
                    self.began.get_or_insert(at);
                    self.logged = true;
                    self.aborted = false;
                }
                if let Some(say) = text(data, "clientTurnId")
                    && self.sent.iter().any(|sent| sent == say)
                    && !self.says.iter().any(|taken| taken == say)
                {
                    self.says.push(say.to_owned());
                }
            }
            ("assistant.turn_start", None) => {
                self.began.get_or_insert(at);
                self.logged = true;
                self.aborted = false;
                self.retry = None;
            }
            ("assistant.message", None) => {
                self.error = None;
                self.retry = None;
            }
            ("assistant.turn_retry", None) => {
                self.retries += 1;
                self.retry = Some(LiveRetry {
                    attempt: self.retries,
                    of: None,
                    error: text(data, "reason").map(|why| cut(why, ERROR_AT_MOST)),
                });
            }
            ("session.error", None) => {
                self.error = text(data, "message").map(|why| cut(why, ERROR_AT_MOST));
            }
            ("tool.execution_start", None) => {
                self.retry = None;
                let Some(id) = text(data, "toolCallId") else {
                    return;
                };
                self.calls.push(LiveCall {
                    id: id.to_owned(),
                    tool: tool(id, data),
                    since: at,
                    children: Vec::new(),
                    jobs: Vec::new(),
                });
            }
            ("tool.execution_complete", None) => {
                let id = text(data, "toolCallId").unwrap_or_default();
                if let Some(index) = self.calls.iter().position(|call| call.id == id) {
                    let call = self.calls.remove(index);
                    self.end(call.id, at);
                }
            }
            ("session.compaction_start", None) => self.compacting = true,
            ("session.compaction_complete", None) => self.compacting = false,
            ("session.model_change", None) => {
                if let Some(model) = text(data, "newModel") {
                    self.model = Some(model.to_owned());
                    self.thinking = text(data, "reasoningEffort").map(str::to_owned);
                }
            }
            ("session.usage_info", None) => {
                let current = data.get("currentTokens").and_then(Value::as_f64);
                let limit = data.get("tokenLimit").and_then(Value::as_f64);
                if let (Some(current), Some(limit)) = (current, limit)
                    && limit > 0.0
                {
                    self.context_left = Some((1.0 - current / limit).clamp(0.0, 1.0));
                }
            }
            ("permission.requested", None) => {
                let asked = data.get("permissionRequest").unwrap_or(&null);
                let title = ["intention", "fullCommandText", "toolTitle", "url", "path"]
                    .iter()
                    .find_map(|key| text(asked, key))
                    .or_else(|| text(asked, "kind"))
                    .unwrap_or("Copilot asks for permission");
                self.open(data, DialogKind::Confirm, title, at);
            }
            ("user_input.requested", None) => {
                let choices = data
                    .get("choices")
                    .and_then(Value::as_array)
                    .is_some_and(|choices| !choices.is_empty());
                let kind = if choices {
                    DialogKind::Select
                } else {
                    DialogKind::Input
                };
                let title = text(data, "question").unwrap_or_default();
                self.open(data, kind, title, at);
            }
            ("elicitation.requested", None) => {
                let title = text(data, "message").unwrap_or_default();
                self.open(data, DialogKind::Custom, title, at);
            }
            ("permission.completed" | "user_input.completed" | "elicitation.completed", None) => {
                let request = text(data, "requestId").unwrap_or_default();
                self.dialogs.retain(|(open, _)| open != request);
            }
            _ => {}
        }
    }

    fn child(&mut self, parent: &str, data: &Value) {
        let Some(id) = text(data, "toolCallId") else {
            return;
        };
        let child = LiveChild {
            id: id.to_owned(),
            tool: tool(id, data),
        };
        if let Some(call) = self
            .calls
            .iter_mut()
            .find(|call| call.id == parent || call.children.iter().any(|child| child.id == parent))
        {
            call.children.push(child);
        }
    }

    fn open(&mut self, data: &Value, kind: DialogKind, title: &str, since: u64) {
        let Some(request) = text(data, "requestId") else {
            return;
        };
        self.dialogs.push((
            request.to_owned(),
            Dialog {
                kind,
                title: cut(title, TITLE_AT_MOST),
                since,
            },
        ));
    }

    fn end(&mut self, id: String, at: u64) {
        self.ended.push_back(LiveEnded { id, at });
        if self.ended.len() > ENDED {
            self.ended.pop_front();
        }
    }

    /// The turn that ran ended at `at`: the turn Steery's words began settles.
    fn close(&mut self, at: u64) {
        for call in std::mem::take(&mut self.calls) {
            self.end(call.id, at);
        }
        self.dialogs.clear();
        self.retry = None;
        self.retries = 0;
        self.logged = false;
        let error = self.error.take();
        let says = std::mem::take(&mut self.says);
        if let Some(began_at) = self.began.take()
            && !says.is_empty()
        {
            self.settled.push_back(LiveSettled {
                began_at,
                at,
                says,
                failed: error.is_some(),
                error,
            });
            if self.settled.len() > SETTLED {
                self.settled.pop_front();
            }
        }
    }

    /// What the window answered now, which wins over what its log implied.
    fn refresh(&mut self, processing: bool, pending: bool, at: u64) {
        if processing && !self.aborted {
            self.began.get_or_insert(at);
        } else if self.began.is_some() || !self.calls.is_empty() {
            self.close(at);
        }
        self.processing = processing;
        self.pending = pending;
    }

    fn state(&self) -> LiveState {
        LiveState {
            turn: self.began.map(|began_at| LiveTurn {
                began_at,
                says: self.says.clone(),
            }),
            idle: Some(!self.processing),
            pending: Some(self.pending),
            // Copilot's server does not say what the person's editor holds.
            typing: None,
            compacting: self.compacting,
            retrying: self.retry.is_some(),
            retry: self.retry.clone(),
            jobs: 0,
            model: self.model.clone(),
            thinking: self.thinking.clone(),
            context_left: self.context_left,
            calls: self.calls.clone(),
            ended: self.ended.iter().cloned().collect(),
            dialog: self.dialogs.first().map(|(_, dialog)| dialog.clone()),
            settled: self.settled.iter().cloned().collect(),
        }
    }
}

/// A call's tool as a Call names it (PROTOCOL.md §4.1).
fn tool(id: &str, data: &Value) -> String {
    match (text(data, "mcpServerName"), text(data, "mcpToolName")) {
        (Some(server), Some(tool)) => calls::mcp(id, server, tool, Value::Null).tool,
        _ => calls::call(id, text(data, "toolName").unwrap_or_default(), Value::Null).tool,
    }
}

fn not_stopped(why: &str) -> Stopped {
    Stopped {
        stopped: false,
        proof: Some(why.to_owned()),
    }
}

fn no_answer(why: String) -> Error {
    Error::refused(refusal::NO_ANSWER, why)
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn cut(text: &str, at_most: usize) -> String {
    text.chars().take(at_most).collect()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
