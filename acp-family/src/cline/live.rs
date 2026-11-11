//! Cline's own door (PROTOCOL.md §4.3): Cline's hub. The hub is a websocket server on a
//! loopback port that `cline hub ensure` starts, and its lock file names it. A `cline` window
//! that finds the hub running joins it, and the hub then runs that window's turns. The hub
//! takes words (`session.send_input`), a model (`session.update_connection`) and a stop
//! (`run.abort`) from any client, and its event stream says what runs. The driver sends that
//! as `session/changed`.
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    self, Dialog, DialogKind, Holder, HolderStop, LiveCall, LiveEnded, LiveSettled, LiveState,
    LiveTurn, Model, Pick, Picked, Said, Say, SayMode, SessionChanged, Stop, Stopped,
};
use steery_sdk::{Error, Notifier, method, refusal};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use super::place::Place;

/// How long the hub has to answer a request, and a stop to end the turn.
const WITHIN: Duration = Duration::from_secs(10);
/// How many ended calls and settled turns a state carries.
const ENDED: usize = 64;
const SETTLED: usize = 16;
/// The longest error and dialog title a state carries.
const ERROR_AT_MOST: usize = 600;
const TITLE_AT_MOST: usize = 200;

/// The hub the driver speaks to, while its connection lives.
pub struct Live {
    notifier: Notifier,
    hub: tokio::sync::Mutex<Option<Arc<Hub>>>,
}

/// One connection to Cline's hub, and what its events said of each conversation followed.
struct Hub {
    /// The client id this connection registered under.
    client: String,
    /// The hub's process, from its lock file.
    pid: u32,
    out: mpsc::UnboundedSender<String>,
    next: AtomicU64,
    ended: AtomicBool,
    /// Replies awaited, by request id.
    replies: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// Says that await the turn they start, by request id.
    starts: Mutex<HashMap<String, oneshot::Sender<()>>>,
    followed: Mutex<HashMap<String, Fold>>,
}

impl Live {
    pub fn new(notifier: Notifier) -> Self {
        Self {
            notifier,
            hub: tokio::sync::Mutex::default(),
        }
    }

    /// The hub's connection: the one open, else a new one where the lock names a hub.
    async fn hub(self: &Arc<Self>, place: &Place) -> Option<Arc<Hub>> {
        let mut held = self.hub.lock().await;
        if let Some(hub) = held
            .as_ref()
            .filter(|hub| !hub.ended.load(Ordering::Relaxed))
        {
            return Some(Arc::clone(hub));
        }
        let hub = self.connect(place).await?;
        *held = Some(Arc::clone(&hub));
        Some(hub)
    }

    async fn connect(self: &Arc<Self>, place: &Place) -> Option<Arc<Hub>> {
        let lock: Value = serde_json::from_str(
            &std::fs::read_to_string(place.dir.join("data/locks/hub/production.json")).ok()?,
        )
        .ok()?;
        let url = text(&lock, "url")?;
        let token = text(&lock, "authToken")?;
        let pid = u32::try_from(lock.get("pid")?.as_u64()?).ok()?;
        let authority = url.strip_prefix("ws://")?.split('/').next()?;
        let mut request = url.into_client_request().ok()?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(&format!("cline-hub-auth.{token}")).ok()?,
        );
        let stream = tokio::time::timeout(WITHIN, tokio::net::TcpStream::connect(authority))
            .await
            .ok()?
            .ok()?;
        let (socket, _) =
            tokio::time::timeout(WITHIN, tokio_tungstenite::client_async(request, stream))
                .await
                .ok()?
                .ok()?;
        let (mut sink, mut source) = socket.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
        let hub = Arc::new(Hub {
            client: format!("steery-{}", std::process::id()),
            pid,
            out,
            next: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            replies: Mutex::default(),
            starts: Mutex::default(),
            followed: Mutex::default(),
        });
        tokio::spawn(async move {
            while let Some(frame) = outgoing.recv().await {
                if sink.send(Message::text(frame)).await.is_err() {
                    break;
                }
            }
        });
        let live = Arc::clone(self);
        let reading = Arc::clone(&hub);
        tokio::spawn(async move {
            while let Some(Ok(message)) = source.next().await {
                if let Message::Text(frame) = message
                    && let Ok(frame) = serde_json::from_str::<Value>(&frame)
                {
                    live.frame(&reading, frame);
                }
            }
            live.closed(&reading);
        });
        hub.ask(
            "client.register",
            json!({"clientType": "steery", "displayName": "Steery"}),
            None,
        )
        .await
        .ok()?;
        Some(hub)
    }

    /// One frame of the hub: a reply to its request, or an event of a conversation.
    fn frame(&self, hub: &Hub, frame: Value) {
        let Some(envelope) = frame.get("envelope") else {
            return;
        };
        match text(&frame, "kind") {
            Some("reply") => {
                if let Some(id) = text(envelope, "requestId")
                    && let Some(reply) = lock(&hub.replies).remove(id)
                {
                    let _ = reply.send(envelope.clone());
                }
            }
            Some("event") => {
                let null = Value::Null;
                let payload = envelope.get("payload").unwrap_or(&null);
                let event = text(envelope, "event").unwrap_or_default();
                if event == "run.started"
                    && let Some(id) = text(payload, "requestId")
                    && let Some(start) = lock(&hub.starts).remove(id)
                {
                    let _ = start.send(());
                }
                let Some(session) = text(envelope, "sessionId") else {
                    return;
                };
                let at = envelope
                    .get("timestamp")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(now);
                let changed = {
                    let mut followed = lock(&hub.followed);
                    let Some(fold) = followed.get_mut(session) else {
                        return;
                    };
                    fold.apply(event, payload, at);
                    fold.changed()
                };
                if let Some(state) = changed {
                    self.changed(session, state);
                }
            }
            _ => {}
        }
    }

    /// The connection closed: nothing the hub ran is known to run.
    fn closed(&self, hub: &Hub) {
        hub.ended.store(true, Ordering::Relaxed);
        lock(&hub.replies).clear();
        lock(&hub.starts).clear();
        let followed = std::mem::take(&mut *lock(&hub.followed));
        for id in followed.into_keys() {
            self.changed(&id, LiveState::default());
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

    /// The hub of conversation `id`, where a person's Cline window in it shows it; the
    /// driver then follows the conversation's events.
    async fn door(self: &Arc<Self>, place: &Place, id: &str) -> Option<Arc<Hub>> {
        let hub = self.hub(place).await?;
        let session = hub.ask("session.get", json!({}), Some(id)).await.ok()?;
        let session = session.get("session")?;
        let clients = hub.ask("client.list", json!({}), None).await.ok()?;
        let windows = clients.get("clients")?.as_array()?;
        let participants = session.get("participants")?.as_array()?;
        let shown = participants
            .iter()
            .filter_map(|p| text(p, "clientId"))
            .any(|client| {
                windows.iter().any(|w| {
                    text(w, "clientId") == Some(client) && text(w, "clientType") == Some("cli")
                })
            });
        if !shown {
            return None;
        }
        let first = {
            let mut followed = lock(&hub.followed);
            let first = !followed.contains_key(id);
            followed
                .entry(id.to_owned())
                .or_default()
                .session(session, now());
            first
        };
        if first {
            let _ = hub.out.send(
                json!({"kind": "stream.subscribe", "clientId": hub.client, "sessionId": id})
                    .to_string(),
            );
        }
        let state = lock(&hub.followed).get_mut(id).and_then(Fold::changed);
        if let Some(state) = state {
            self.changed(id, state);
        }
        Some(hub)
    }

    /// `held`: a person's window in the hub, with the turn that runs as its token; none
    /// where no window that joined Cline's hub shows the conversation.
    pub async fn held(self: &Arc<Self>, place: &Place, id: &str) -> Option<Holder> {
        let hub = self.door(place, id).await?;
        Some(Holder::Person {
            door: Some(harness::Door::Picks),
            stop: HolderStop::Owner(hub.token(id)),
        })
    }

    /// `say`: the words into the conversation through the hub, its id named in the turn
    /// that takes them.
    pub async fn say(self: &Arc<Self>, place: &Place, say: Say) -> Result<Said, Error> {
        let Some(hub) = self.door(place, &say.external_id).await else {
            return Err(no_door());
        };
        let (running, pending) = hub.running(&say.external_id);
        let delivery = match say.mode {
            SayMode::Immediate if running || pending => {
                return Err(Error::refused(
                    refusal::HARNESS_BUSY,
                    "a turn runs or words wait in Cline",
                ));
            }
            _ if !running => None,
            SayMode::Steer => Some("steer"),
            SayMode::Queue | SayMode::Immediate => Some("queue"),
        };
        if let Some(fold) = lock(&hub.followed).get_mut(&say.external_id) {
            fold.said(say.id.clone(), &say.words, delivery);
        }
        let mut payload = json!({"prompt": say.words});
        if let Some(delivery) = delivery {
            payload["delivery"] = json!(delivery);
            hub.ask("session.send_input", payload, Some(&say.external_id))
                .await
                .map_err(no_answer)?;
            return Ok(Said { took: true });
        }
        // Words that start a turn are answered only when the turn ends; the hub's
        // `run.started` for this request says the turn took them.
        let (id, reply) = hub.send("session.send_input", payload, Some(&say.external_id));
        let (start, started) = oneshot::channel();
        lock(&hub.starts).insert(id.clone(), start);
        let took = tokio::select! {
            _ = started => true,
            answer = reply => match answer {
                Ok(envelope) if envelope.get("ok") == Some(&json!(false)) => {
                    return Err(no_answer(failure(&envelope)));
                }
                Ok(_) => true,
                Err(_) => false,
            },
            _ = tokio::time::sleep(WITHIN) => false,
        };
        lock(&hub.starts).remove(&id);
        Ok(Said { took })
    }

    /// `pick`: the model the conversation's next request runs on, through the hub, checked
    /// against the models Cline last listed. Cline's ACP server lists no thinking level.
    pub async fn pick(
        self: &Arc<Self>,
        place: &Place,
        listed: &[Model],
        pick: Pick,
    ) -> Result<Picked, Error> {
        if pick.thinking.is_some() {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Cline lists no thinking level for its models",
            ));
        }
        let Some(hub) = self.door(place, &pick.external_id).await else {
            return Err(no_door());
        };
        if let Some(model) = &pick.model {
            if !listed.is_empty() && !listed.iter().any(|known| &known.model == model) {
                return Err(Error::refused(
                    refusal::MODEL_MISSING,
                    format!("Cline lists no model {model}"),
                ));
            }
            hub.ask(
                "session.update_connection",
                json!({"updates": {"modelId": model}}),
                Some(&pick.external_id),
            )
            .await
            .map_err(|why| Error::refused(refusal::SELECTION_UNSUPPORTED, why))?;
        }
        let now = hub
            .ask("session.get", json!({}), Some(&pick.external_id))
            .await
            .map_err(no_answer)?;
        let model = now
            .pointer("/session/metadata/model")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(fold) = lock(&hub.followed).get_mut(&pick.external_id) {
            fold.model.clone_from(&model);
        }
        Ok(Picked {
            model,
            thinking: None,
        })
    }

    /// `stop` of a window's owner: the hub aborts the turn that runs, where it is still the
    /// one `held` named, and where a say is named, the one its words went into.
    pub async fn stop(self: &Arc<Self>, place: &Place, stop: &Stop) -> Result<Stopped, Error> {
        let Holder::Person {
            stop: HolderStop::Owner(token),
            ..
        } = &stop.holder
        else {
            return Ok(not_stopped("no window in Cline's hub holds it"));
        };
        let id = &stop.external_id;
        let Some(hub) = self.door(place, id).await else {
            return Ok(not_stopped("the Cline window that held it left the hub"));
        };
        if !hub.running(id).0 {
            return Ok(not_stopped("no turn runs in Cline"));
        }
        if let Some(say) = &stop.say {
            let took = lock(&hub.followed)
                .get(id)
                .is_some_and(|fold| fold.says.contains(say));
            if !took {
                return Err(Error::refused(
                    refusal::OTHER_TURN,
                    "the turn that runs did not take these words",
                ));
            }
        } else if token.is_some() && *token != hub.token(id) {
            return Ok(not_stopped("the turn it named ended; another runs"));
        }
        hub.ask(
            "run.abort",
            json!({"reason": "Steery stopped the turn"}),
            Some(id),
        )
        .await
        .map_err(no_answer)?;
        // The hub answers every abort as applied; the conversation going idle is the proof.
        let ended = tokio::time::timeout(WITHIN, async {
            while hub.running(id).0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .is_ok();
        Ok(Stopped {
            stopped: ended,
            proof: Some(if ended {
                "Cline's hub aborted the turn and the conversation went idle".into()
            } else {
                "Cline's hub took the abort, but the turn still runs".into()
            }),
        })
    }
}

impl Hub {
    /// Send a command; its reply comes on the receiver.
    fn send(
        &self,
        command: &str,
        payload: Value,
        session: Option<&str>,
    ) -> (String, oneshot::Receiver<Value>) {
        let id = format!(
            "{}-{}",
            self.client,
            self.next.fetch_add(1, Ordering::Relaxed) + 1
        );
        let mut envelope = json!({
            "version": "v1",
            "command": command,
            "requestId": id,
            "clientId": self.client,
            "payload": payload,
        });
        if let Some(session) = session {
            envelope["sessionId"] = json!(session);
        }
        let (reply, answer) = oneshot::channel();
        lock(&self.replies).insert(id.clone(), reply);
        let _ = self
            .out
            .send(json!({"kind": "command", "envelope": envelope}).to_string());
        (id, answer)
    }

    /// A command's reply payload, or the hub's words where it refused.
    async fn ask(
        &self,
        command: &str,
        payload: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        let (id, answer) = self.send(command, payload, session);
        let envelope = tokio::time::timeout(WITHIN, answer).await;
        lock(&self.replies).remove(&id);
        let envelope = envelope
            .map_err(|_| format!("Cline's hub did not answer `{command}`"))?
            .map_err(|_| "the connection to Cline's hub closed".to_owned())?;
        if envelope.get("ok") != Some(&json!(true)) {
            return Err(failure(&envelope));
        }
        Ok(envelope.get("payload").cloned().unwrap_or(Value::Null))
    }

    /// Whether a turn of conversation `id` runs, and whether words wait.
    fn running(&self, id: &str) -> (bool, bool) {
        lock(&self.followed).get(id).map_or((false, false), |fold| {
            (fold.began.is_some(), fold.pending == Some(true))
        })
    }

    /// The token of the turn that runs: the hub and when the turn began.
    fn token(&self, id: &str) -> Option<String> {
        let began = lock(&self.followed).get(id)?.began?;
        Some(format!("{}:{began}", self.pid))
    }
}

/// What the hub's events said of one conversation.
#[derive(Default)]
struct Fold {
    /// When the turn that runs began.
    began: Option<u64>,
    /// The say ids the turn that runs took.
    says: Vec<String>,
    /// The say ids that the next turn takes.
    next: Vec<String>,
    /// Words that wait in Cline's queue: their text, their say's id, and whether they steer.
    waiting: Vec<(String, String, bool)>,
    pending: Option<bool>,
    model: Option<String>,
    calls: Vec<LiveCall>,
    /// How many calls began: Cline names a call only by its tool.
    count: u64,
    ended: VecDeque<LiveEnded>,
    settled: VecDeque<LiveSettled>,
    /// Approvals open in front of the person, by id.
    dialogs: Vec<(String, Dialog)>,
    /// The error the turn that runs failed on.
    error: Option<String>,
    /// The state last sent.
    sent: Option<LiveState>,
}

impl Fold {
    /// The conversation as the hub keeps it: whether it runs, and its model.
    fn session(&mut self, session: &Value, at: u64) {
        if let Some(model) = session.pointer("/metadata/model").and_then(Value::as_str) {
            self.model = Some(model.to_owned());
        }
        match text(session, "status") {
            Some("running") => self.begin(at),
            Some(_) => self.close(at),
            None => {}
        }
    }

    /// Words given: the next turn takes them, or they wait in Cline's queue.
    fn said(&mut self, id: Option<String>, words: &str, delivery: Option<&str>) {
        let Some(id) = id else {
            return;
        };
        match delivery {
            None => self.next.push(id),
            Some(delivery) => self
                .waiting
                .push((words.to_owned(), id, delivery == "steer")),
        }
    }

    fn apply(&mut self, event: &str, payload: &Value, at: u64) {
        match event {
            "session.updated" => {
                if let Some(session) = payload.get("session") {
                    self.session(session, at);
                }
            }
            "session.pending_prompts" => {
                let prompts = payload.get("prompts").and_then(Value::as_array);
                self.pending = Some(prompts.is_some_and(|prompts| !prompts.is_empty()));
            }
            "session.pending_prompt_submitted" => {
                let words = payload.pointer("/prompt/prompt").and_then(Value::as_str);
                let Some(words) = words else {
                    return;
                };
                let Some(at_wait) = self
                    .waiting
                    .iter()
                    .position(|(waiting, _, _)| words.contains(waiting.as_str()))
                else {
                    return;
                };
                let (_, id, steers) = self.waiting.remove(at_wait);
                if steers && self.began.is_some() {
                    self.says.push(id);
                } else if self.began.is_some() {
                    // Queued words run as the next turn.
                    self.close(at);
                    self.next.push(id);
                    self.begin(at);
                } else {
                    self.next.push(id);
                }
            }
            "tool.started" => {
                self.count += 1;
                let tool = text(payload, "toolName").unwrap_or("tool").to_owned();
                self.calls.push(LiveCall {
                    id: format!("{tool}-{}", self.count),
                    tool,
                    since: at,
                    children: Vec::new(),
                    jobs: Vec::new(),
                });
            }
            "tool.finished" => {
                let tool = text(payload, "toolName").unwrap_or("tool");
                if let Some(index) = self.calls.iter().position(|call| call.tool == tool) {
                    let call = self.calls.remove(index);
                    self.end(call.id, at);
                }
            }
            "approval.requested" => {
                if let Some(id) = text(payload, "approvalId") {
                    let tool = text(payload, "toolName").unwrap_or_default();
                    self.dialogs.push((
                        id.to_owned(),
                        Dialog {
                            kind: DialogKind::Confirm,
                            title: cut(&format!("Allow {tool}?"), TITLE_AT_MOST),
                            since: at,
                        },
                    ));
                }
            }
            "approval.resolved" => {
                let id = text(payload, "approvalId").unwrap_or_default();
                self.dialogs.retain(|(open, _)| open != id);
            }
            "run.failed" => {
                let error = payload
                    .get("error")
                    .and_then(|error| error.as_str().or_else(|| text(error, "message")))
                    .unwrap_or("the turn failed");
                self.error = Some(cut(error, ERROR_AT_MOST));
            }
            _ => {}
        }
    }

    fn begin(&mut self, at: u64) {
        if self.began.is_none() {
            self.began = Some(at);
            self.says.append(&mut self.next);
            self.error = None;
        }
    }

    /// The turn that ran ended at `at`: a turn Steery's words began settles.
    fn close(&mut self, at: u64) {
        for call in std::mem::take(&mut self.calls) {
            self.end(call.id, at);
        }
        self.dialogs.clear();
        let error = self.error.take();
        let Some(began_at) = self.began.take() else {
            return;
        };
        let says = std::mem::take(&mut self.says);
        if says.is_empty() {
            return;
        }
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

    fn end(&mut self, id: String, at: u64) {
        self.ended.push_back(LiveEnded { id, at });
        if self.ended.len() > ENDED {
            self.ended.pop_front();
        }
    }

    fn state(&self) -> LiveState {
        LiveState {
            turn: self.began.map(|began_at| LiveTurn {
                began_at,
                says: self.says.clone(),
            }),
            idle: Some(self.began.is_none() && self.pending != Some(true)),
            pending: self.pending,
            typing: None,
            model: self.model.clone(),
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

fn no_door() -> Error {
    Error::refused(
        refusal::NO_DOOR,
        "no Cline window in Cline's hub shows this conversation: a window opened while no \
         hub ran runs its turns itself",
    )
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

/// The hub's words for a refused command.
fn failure(envelope: &Value) -> String {
    let error = envelope.get("error");
    error
        .and_then(|error| text(error, "message").or_else(|| error.as_str()))
        .unwrap_or("Cline's hub refused the command")
        .to_owned()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_say_that_starts_a_turn_settles_with_it() {
        let mut fold = Fold::default();
        fold.said(Some("s1".into()), "hello", None);
        fold.session(
            &json!({"status": "running", "metadata": {"model": "m"}}),
            10,
        );
        let state = fold.state();
        assert_eq!(
            state.turn.as_ref().map(|turn| turn.says.clone()),
            Some(vec!["s1".to_owned()])
        );
        assert_eq!(state.model.as_deref(), Some("m"));
        fold.apply(
            "session.updated",
            &json!({"session": {"status": "idle"}}),
            20,
        );
        let state = fold.state();
        assert!(state.turn.is_none());
        assert_eq!(state.settled.len(), 1);
        assert_eq!((state.settled[0].began_at, state.settled[0].at), (10, 20));
    }

    #[test]
    fn steered_words_join_the_running_turn() {
        let mut fold = Fold::default();
        fold.session(&json!({"status": "running"}), 1);
        fold.said(Some("s2".into()), "turn left", Some("steer"));
        fold.apply(
            "session.pending_prompts",
            &json!({"prompts": [{"prompt": "turn left"}]}),
            2,
        );
        assert_eq!(fold.state().pending, Some(true));
        fold.apply(
            "session.pending_prompt_submitted",
            &json!({"prompt": {"prompt": "turn left", "delivery": "steer"}}),
            3,
        );
        assert_eq!(fold.says, vec!["s2".to_owned()]);
        assert_eq!(fold.began, Some(1));
    }

    #[test]
    fn queued_words_open_the_next_turn() {
        let mut fold = Fold::default();
        fold.session(&json!({"status": "running"}), 1);
        fold.said(Some("q".into()), "later", Some("queue"));
        fold.apply(
            "session.pending_prompt_submitted",
            &json!({"prompt": {"prompt": "later", "delivery": "queue"}}),
            5,
        );
        assert_eq!(
            (fold.began, fold.says.clone()),
            (Some(5), vec!["q".to_owned()])
        );
    }

    #[test]
    fn a_tool_call_runs_until_it_finishes() {
        let mut fold = Fold::default();
        fold.apply("tool.started", &json!({"toolName": "read_files"}), 1);
        assert_eq!(fold.state().calls.len(), 1);
        fold.apply("tool.finished", &json!({"toolName": "read_files"}), 2);
        let state = fold.state();
        assert!(state.calls.is_empty());
        assert_eq!(state.ended[0].at, 2);
    }
}
