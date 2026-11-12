//! Antigravity's own door (PROTOCOL.md §4.3): an Antigravity window that started with its
//! server's token ([`super::door`]) takes words (`SendUserCascadeMessage`) and a stop
//! (`CancelCascadeInvocation`) through its language server. Its state stream
//! (`StreamAgentStateUpdates`) says what runs, and the driver sends that as
//! `session/changed`. The server takes a model only together with words, and a turn that the
//! person types goes back to the model in the window's footer, so the door keeps its own
//! model: `pick` is refused.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    self, Holder, HolderStop, LiveCall, LiveEnded, LiveSettled, LiveState, LiveTurn, Said, Say,
    SayMode, SessionChanged, Stop, Stopped,
};
use steery_sdk::{Error, Notifier, method, refusal};

use super::door::{self, Window};
use super::{drive, events, store::Store};
use crate::turn::drive::now;

/// What a door keeps of the turns and calls it saw: as many as a state carries.
const ENDED: usize = 64;
const SETTLED: usize = 16;
/// How many say ids a door keeps as Steery's.
const SENT: usize = 64;
/// How long the driver waits before it follows a window again whose stream ended.
const AGAIN: Duration = Duration::from_secs(1);

/// How the window takes words: after the turn that runs, or at its next model call.
const WHEN_IDLE: &str = "MESSAGE_DELIVERY_STRATEGY_WHEN_IDLE";
const NEXT_INVOCATION: &str = "MESSAGE_DELIVERY_STRATEGY_NEXT_INVOCATION";

/// Every door the driver follows, by conversation.
pub struct Live {
    notifier: Notifier,
    doors: Mutex<HashMap<String, Arc<Followed>>>,
}

/// One window that shows a conversation, and what its state stream said.
struct Followed {
    id: String,
    window: Window,
    fold: Mutex<Fold>,
    /// The stream ended and the window did not answer again.
    ended: AtomicBool,
}

impl Live {
    pub fn new(notifier: Notifier) -> Self {
        Self {
            notifier,
            doors: Mutex::default(),
        }
    }

    /// The door of conversation `id`: the one followed already, else the window found now,
    /// whose stream the driver then follows. The first message of the stream is the whole
    /// state, so a door answers from what runs as soon as it is found.
    async fn door(self: &Arc<Self>, store: &Store, id: &str) -> Result<Arc<Followed>, String> {
        if let Some(door) = lock(&self.doors)
            .get(id)
            .filter(|door| !door.ended.load(Ordering::Relaxed))
        {
            return Ok(Arc::clone(door));
        }
        let window = door::find(&store.lock(id)).await?;
        let mut stream = follow(&window, id).await?;
        let first = tokio::time::timeout(door::WITHIN, stream.next())
            .await
            .map_err(|_| "the Antigravity window sent no state".to_owned())??
            .ok_or("the Antigravity window sent no state")?;
        let mut fold = Fold::default();
        fold.apply(&first, now());
        let found = Arc::new(Followed {
            id: id.to_owned(),
            window,
            fold: Mutex::new(fold),
            ended: AtomicBool::new(false),
        });
        // Two asks that raced keep the first door; the other's stream closes with it.
        let door = Arc::clone(
            lock(&self.doors)
                .entry(id.to_owned())
                .and_modify(|held| {
                    if held.ended.load(Ordering::Relaxed) {
                        *held = Arc::clone(&found);
                    }
                })
                .or_insert_with(|| Arc::clone(&found)),
        );
        if Arc::ptr_eq(&door, &found) {
            let live = Arc::clone(self);
            let followed = Arc::clone(&door);
            tokio::spawn(async move { live.follow(followed, stream).await });
        }
        Ok(door)
    }

    /// `held`: the window's owner, with the turn that runs as its token; none where no
    /// window with a door shows the conversation.
    pub async fn held(self: &Arc<Self>, store: &Store, id: &str) -> Option<Holder> {
        let door = self.door(store, id).await.ok()?;
        let token = door.token();
        Some(Holder::Person {
            door: Some(harness::Door::Keeps),
            stop: HolderStop::Owner(token),
        })
    }

    /// `say`: the words into the window's conversation, named by the say's id so its turn
    /// can be told.
    pub async fn say(self: &Arc<Self>, store: &Store, say: Say) -> Result<Said, Error> {
        let door = self
            .door(store, &say.external_id)
            .await
            .map_err(|why| Error::refused(refusal::NO_DOOR, why))?;
        let (running, pending) = {
            let fold = lock(&door.fold);
            (fold.running, fold.pending)
        };
        // Words the window takes "when idle" go in at once where nothing runs.
        let strategy = match say.mode {
            SayMode::Steer if running => NEXT_INVOCATION,
            SayMode::Steer | SayMode::Queue => WHEN_IDLE,
            SayMode::Immediate => {
                if running || pending {
                    return Err(Error::refused(
                        refusal::HARNESS_BUSY,
                        "Antigravity runs a turn in this conversation",
                    ));
                }
                WHEN_IDLE
            }
        };
        let mut params = json!({
            "cascadeId": say.external_id,
            "items": [{"text": say.words}],
            "deliveryStrategy": strategy,
        });
        if let Some(id) = &say.id {
            params["clientMessageId"] = json!(id);
            lock(&door.fold).sent(id);
        }
        door.window
            .call("SendUserCascadeMessage", &params)
            .await
            .map_err(no_answer)?;
        Ok(Said { took: true })
    }

    /// `stop` of a window's owner: Antigravity cancels the turn that runs, where it is still
    /// the one `held` named, and where a say is named, the one its words went into.
    pub async fn stop(self: &Arc<Self>, store: &Store, stop: &Stop) -> Result<Stopped, Error> {
        let Holder::Person {
            stop: HolderStop::Owner(token),
            ..
        } = &stop.holder
        else {
            return Ok(not_stopped("no Antigravity window with a door holds it"));
        };
        let Ok(door) = self.door(store, &stop.external_id).await else {
            return Ok(not_stopped("the Antigravity window that held it is gone"));
        };
        let (running, says, now) = {
            let fold = lock(&door.fold);
            (fold.running, fold.says.clone(), door.token_of(&fold))
        };
        if !running {
            return Ok(not_stopped("no turn runs in the Antigravity window"));
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
        door.window
            .call(
                "CancelCascadeInvocation",
                &json!({"cascadeId": stop.external_id}),
            )
            .await
            .map_err(no_answer)?;
        Ok(Stopped {
            stopped: true,
            proof: Some(format!(
                "Antigravity (pid {}) cancelled the turn",
                door.window.pid
            )),
        })
    }

    /// Read the door's state stream until its window closes, and send each new state.
    async fn follow(self: Arc<Self>, door: Arc<Followed>, mut stream: door::Stream) {
        let mut last = None;
        loop {
            let state = lock(&door.fold).state();
            if last.as_ref() != Some(&state) {
                self.changed(&door.id, state.clone());
                last = Some(state);
            }
            match stream.next().await {
                Ok(Some(message)) => lock(&door.fold).apply(&message, now()),
                // The stream ended: follow the window again while it answers.
                Ok(None) | Err(_) => {
                    tokio::time::sleep(AGAIN).await;
                    match follow(&door.window, &door.id).await {
                        Ok(again) => stream = again,
                        Err(_) => break,
                    }
                }
            }
        }
        door.ended.store(true, Ordering::Relaxed);
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
        self.token_of(&lock(&self.fold))
    }

    fn token_of(&self, fold: &Fold) -> Option<String> {
        let began = fold.began?;
        Some(format!("{}:{}:{began}", self.window.pid, self.window.port))
    }
}

/// The state stream of conversation `id` in `window`.
async fn follow(window: &Window, id: &str) -> Result<door::Stream, String> {
    window
        .stream(
            "StreamAgentStateUpdates",
            &json!({
                "conversationId": id,
                "subscriberId": format!("steery-{}", std::process::id()),
            }),
        )
        .await
}

/// What the state stream said of the conversation.
#[derive(Default)]
struct Fold {
    /// The say ids Steery sent and no turn took yet, newest last.
    sent: VecDeque<String>,
    /// Whether a turn runs, when it began, and the says it took.
    running: bool,
    began: Option<u64>,
    says: Vec<String>,
    /// When the newest turn the stream named began, and when the last turn ended.
    turn_start: Option<u64>,
    closed_at: u64,
    /// The turn ended on an error.
    error: Option<String>,
    pending: bool,
    calls: Vec<LiveCall>,
    ended: VecDeque<LiveEnded>,
    settled: VecDeque<LiveSettled>,
    model: Option<String>,
    thinking: Option<String>,
    context_left: Option<f64>,
}

impl Fold {
    fn sent(&mut self, id: &str) {
        self.sent.push_back(id.to_owned());
        if self.sent.len() > SENT {
            self.sent.pop_front();
        }
    }

    /// One message of the stream: the whole state first, then what changed.
    fn apply(&mut self, message: &Value, at: u64) {
        let Some(update) = message.get("update") else {
            return;
        };
        if let Some(start) = update
            .pointer("/turnsUpdate/turns")
            .and_then(Value::as_array)
            .and_then(|turns| turns.last())
            .and_then(|turn| text(turn, "startTime"))
            .and_then(events::millis)
        {
            self.turn_start = Some(start);
        }
        let metadata = update.get("executorMetadata").into_iter().chain(
            update
                .pointer("/executorMetadatasUpdate/executorMetadatas")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        );
        for metadata in metadata {
            if let Some(model) = metadata
                .pointer("/cascadeConfig/plannerConfig/modelName")
                .and_then(Value::as_str)
            {
                let (model, thinking) = drive::picked(model);
                self.model = Some(model);
                self.thinking = thinking;
            }
            if text(metadata, "terminationReason") == Some("EXECUTOR_TERMINATION_REASON_ERROR") {
                self.error = Some("Antigravity ended the turn on an error".to_owned());
            }
        }
        if let Some(usage) = update.get("contextWindowUsage") {
            let used = usage.get("estimatedTokensUsed").and_then(Value::as_f64);
            let most = usage.get("maxContextTokens").and_then(Value::as_f64);
            if let (Some(used), Some(most)) = (used, most)
                && most > 0.0
            {
                self.context_left = Some((1.0 - used / most).clamp(0.0, 1.0));
            }
        }
        if let Some(pending) = update.get("pendingAgentMessagesUpdate") {
            self.pending = pending
                .get("totalLength")
                .and_then(Value::as_u64)
                .is_some_and(|waiting| waiting > 0);
        }
        // A turn that runs as the door opens began when the stream says it did.
        match text(update, "status") {
            Some("CASCADE_RUN_STATUS_IDLE") => {
                self.running = false;
                self.close(at);
            }
            Some(
                "CASCADE_RUN_STATUS_RUNNING"
                | "CASCADE_RUN_STATUS_BUSY"
                | "CASCADE_RUN_STATUS_CANCELING",
            ) => {
                self.running = true;
                let start = self.turn_start.filter(|start| *start > self.closed_at);
                self.began.get_or_insert(start.unwrap_or(at));
            }
            _ => {}
        }
        for step in update
            .pointer("/mainTrajectoryUpdate/stepsUpdate/steps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            self.step(step, at);
        }
    }

    /// One step of the conversation: words that went in, or a tool call.
    fn step(&mut self, step: &Value, at: u64) {
        if let Some(say) = step
            .pointer("/userInput/clientMessageId")
            .and_then(Value::as_str)
            && let Some(index) = self.sent.iter().position(|sent| sent == say)
        {
            self.sent.remove(index);
            self.says.push(say.to_owned());
        }
        let Some(call) = step.pointer("/metadata/toolCall") else {
            return;
        };
        let Some(id) = text(call, "id") else {
            return;
        };
        let running = matches!(
            text(step, "status"),
            Some("CORTEX_STEP_STATUS_PENDING" | "CORTEX_STEP_STATUS_RUNNING")
        );
        let known = self.calls.iter().position(|call| call.id == id);
        match (running, known) {
            (true, None) => {
                let input = text(call, "argumentsJson")
                    .and_then(|input| serde_json::from_str(input).ok())
                    .unwrap_or(Value::Null);
                let since = step
                    .pointer("/metadata/createdAt")
                    .and_then(Value::as_str)
                    .and_then(events::millis)
                    .unwrap_or(at);
                self.calls.push(LiveCall {
                    id: id.to_owned(),
                    tool: events::call(id, text(call, "name").unwrap_or_default(), input).tool,
                    since,
                    children: Vec::new(),
                    jobs: Vec::new(),
                });
            }
            (false, Some(index)) => {
                let call = self.calls.remove(index);
                let ended = step
                    .pointer("/metadata/completedAt")
                    .and_then(Value::as_str)
                    .and_then(events::millis)
                    .unwrap_or(at);
                self.end(call.id, ended);
            }
            _ => {}
        }
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
        let error = self.error.take();
        let says = std::mem::take(&mut self.says);
        self.closed_at = at;
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

    fn state(&self) -> LiveState {
        LiveState {
            turn: self.began.map(|began_at| LiveTurn {
                began_at,
                says: self.says.clone(),
            }),
            idle: Some(!self.running),
            pending: Some(self.pending),
            // Antigravity's server does not say what the person's editor holds.
            typing: None,
            compacting: false,
            retrying: false,
            retry: None,
            jobs: 0,
            model: self.model.clone(),
            thinking: self.thinking.clone(),
            context_left: self.context_left,
            calls: self.calls.clone(),
            ended: self.ended.iter().cloned().collect(),
            dialog: None,
            settled: self.settled.iter().cloned().collect(),
        }
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

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
