//! Qwen Code's own door (PROTOCOL.md §4.3): its daemon, `qwen serve` ([`door`]). A daemon
//! runs each conversation that a client loads in it (the Web Shell, `qwen attach`, the API)
//! and keeps writing its chat file. Any client gives it words (`POST /session/<id>/prompt`,
//! `/mid-turn-message`), a model (`/model`) and a cancel (`/cancel`); its server-sent events
//! say what runs, which the driver sends as `session/changed`.
//!
//! Words that steer wait in the daemon until the running turn's next step, and go in there
//! (`mid_turn_message_injected`); where that turn ends first, the daemon runs them as their
//! own turn, with the message's id as the prompt's. Other words queue as a turn of their own.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    self, Dialog, DialogKind, Holder, HolderStop, LiveCall, LiveEnded, LiveSettled, LiveState,
    LiveTurn, Pick, Picked, Said, Say, SayMode, SessionChanged, Stop, Stopped,
};
use steery_sdk::{Error, Notifier, method, refusal};
use tokio::sync::{Notify, oneshot};

use super::door::{self, Daemon};
use crate::turn::drive::now;

/// How often the driver asks the daemon what runs, while something does.
const EVERY: Duration = Duration::from_secs(1);
/// How often the driver asks whether the daemon still runs the conversation for a client.
const VIEWED_EVERY: Duration = Duration::from_secs(5);
/// How long a stop waits for the turn to end: inside the runtime's 2.8 s for `stop`
/// (PROTOCOL.md §7).
const STOPPED_WITHIN: Duration = Duration::from_millis(2500);
/// How many ended calls, settled turns and finished prompts a fold keeps.
const ENDED: usize = 64;
const SETTLED: usize = 16;
const FINISHED: usize = 32;
/// The longest error and dialog title a state carries.
const ERROR_AT_MOST: usize = 600;
const TITLE_AT_MOST: usize = 200;
/// The `_meta` key that keeps a model pick off the person's default (`acp-bridge/src/
/// bridgeTypes.ts:610`): the pick is for this conversation alone.
const PERSIST_DEFAULT: &str = "qwen.session.modelPersistDefault";

/// The conversations that a daemon runs and that the driver follows.
pub struct Live {
    notifier: Notifier,
    followed: tokio::sync::Mutex<HashMap<String, Arc<Session>>>,
}

/// One conversation in a daemon, its event stream, and what the events said.
struct Session {
    daemon: Daemon,
    id: String,
    /// The folder the daemon runs it in.
    workspace: Option<String>,
    ended: AtomicBool,
    /// Wakes the loop that asks the daemon what runs.
    poke: Notify,
    /// Closes the event stream.
    shut: Notify,
    fold: Mutex<Fold>,
}

impl Live {
    pub fn new(notifier: Notifier) -> Self {
        Self {
            notifier,
            followed: tokio::sync::Mutex::default(),
        }
    }

    /// The conversation `id` where a daemon of this root runs it; the driver then follows its
    /// events. A daemon runs only the conversations that a client loaded in it.
    async fn door(self: &Arc<Self>, home: &Path, root: &Path, id: &str) -> Option<Arc<Session>> {
        if !super::drive::plain_id(id) {
            return None;
        }
        let mut followed = self.followed.lock().await;
        if let Some(session) = followed
            .get(id)
            .filter(|session| !session.ended.load(Ordering::Relaxed))
        {
            return Some(Arc::clone(session));
        }
        for daemon in door::daemons(home, root) {
            let Ok(status) = daemon.get(&format!("/session/{id}/status")).await else {
                continue;
            };
            if status.status != 200 {
                continue;
            }
            let status = status.json().unwrap_or_default();
            let session = Arc::new(Session {
                workspace: status["workspaceCwd"].as_str().map(str::to_owned),
                daemon,
                id: id.to_owned(),
                ended: AtomicBool::new(false),
                poke: Notify::new(),
                shut: Notify::new(),
                fold: Mutex::default(),
            });
            let (opened, open) = oneshot::channel();
            let (live, reading) = (Arc::clone(self), Arc::clone(&session));
            tokio::spawn(async move {
                let path = format!("/session/{}/events", reading.id);
                let events = reading.daemon.events(
                    &path,
                    || {
                        let _ = opened.send(());
                    },
                    |event| live.event(&reading, &event),
                );
                tokio::select! {
                    _ = events => {}
                    () = reading.shut.notified() => {}
                }
                live.closed(&reading).await;
            });
            if tokio::time::timeout(Duration::from_secs(2), open)
                .await
                .is_err()
            {
                session.ended.store(true, Ordering::Relaxed);
                session.shut.notify_one();
                continue;
            }
            session.lock().model = session.model().await;
            session.refresh(self).await;
            followed.insert(id.to_owned(), Arc::clone(&session));
            let (live, asking) = (Arc::clone(self), Arc::clone(&session));
            tokio::spawn(async move { live.ask_loop(&asking).await });
            return Some(session);
        }
        None
    }

    /// One event of the conversation.
    fn event(&self, session: &Session, event: &Value) {
        let data = &event["data"];
        let at = now();
        let prompt = data["promptId"]
            .as_str()
            .or_else(|| event["promptId"].as_str())
            .unwrap_or_default();
        match event["type"].as_str().unwrap_or_default() {
            "session_update" => {
                let update = &data["update"];
                session.apply(self, |fold| fold.update(update, at));
                if update["sessionUpdate"] == "user_message_chunk" {
                    session.poke.notify_one();
                }
            }
            "turn_complete" => {
                session.apply(self, |fold| fold.finished(prompt, at));
                session.poke.notify_one();
            }
            "turn_error" => {
                let error = data["message"].as_str().unwrap_or("the turn failed");
                session.apply(self, |fold| fold.failed(prompt, cut(error, ERROR_AT_MOST)));
                session.poke.notify_one();
            }
            "pending_prompt_started" => {
                session.apply(self, |fold| fold.started(prompt));
                session.poke.notify_one();
            }
            "mid_turn_message_injected" => {
                let ids: Vec<&str> = data["messageIds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                session.apply(self, |fold| fold.injected(&ids));
                session.poke.notify_one();
            }
            "permission_request" => {
                let request = data["requestId"].as_str().unwrap_or_default();
                let title = data["toolCall"]["title"].as_str().unwrap_or("a tool");
                session.apply(self, |fold| {
                    fold.dialogs.push((
                        request.to_owned(),
                        Dialog {
                            kind: DialogKind::Confirm,
                            title: cut(&format!("Allow {title}?"), TITLE_AT_MOST),
                            since: at,
                        },
                    ));
                });
            }
            "permission_resolved" => {
                let request = data["requestId"].as_str().unwrap_or_default();
                session.apply(self, |fold| {
                    fold.dialogs.retain(|(open, _)| open != request)
                });
            }
            "model_switched" => {
                if let Some(model) = data["modelId"].as_str() {
                    session.apply(self, |fold| fold.model = Some(base_model(model)));
                }
            }
            "session_closed" | "session_died" | "client_evicted" => {
                session.ended.store(true, Ordering::Relaxed);
                session.shut.notify_one();
            }
            "prompt_cancelled"
            | "pending_prompt_added"
            | "pending_prompt_completed"
            | "state_resync_required" => session.poke.notify_one(),
            _ => {}
        }
    }

    /// Asks the daemon what runs: at once when an event says something changed, every second
    /// while a turn runs or words wait, and every five whether it still runs the conversation
    /// for a client. One that it no longer runs, or that no client has and where nothing runs,
    /// is let go, so the daemon can unload it.
    async fn ask_loop(self: &Arc<Self>, session: &Arc<Session>) {
        let mut viewed_at = tokio::time::Instant::now();
        while !session.ended.load(Ordering::Relaxed) {
            let poked = tokio::select! {
                () = session.poke.notified() => true,
                () = tokio::time::sleep(EVERY) => false,
            };
            if session.ended.load(Ordering::Relaxed) {
                break;
            }
            if poked || session.lock().works() {
                session.refresh(self).await;
            }
            if viewed_at.elapsed() < VIEWED_EVERY {
                continue;
            }
            viewed_at = tokio::time::Instant::now();
            let status = session
                .daemon
                .get(&format!("/session/{}/status", session.id))
                .await;
            let gone = match status {
                Ok(status) if status.status == 200 => {
                    let clients = status
                        .json()
                        .map_or(0, |s| s["clientCount"].as_u64().unwrap_or(0));
                    clients == 0 && !session.lock().works()
                }
                Ok(_) => true,
                Err(_) => false,
            };
            if gone {
                session.ended.store(true, Ordering::Relaxed);
                session.shut.notify_one();
            }
        }
    }

    /// The stream closed: nothing the daemon ran is known to run.
    async fn closed(&self, session: &Arc<Session>) {
        session.ended.store(true, Ordering::Relaxed);
        session.poke.notify_one();
        let mut followed = self.followed.lock().await;
        if followed
            .get(&session.id)
            .is_some_and(|known| Arc::ptr_eq(known, session))
        {
            followed.remove(&session.id);
        }
        drop(followed);
        self.changed(&session.id, LiveState::default());
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

    /// `held`: a daemon that runs the conversation, with the prompt that runs as its token;
    /// none where no daemon does.
    pub async fn held(self: &Arc<Self>, home: &Path, root: &Path, id: &str) -> Option<Holder> {
        let session = self.door(home, root, id).await?;
        let token = session
            .lock()
            .running
            .as_ref()
            .map(|(prompt, _)| prompt.clone());
        Some(Holder::Person {
            door: Some(harness::Door::Picks),
            stop: HolderStop::Owner(token),
        })
    }

    /// `say`: words that steer go into the running turn at its next step; others, and words
    /// that steer where nothing runs, go in as a prompt, which the daemon runs after any that
    /// runs or waits.
    pub async fn say(self: &Arc<Self>, home: &Path, root: &Path, say: Say) -> Result<Said, Error> {
        let Some(session) = self.door(home, root, &say.external_id).await else {
            return Err(no_door());
        };
        session.refresh(self).await;
        let (running, pending) = {
            let fold = session.lock();
            (fold.running.is_some(), fold.pending == Some(true))
        };
        if say.mode == SayMode::Immediate && (running || pending) {
            return Err(Error::refused(
                refusal::HARNESS_BUSY,
                "a turn runs or words wait in Qwen Code",
            ));
        }
        if say.mode == SayMode::Steer && running {
            let answer = session
                .post(
                    "mid-turn-message",
                    &json!({"message": say.words, "rejectIfIdle": true}),
                )
                .await?;
            if answer["accepted"] == true
                && let Some(message) = answer["messageId"].as_str()
            {
                if let Some(id) = &say.id {
                    session.apply(self, |fold| fold.steered(message, id, now()));
                }
                return Ok(Said { took: true });
            }
            if answer["reason"] != "session_idle" {
                return Err(Error::refused(
                    refusal::REFUSED,
                    format!("Qwen Code's daemon did not take the words: {answer}"),
                ));
            }
        }
        let answer = session
            .post(
                "prompt",
                &json!({"prompt": [{"type": "text", "text": say.words}]}),
            )
            .await?;
        let Some(prompt) = answer["promptId"].as_str() else {
            return Ok(Said { took: false });
        };
        if let Some(id) = &say.id {
            session.apply(self, |fold| fold.asked(prompt, id, now()));
        }
        session.poke.notify_one();
        Ok(Said { took: true })
    }

    /// `pick`: the model of this conversation alone, through the daemon; the person's
    /// default stays. Qwen lists no thinking level a pick can name.
    pub async fn pick(
        self: &Arc<Self>,
        home: &Path,
        root: &Path,
        pick: Pick,
    ) -> Result<Picked, Error> {
        if let Some(thinking) = &pick.thinking {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                format!("Qwen Code has no thinking level {thinking} a pick can name"),
            ));
        }
        let Some(session) = self.door(home, root, &pick.external_id).await else {
            return Err(no_door());
        };
        if let Some(model) = &pick.model {
            let answer = session
                .daemon
                .post(
                    &format!("/session/{}/model", session.id),
                    &json!({"modelId": model, "_meta": {PERSIST_DEFAULT: false}}),
                )
                .await
                .map_err(|why| Error::refused(refusal::NO_ANSWER, why))?;
            if !answer.ok() {
                let why = answer.failure();
                let word = if why.contains("not found") {
                    refusal::MODEL_MISSING
                } else {
                    refusal::REFUSED
                };
                return Err(Error::refused(word, why));
            }
            let picked = answer.json().unwrap_or_default();
            let picked = picked["_meta"]["qwenModelSwitch"]["modelId"]
                .as_str()
                .unwrap_or(model);
            session.apply(self, |fold| fold.model = Some(base_model(picked)));
        }
        let fold = session.lock();
        Ok(Picked {
            model: fold.model.clone(),
            thinking: None,
        })
    }

    /// `stop` of the daemon: it cancels the running prompt, where that is still the one
    /// `held` named, and where a say is named, the one that took its words. Words that wait
    /// stay, and the next of them starts.
    pub async fn stop(
        self: &Arc<Self>,
        home: &Path,
        root: &Path,
        stop: &Stop,
    ) -> Result<Stopped, Error> {
        let Holder::Person {
            stop: HolderStop::Owner(token),
            ..
        } = &stop.holder
        else {
            return Ok(not_stopped("no Qwen Code daemon holds it"));
        };
        let Some(session) = self.door(home, root, &stop.external_id).await else {
            return Ok(not_stopped(
                "the Qwen Code daemon that held it no longer runs it",
            ));
        };
        session.refresh(self).await;
        let Some(target) = session
            .lock()
            .running
            .as_ref()
            .map(|(prompt, _)| prompt.clone())
        else {
            return Ok(not_stopped("no turn runs in Qwen Code"));
        };
        if let Some(say) = &stop.say {
            let took = session
                .lock()
                .says
                .get(&target)
                .is_some_and(|says| says.iter().any(|taken| &taken.say == say));
            if !took {
                return Err(Error::refused(
                    refusal::OTHER_TURN,
                    "the turn that runs did not take these words",
                ));
            }
        } else if token.as_ref().is_some_and(|token| *token != target) {
            return Ok(not_stopped("the turn it named ended; another runs"));
        }
        session.post("cancel", &json!({})).await?;
        let ended = tokio::time::timeout(STOPPED_WITHIN, async {
            loop {
                session.refresh(self).await;
                let still = session
                    .lock()
                    .running
                    .as_ref()
                    .is_some_and(|(prompt, _)| *prompt == target);
                if !still {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .is_ok();
        Ok(Stopped {
            stopped: ended,
            proof: Some(if ended {
                "Qwen Code's daemon cancelled the turn".into()
            } else {
                "Qwen Code's daemon took the cancel, but the turn still runs".into()
            }),
        })
    }

    /// The command that opens the conversation in a terminal through the daemon that runs
    /// it (`qwen attach`); none where no daemon does.
    pub async fn attach(self: &Arc<Self>, home: &Path, root: &Path, id: &str) -> Option<String> {
        let session = self.door(home, root, id).await?;
        let mut command = format!("qwen attach --port {} --session {id}", session.daemon.port);
        if let Some(workspace) = &session.workspace
            && session.daemon.primary.as_ref() != Some(workspace)
        {
            command.push_str(&format!(
                " --workspace '{}'",
                workspace.replace('\'', "'\\''")
            ));
        }
        Some(command)
    }
}

impl Session {
    fn lock(&self) -> std::sync::MutexGuard<'_, Fold> {
        lock(&self.fold)
    }

    /// `POST /session/<id>/<path>`: the daemon's answer where it took it, else a refusal
    /// with its words.
    async fn post(&self, path: &str, body: &Value) -> Result<Value, Error> {
        let answer = self
            .daemon
            .post(&format!("/session/{}/{path}", self.id), body)
            .await
            .map_err(|why| Error::refused(refusal::NO_ANSWER, why))?;
        if answer.status == 503 {
            return Err(Error::refused(refusal::HARNESS_BUSY, answer.failure()));
        }
        if !answer.ok() {
            return Err(Error::refused(refusal::REFUSED, answer.failure()));
        }
        Ok(answer.json().unwrap_or_default())
    }

    async fn get(&self, path: &str) -> Option<Value> {
        let answer = self
            .daemon
            .get(&format!("/session/{}/{path}", self.id))
            .await
            .ok()?;
        (answer.status == 200).then(|| answer.json()).flatten()
    }

    /// The model the daemon runs the conversation on (`/daemon/status?detail=full`).
    async fn model(&self) -> Option<String> {
        let status = self
            .daemon
            .get("/daemon/status?detail=full")
            .await
            .ok()?
            .json()?;
        let model = status["full"]["sessions"]
            .as_array()?
            .iter()
            .find(|session| session["sessionId"] == self.id.as_str())?["currentModelId"]
            .as_str()?;
        Some(base_model(model))
    }

    /// Ask the daemon which prompt runs and whether words wait, and send the state where
    /// that changed.
    async fn refresh(&self, live: &Live) {
        let (Some(current), Some(queued), Some(steered)) = (
            self.get("turns/current").await,
            self.get("pending-prompts").await,
            self.get("mid-turn-messages").await,
        ) else {
            return;
        };
        let running = (current["state"] == "running")
            .then(|| {
                let prompt = current["promptId"].as_str()?.to_owned();
                Some((prompt, current["startedAt"].as_u64().unwrap_or_else(now)))
            })
            .flatten();
        let waiting = queued["pendingPrompts"]
            .as_array()
            .is_some_and(|prompts| prompts.iter().any(|prompt| prompt["state"] == "queued"))
            || steered["messages"]
                .as_array()
                .is_some_and(|messages| !messages.is_empty());
        self.apply(live, |fold| fold.observed(running, waiting, now()));
    }

    /// Fold one change into the conversation, and send its state where that changed.
    fn apply<T>(&self, live: &Live, change: impl FnOnce(&mut Fold) -> T) -> T {
        let (out, state) = {
            let mut fold = self.lock();
            let out = change(&mut fold);
            (out, fold.changed())
        };
        if let Some(state) = state {
            live.changed(&self.id, state);
        }
        out
    }
}

/// A say that went in, and when.
#[derive(Clone)]
struct Taken {
    say: String,
    at: u64,
}

/// What the daemon's events and answers said of one conversation.
#[derive(Default)]
struct Fold {
    /// The prompt that runs, and when it began.
    running: Option<(String, u64)>,
    pending: Option<bool>,
    /// Steery's says, by the prompt that took them.
    says: HashMap<String, Vec<Taken>>,
    /// Says that steer, by their message, until a turn takes them.
    steered: HashMap<String, Taken>,
    /// The prompts that ended lately, and the failure of each, so a say whose prompt ended
    /// before its answer came still settles.
    finished: VecDeque<(String, u64, Option<String>)>,
    /// The failure of a prompt that runs.
    failures: HashMap<String, String>,
    model: Option<String>,
    context_left: Option<f64>,
    calls: Vec<LiveCall>,
    ended: VecDeque<LiveEnded>,
    settled: VecDeque<LiveSettled>,
    /// Permissions open in front of the person, by their request.
    dialogs: Vec<(String, Dialog)>,
    sent: Option<LiveState>,
}

impl Fold {
    fn works(&self) -> bool {
        self.running.is_some() || self.pending == Some(true)
    }

    /// One ACP update of the conversation: a call begins or ends, or the context fills.
    fn update(&mut self, update: &Value, at: u64) {
        let call = update["toolCallId"].as_str().unwrap_or_default();
        match update["sessionUpdate"].as_str().unwrap_or_default() {
            "tool_call" if !call.is_empty() => {
                let known = self.calls.iter().any(|known| known.id == call)
                    || self.ended.iter().any(|known| known.id == call);
                if !known {
                    let tool = update["_meta"]["toolName"]
                        .as_str()
                        .or_else(|| update["title"].as_str())
                        .unwrap_or("tool");
                    self.calls.push(LiveCall {
                        id: call.to_owned(),
                        tool: tool.to_owned(),
                        since: at,
                        children: Vec::new(),
                        jobs: Vec::new(),
                    });
                }
            }
            "tool_call_update" => {
                let done = matches!(update["status"].as_str(), Some("completed" | "failed"));
                if done && let Some(index) = self.calls.iter().position(|known| known.id == call) {
                    let call = self.calls.remove(index);
                    self.end(call.id, at);
                }
            }
            "usage_update" => {
                if let (Some(used), Some(size)) = (update["used"].as_f64(), update["size"].as_f64())
                    && size > 0.0
                {
                    self.context_left = Some((1.0 - used / size).clamp(0.0, 1.0));
                }
            }
            _ => {}
        }
    }

    /// A say went in as prompt `prompt`.
    fn asked(&mut self, prompt: &str, say: &str, at: u64) {
        let taken = Taken {
            say: say.to_owned(),
            at,
        };
        if let Some((_, ended, failure)) = self.finished.iter().find(|(done, ..)| done == prompt) {
            let (ended, failure) = (*ended, failure.clone());
            self.settle(vec![taken], ended, failure);
            return;
        }
        self.says.entry(prompt.to_owned()).or_default().push(taken);
    }

    /// A say that steers went in as message `message`.
    fn steered(&mut self, message: &str, say: &str, at: u64) {
        let taken = Taken {
            say: say.to_owned(),
            at,
        };
        // The message may have gone in before its answer came.
        if let Some((prompt, _)) = &self.running
            && prompt == message
        {
            self.says.entry(message.to_owned()).or_default().push(taken);
            return;
        }
        self.steered.insert(message.to_owned(), taken);
    }

    /// The running turn took messages `ids` at its next step.
    fn injected(&mut self, ids: &[&str]) {
        let Some((prompt, _)) = self.running.clone() else {
            return;
        };
        for id in ids {
            if let Some(taken) = self.steered.remove(*id) {
                self.says.entry(prompt.clone()).or_default().push(taken);
            }
        }
    }

    /// A waiting prompt starts: a message that steered and missed its turn runs as its own.
    fn started(&mut self, prompt: &str) {
        if let Some(taken) = self.steered.remove(prompt) {
            self.says.entry(prompt.to_owned()).or_default().push(taken);
        }
    }

    fn failed(&mut self, prompt: &str, error: String) {
        if !prompt.is_empty() {
            self.failures.insert(prompt.to_owned(), error);
        }
    }

    /// Prompt `prompt` ended: its says settle, and its calls end.
    fn finished(&mut self, prompt: &str, at: u64) {
        if prompt.is_empty() || self.finished.iter().any(|(done, ..)| done == prompt) {
            return;
        }
        let failure = self.failures.remove(prompt);
        self.finished
            .push_back((prompt.to_owned(), at, failure.clone()));
        if self.finished.len() > FINISHED {
            self.finished.pop_front();
        }
        if self
            .running
            .as_ref()
            .is_some_and(|(running, _)| running == prompt)
        {
            self.running = None;
            for call in std::mem::take(&mut self.calls) {
                self.end(call.id, at);
            }
            self.dialogs.clear();
        }
        let says = self.says.remove(prompt).unwrap_or_default();
        self.settle(says, at, failure);
    }

    /// What the daemon answered: the prompt that runs, and whether words wait. A prompt that
    /// no longer runs ended, where no event said so.
    fn observed(&mut self, running: Option<(String, u64)>, waiting: bool, at: u64) {
        self.pending = Some(waiting);
        let before = self.running.as_ref().map(|(prompt, _)| prompt.clone());
        let now_runs = running.as_ref().map(|(prompt, _)| prompt.clone());
        if before != now_runs
            && let Some(before) = before
        {
            self.finished(&before, at);
        }
        if now_runs.is_some() {
            self.running = running;
        }
    }

    fn settle(&mut self, says: Vec<Taken>, at: u64, error: Option<String>) {
        let Some(began_at) = says.iter().map(|taken| taken.at).min() else {
            return;
        };
        self.settled.push_back(LiveSettled {
            began_at,
            at,
            says: says.into_iter().map(|taken| taken.say).collect(),
            failed: error.is_some(),
            error,
        });
        if self.settled.len() > SETTLED {
            self.settled.pop_front();
        }
    }

    fn end(&mut self, id: String, at: u64) {
        self.ended.push_back(LiveEnded { id, at });
        if self.ended.len() > ENDED {
            self.ended.pop_front();
        }
    }

    fn state(&self) -> LiveState {
        LiveState {
            turn: self.running.as_ref().map(|(prompt, began_at)| LiveTurn {
                began_at: *began_at,
                says: self
                    .says
                    .get(prompt)
                    .into_iter()
                    .flatten()
                    .map(|taken| taken.say.clone())
                    .collect(),
            }),
            idle: self
                .pending
                .map(|pending| self.running.is_none() && !pending),
            pending: self.pending,
            typing: None,
            model: self.model.clone(),
            thinking: None,
            context_left: self.context_left,
            calls: self.calls.clone(),
            ended: self.ended.iter().cloned().collect(),
            dialog: self.dialogs.last().map(|(_, dialog)| dialog.clone()),
            settled: self.settled.iter().cloned().collect(),
            ..LiveState::default()
        }
    }

    /// The state, where it differs from the one last sent.
    fn changed(&mut self) -> Option<LiveState> {
        let state = self.state();
        if self.sent.as_ref() == Some(&state) {
            return None;
        }
        self.sent = Some(state.clone());
        Some(state)
    }
}

/// A model as `models` and the chat file name it: Qwen's ACP id without its `(<auth type>)`
/// (`parseAcpBaseModelId`, `cli/src/utils/acpModelUtils.ts:281`), and a runtime snapshot's
/// `$runtime|<auth type>|<model>` without its prefix.
fn base_model(id: &str) -> String {
    let trimmed = id.trim();
    let bare = match (trimmed.rfind('('), trimmed.ends_with(')')) {
        (Some(open), true) => &trimmed[..open],
        _ => trimmed,
    };
    bare.strip_prefix("$runtime|")
        .and_then(|rest| rest.split_once('|'))
        .map_or(bare, |(_, model)| model)
        .to_owned()
}

fn no_door() -> Error {
    Error::refused(
        refusal::NO_DOOR,
        "no Qwen Code daemon runs this conversation: a daemon (`qwen serve`) runs only the \
         conversations that a client, such as its Web Shell or `qwen attach`, loads in it",
    )
}

fn not_stopped(why: &str) -> Stopped {
    Stopped {
        stopped: false,
        proof: Some(why.to_owned()),
    }
}

fn cut(text: &str, at_most: usize) -> String {
    text.chars().take(at_most).collect()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
