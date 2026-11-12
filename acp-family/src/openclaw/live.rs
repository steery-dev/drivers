//! OpenClaw's own door (PROTOCOL.md §4.3): its gateway. Every client of OpenClaw (the TUI,
//! the Control UI, the apps, the channels, `openclaw acp`) runs its turns through the
//! gateway's WebSocket, `ws://127.0.0.1:<port>`. The driver joins it as an operator with the
//! gateway's own secret from `openclaw.json`, subscribes to every session's events
//! (`sessions.subscribe`), takes words (`chat.send`), a model and a thinking level
//! (`sessions.patch`) and a stop (`chat.abort`), and sends what runs as `session/changed`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

use super::store::Store;
use crate::turn::drive::now;

/// The gateway protocol this driver speaks (`connect.minProtocol`, `maxProtocol`).
const PROTOCOL: u64 = 4;
/// The gateway's port where neither `OPENCLAW_GATEWAY_PORT` nor `gateway.port` names one
/// (`DEFAULT_GATEWAY_PORT`).
const PORT: u16 = 18789;
/// How long the gateway has to answer a request.
const WITHIN: Duration = Duration::from_secs(10);
/// How long a stop waits for the run to end.
const STOPPED_WITHIN: Duration = Duration::from_millis(2500);
/// How many ended calls and settled turns a state carries.
const ENDED: usize = 64;
const SETTLED: usize = 16;
/// The longest error and dialog title a state carries.
const ERROR_AT_MOST: usize = 600;
const TITLE_AT_MOST: usize = 200;

pub struct Live {
    notifier: Notifier,
    gateway: tokio::sync::Mutex<Option<Arc<Gateway>>>,
}

/// One connection to OpenClaw's gateway, and what its events said of each session followed.
struct Gateway {
    out: mpsc::UnboundedSender<String>,
    next: AtomicU64,
    ended: AtomicBool,
    /// Replies awaited, by request id.
    replies: Mutex<HashMap<String, oneshot::Sender<Result<Value, Refused>>>>,
    /// The sessions followed, by their key.
    followed: Mutex<HashMap<String, Fold>>,
}

impl Live {
    pub fn new(notifier: Notifier) -> Self {
        Self {
            notifier,
            gateway: tokio::sync::Mutex::default(),
        }
    }

    /// The gateway's connection: the one open, else a new one.
    async fn gateway(self: &Arc<Self>, root: &Path) -> Result<Arc<Gateway>, String> {
        let mut held = self.gateway.lock().await;
        if let Some(gateway) = held
            .as_ref()
            .filter(|gateway| !gateway.ended.load(Ordering::Relaxed))
        {
            return Ok(Arc::clone(gateway));
        }
        let gateway = self.connect(root).await?;
        *held = Some(Arc::clone(&gateway));
        Ok(gateway)
    }

    /// Dial the gateway on loopback, answer its `connect.challenge` with `connect` as an
    /// operator, and subscribe to every session's events.
    async fn connect(self: &Arc<Self>, root: &Path) -> Result<Arc<Gateway>, String> {
        let config = std::fs::read_to_string(root.join("openclaw.json"))
            .ok()
            .and_then(|text| crate::jsonc::parse(&text).ok())
            .unwrap_or(Value::Null);
        let authority = format!("127.0.0.1:{}", port(&config));
        let unreached = || format!("OpenClaw's gateway does not answer on {authority}");
        let request = format!("ws://{authority}")
            .into_client_request()
            .map_err(|_| unreached())?;
        let stream = tokio::time::timeout(WITHIN, tokio::net::TcpStream::connect(&authority))
            .await
            .map_err(|_| unreached())?
            .map_err(|_| unreached())?;
        let (socket, _) =
            tokio::time::timeout(WITHIN, tokio_tungstenite::client_async(request, stream))
                .await
                .map_err(|_| unreached())?
                .map_err(|_| unreached())?;
        let (mut sink, mut source) = socket.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
        let gateway = Arc::new(Gateway {
            out,
            next: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            replies: Mutex::default(),
            followed: Mutex::default(),
        });
        tokio::spawn(async move {
            while let Some(frame) = outgoing.recv().await {
                if sink.send(Message::text(frame)).await.is_err() {
                    break;
                }
            }
        });
        let (challenged, challenge) = oneshot::channel();
        let live = Arc::clone(self);
        let reading = Arc::clone(&gateway);
        tokio::spawn(async move {
            let mut challenged = Some(challenged);
            while let Some(Ok(message)) = source.next().await {
                let Message::Text(frame) = message else {
                    continue;
                };
                let Ok(frame) = serde_json::from_str::<Value>(&frame) else {
                    continue;
                };
                if text(&frame, "event") == Some("connect.challenge") {
                    if let Some(challenged) = challenged.take() {
                        let _ = challenged.send(());
                    }
                    continue;
                }
                live.frame(&reading, &frame);
            }
            live.closed(&reading);
        });
        tokio::time::timeout(WITHIN, challenge)
            .await
            .map_err(|_| format!("OpenClaw's gateway on {authority} sent no connect challenge"))?
            .map_err(|_| unreached())?;
        let hello = gateway
            .ask(
                "connect",
                json!({
                    "minProtocol": PROTOCOL,
                    "maxProtocol": PROTOCOL,
                    "client": {
                        "id": "gateway-client",
                        "displayName": "Steery",
                        "version": env!("CARGO_PKG_VERSION"),
                        "platform": std::env::consts::OS,
                        "mode": "backend",
                    },
                    "role": "operator",
                    "scopes": ["operator.read", "operator.write"],
                    "auth": auth(&config),
                }),
            )
            .await
            .map_err(|why| format!("OpenClaw's gateway refused Steery's connection: {why}"));
        let subscribed = match hello {
            Ok(_) => gateway
                .ask("sessions.subscribe", json!({}))
                .await
                .map_err(|why| format!("OpenClaw's gateway did not subscribe Steery: {why}")),
            Err(why) => Err(why),
        };
        if let Err(why) = subscribed {
            gateway.ended.store(true, Ordering::Relaxed);
            return Err(why);
        }
        Ok(gateway)
    }

    /// One frame of the gateway: a reply to a request, or an event of a session.
    fn frame(&self, gateway: &Gateway, frame: &Value) {
        if text(frame, "type") == Some("res") {
            if let Some(id) = text(frame, "id")
                && let Some(reply) = lock(&gateway.replies).remove(id)
            {
                let answer = match frame.get("ok") {
                    Some(Value::Bool(true)) => {
                        Ok(frame.get("payload").cloned().unwrap_or(Value::Null))
                    }
                    _ => Err(Refused::from(frame.get("error").unwrap_or(&Value::Null))),
                };
                let _ = reply.send(answer);
            }
            return;
        }
        if text(frame, "type") != Some("event") {
            return;
        }
        let null = Value::Null;
        let payload = frame.get("payload").unwrap_or(&null);
        let Some(key) = text(payload, "sessionKey") else {
            return;
        };
        let changed = {
            let mut followed = lock(&gateway.followed);
            let Some(fold) = followed.get_mut(key) else {
                return;
            };
            fold.apply(text(frame, "event").unwrap_or_default(), payload);
            fold.changed().map(|state| (fold.external.clone(), state))
        };
        if let Some((external, state)) = changed {
            self.changed(&external, state);
        }
    }

    /// The connection closed: nothing the gateway ran is known to run.
    fn closed(&self, gateway: &Gateway) {
        gateway.ended.store(true, Ordering::Relaxed);
        lock(&gateway.replies).clear();
        let followed = std::mem::take(&mut *lock(&gateway.followed));
        for fold in followed.into_values() {
            self.changed(&fold.external, LiveState::default());
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

    /// The gateway and the key of conversation `id`, where the key writes that conversation
    /// and the gateway has its session; the driver then follows the session's events.
    async fn door(
        self: &Arc<Self>,
        root: &Path,
        id: &str,
    ) -> Result<(Arc<Gateway>, String), Error> {
        let standing = Store::new(root)
            .standing(id)?
            .ok_or_else(|| no_door("OpenClaw has no such conversation"))?;
        if !standing.newest {
            return Err(no_door(&format!(
                "OpenClaw's key {} writes a newer conversation now",
                standing.key
            )));
        }
        let gateway = self.gateway(root).await.map_err(|why| no_door(&why))?;
        let key = standing.key;
        let first = {
            let mut followed = lock(&gateway.followed);
            let first = !followed.contains_key(&key);
            if first {
                followed.insert(
                    key.clone(),
                    Fold {
                        external: id.to_owned(),
                        ..Fold::default()
                    },
                );
            }
            first
        };
        if first {
            let described = gateway
                .ask("sessions.describe", json!({"key": key}))
                .await
                .map(|answer| answer.get("session").cloned().unwrap_or(Value::Null));
            match described {
                Ok(session) if session.is_object() => {
                    if let Some(fold) = lock(&gateway.followed).get_mut(&key) {
                        fold.snapshot(&session, now());
                    }
                }
                Ok(_) => {
                    lock(&gateway.followed).remove(&key);
                    return Err(no_door(&format!("OpenClaw's gateway has no session {key}")));
                }
                Err(why) => {
                    lock(&gateway.followed).remove(&key);
                    return Err(no_door(&format!(
                        "OpenClaw's gateway did not describe the session: {why}"
                    )));
                }
            }
        }
        let state = lock(&gateway.followed)
            .get_mut(&key)
            .and_then(Fold::changed);
        if let Some(state) = state {
            self.changed(id, state);
        }
        Ok((gateway, key))
    }

    /// `held`: the gateway's session of the conversation, with the run that writes it as its
    /// token; none where the gateway does not answer for it.
    pub async fn held(self: &Arc<Self>, root: &Path, id: &str) -> Option<Holder> {
        let (gateway, key) = self.door(root, id).await.ok()?;
        Some(Holder::Person {
            door: Some(harness::Door::Picks),
            stop: HolderStop::Owner(gateway.token(&key)),
        })
    }

    /// `say`: `chat.send` on the conversation's key. Its run id is the driver's own, so the
    /// run that takes the words is known before the gateway answers.
    pub async fn say(self: &Arc<Self>, root: &Path, say: Say) -> Result<Said, Error> {
        let (gateway, key) = self.door(root, &say.external_id).await?;
        let (running, pending) = gateway.running(&key);
        if say.mode == SayMode::Immediate && (running || pending) {
            return Err(Error::refused(
                refusal::HARNESS_BUSY,
                "a run writes the session or words wait in OpenClaw",
            ));
        }
        let steer = say.mode == SayMode::Steer && running;
        let run = gateway.input();
        if let Some(fold) = lock(&gateway.followed).get_mut(&key) {
            match steer {
                true => fold.steered(run.clone(), say.id.clone()),
                false => {
                    fold.queued.insert(run.clone(), say.id.clone());
                }
            }
        }
        // `steer` goes into the active run at its next boundary; `followup` waits for the run
        // to end (`docs/concepts/queue-steering.md`). With nothing running, either starts one.
        let mode = if steer { "steer" } else { "followup" };
        let sent = gateway
            .ask(
                "chat.send",
                json!({"sessionKey": key, "message": say.words, "idempotencyKey": run,
                       "queueMode": mode}),
            )
            .await;
        match sent {
            Ok(answer) => Ok(Said {
                took: text(&answer, "runId") == Some(run.as_str()),
            }),
            Err(why) => {
                if let Some(fold) = lock(&gateway.followed).get_mut(&key) {
                    fold.forget(&run);
                }
                Err(no_answer(why))
            }
        }
    }

    /// `pick`: `sessions.patch` of the key's model or thinking level, checked against the
    /// models OpenClaw last listed and read back from the gateway's `resolved`.
    pub async fn pick(
        self: &Arc<Self>,
        root: &Path,
        listed: &[Model],
        pick: Pick,
    ) -> Result<Picked, Error> {
        crate::turn::drive::offered("OpenClaw", listed, pick.model.as_deref(), None)?;
        let (gateway, key) = self.door(root, &pick.external_id).await?;
        let mut patch = json!({"key": key});
        if let Some(model) = &pick.model {
            // The gateway names a model `<provider>/<model>`, as `models` split it.
            let provider = listed
                .iter()
                .find(|entry| &entry.model == model)
                .and_then(|entry| entry.provider.as_ref());
            patch["model"] = json!(
                provider.map_or_else(|| model.clone(), |provider| format!("{provider}/{model}"))
            );
        }
        if let Some(level) = &pick.thinking {
            patch["thinkingLevel"] = json!(level);
        }
        // The gateway refuses a patch that raced a change of the session (an abort's end, a
        // run's start) and asks for it again; once more is enough.
        let mut answer = gateway.ask("sessions.patch", patch.clone()).await;
        if let Err(Refused {
            code: Some(code), ..
        }) = &answer
            && code == MUTATION_RACED
        {
            answer = gateway.ask("sessions.patch", patch).await;
        }
        let answer = answer
            .map_err(|why| Error::refused(refusal::SELECTION_UNSUPPORTED, why.to_string()))?;
        let resolved = answer.get("resolved").unwrap_or(&Value::Null);
        let mut followed = lock(&gateway.followed);
        let fold = followed.get_mut(&key);
        let model = text(resolved, "model").map(str::to_owned);
        let thinking = text(resolved, "thinkingLevel").map(str::to_owned);
        if let Some(fold) = fold {
            if model.is_some() {
                fold.model = model.clone();
            }
            if thinking.is_some() {
                fold.thinking = thinking.clone();
            }
        }
        Ok(Picked { model, thinking })
    }

    /// `stop` through the gateway: `chat.abort` of the run that writes the key, where it is
    /// still the one `held` named, and where a say is named, the one its words went into.
    pub async fn stop(self: &Arc<Self>, root: &Path, stop: &Stop) -> Result<Stopped, Error> {
        let Holder::Person {
            stop: HolderStop::Owner(token),
            ..
        } = &stop.holder
        else {
            return Ok(not_stopped("OpenClaw's gateway does not hold it"));
        };
        let Ok((gateway, key)) = self.door(root, &stop.external_id).await else {
            return Ok(not_stopped("OpenClaw's gateway no longer answers for it"));
        };
        let (running, _) = gateway.running(&key);
        if !running {
            return Ok(not_stopped("no run writes the session in OpenClaw"));
        }
        let run = gateway.token(&key);
        if let Some(say) = &stop.say {
            let took = lock(&gateway.followed)
                .get(&key)
                .and_then(|fold| fold.turn.as_ref())
                .is_some_and(|turn| turn.says.contains(say));
            if !took {
                return Err(Error::refused(
                    refusal::OTHER_TURN,
                    "the run that writes the session did not take these words",
                ));
            }
        } else if token.is_some() && *token != run {
            return Ok(not_stopped(
                "the run it named ended; another writes the session",
            ));
        }
        let mut params = json!({"sessionKey": key});
        if let Some(run) = &run {
            params["runId"] = json!(run);
        }
        let answer = gateway.ask("chat.abort", params).await.map_err(no_answer)?;
        if answer.get("aborted") != Some(&json!(true)) {
            return Ok(not_stopped("OpenClaw's gateway aborted no run"));
        }
        // The run's lifecycle `end` with `aborted: true` is the proof.
        let ended = tokio::time::timeout(STOPPED_WITHIN, async {
            while !gateway.cancelled(&key, run.as_deref()) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .is_ok();
        Ok(Stopped {
            stopped: ended,
            proof: Some(if ended {
                "OpenClaw's gateway aborted the run and it ended aborted".into()
            } else {
                "OpenClaw's gateway took the abort, but the run has not ended yet".into()
            }),
        })
    }
}

impl Gateway {
    /// A request's payload, or the gateway's words where it refused.
    async fn ask(&self, method: &str, params: Value) -> Result<Value, Refused> {
        let id = (self.next.fetch_add(1, Ordering::Relaxed) + 1).to_string();
        let (reply, answer) = oneshot::channel();
        lock(&self.replies).insert(id.clone(), reply);
        let _ = self
            .out
            .send(json!({"type": "req", "id": id, "method": method, "params": params}).to_string());
        let answer = tokio::time::timeout(WITHIN, answer).await;
        lock(&self.replies).remove(&id);
        answer
            .map_err(|_| {
                Refused::unanswered(format!("OpenClaw's gateway did not answer `{method}`"))
            })?
            .map_err(|_| {
                Refused::unanswered("the connection to OpenClaw's gateway closed".into())
            })?
    }

    /// A fresh run id, sent as the `idempotencyKey` that the gateway names the run by.
    fn input(&self) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!(
            "steery{:x}{:x}{:x}",
            std::process::id(),
            nanos,
            self.next.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Whether a run writes session `key`, and whether words wait for their own run.
    fn running(&self, key: &str) -> (bool, bool) {
        lock(&self.followed)
            .get(key)
            .map_or((false, false), |fold| {
                (fold.turn.is_some(), !fold.queued.is_empty())
            })
    }

    /// Whether run `run` of session `key` ended aborted; with no run named, whether the turn
    /// that ran ended aborted.
    fn cancelled(&self, key: &str, run: Option<&str>) -> bool {
        lock(&self.followed).get(key).is_some_and(|fold| {
            fold.turn.is_none()
                && fold.cancelled.is_some()
                && run.is_none_or(|run| fold.cancelled.as_deref() == Some(run))
        })
    }

    /// The token of the turn that runs: its run id.
    fn token(&self, key: &str) -> Option<String> {
        lock(&self.followed).get(key)?.turn.as_ref()?.run.clone()
    }
}

/// The run that writes the session.
struct Turn {
    /// The gateway's run id; none where the gateway says a run is active but not which.
    run: Option<String>,
    began_at: u64,
    /// The say ids it took.
    says: Vec<String>,
}

/// What the gateway's events said of one session.
#[derive(Default)]
struct Fold {
    /// The conversation's id, as Steery knows it: its first window's.
    external: String,
    turn: Option<Turn>,
    /// Words sent to wait for their own run, by that run's id, until it begins.
    queued: HashMap<String, Option<String>>,
    /// Words steered into a run, by their own run id: should the run end before they go in,
    /// the gateway runs them under that id.
    steered: HashMap<String, Option<String>>,
    /// The run id of the last run that ended aborted.
    cancelled: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
    context_left: Option<f64>,
    compacting: bool,
    calls: Vec<LiveCall>,
    ended: std::collections::VecDeque<LiveEnded>,
    settled: std::collections::VecDeque<LiveSettled>,
    /// Exec approvals open in front of the person, by the call they gate.
    dialogs: Vec<(String, Dialog)>,
    /// The state last sent.
    sent: Option<LiveState>,
}

impl Fold {
    /// The session as `sessions.describe` answers it: its model, its context and its runs.
    fn snapshot(&mut self, session: &Value, at: u64) {
        self.session(session);
        if self.model.is_none() {
            self.model = text(session, "model").map(str::to_owned);
        }
        if self.thinking.is_none() {
            self.thinking = text(session, "thinkingLevel")
                .or_else(|| text(session, "thinkingDefault"))
                .map(str::to_owned);
        }
        if self.turn.is_some() || session.get("hasActiveRun") != Some(&json!(true)) {
            return;
        }
        let active: Vec<&str> = session
            .get("activeRunIds")
            .and_then(Value::as_array)
            .map(|runs| runs.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let last = text(session, "lastRunId");
        let run = last
            .filter(|last| active.contains(last))
            .or_else(|| (active.len() == 1).then(|| active[0]));
        self.begin(run.map(str::to_owned), at);
    }

    /// A session row (`sessions.describe`, `sessions.changed`): its context and thinking.
    fn session(&mut self, session: &Value) {
        let used = session.get("totalTokens").and_then(Value::as_f64);
        let max = session.get("contextTokens").and_then(Value::as_f64);
        if let (Some(used), Some(max)) = (used, max)
            && max > 0.0
            && session.get("totalTokensFresh") != Some(&json!(false))
        {
            self.context_left = Some((1.0 - used / max).clamp(0.0, 1.0));
        }
        if let Some(level) = text(session, "thinkingLevel") {
            self.thinking = Some(level.to_owned());
        }
    }

    /// Words steered into the run that writes the session.
    fn steered(&mut self, run: String, say: Option<String>) {
        if let (Some(say), Some(turn)) = (&say, &mut self.turn) {
            turn.says.push(say.clone());
        }
        self.steered.insert(run, say);
    }

    /// Words the gateway refused: no run takes them.
    fn forget(&mut self, run: &str) {
        self.queued.remove(run);
        if let Some(Some(say)) = self.steered.remove(run)
            && let Some(turn) = &mut self.turn
        {
            turn.says.retain(|taken| *taken != say);
        }
    }

    fn apply(&mut self, event: &str, payload: &Value) {
        let null = Value::Null;
        let data = payload.get("data").unwrap_or(&null);
        let at = payload
            .get("ts")
            .and_then(Value::as_u64)
            .unwrap_or_else(now);
        let run = text(payload, "runId");
        match (event, text(payload, "stream")) {
            ("sessions.changed", _) => self.session(payload),
            // A run's last chat frame. Words steered in, or queued and merged, end there with
            // no run of their own.
            ("chat", _)
                if matches!(text(payload, "state"), Some("final" | "error" | "aborted")) =>
            {
                if let Some(run) = run
                    && self.turn.as_ref().and_then(|turn| turn.run.as_deref()) != Some(run)
                {
                    self.queued.remove(run);
                    self.steered.remove(run);
                }
            }
            ("agent", Some("lifecycle")) => self.lifecycle(run, data, at),
            ("agent" | "session.tool", Some("tool")) => self.tool(data, at),
            ("agent", Some("approval")) => self.approval(data, at),
            ("agent", Some("compaction")) => match text(data, "phase") {
                Some("start") => self.compacting = true,
                Some("end") => self.compacting = false,
                _ => {}
            },
            _ => {}
        }
    }

    fn lifecycle(&mut self, run: Option<&str>, data: &Value, at: u64) {
        match text(data, "phase") {
            Some("start") => {
                let began = data.get("startedAt").and_then(Value::as_u64).unwrap_or(at);
                if self
                    .turn
                    .as_ref()
                    .is_some_and(|turn| turn.run.is_some() && turn.run.as_deref() != run)
                {
                    self.close(began, None);
                }
                match &mut self.turn {
                    // The run the snapshot could not name.
                    Some(turn) if turn.run.is_none() => turn.run = run.map(str::to_owned),
                    _ => self.begin(run.map(str::to_owned), began),
                }
            }
            Some("model") => {
                if let Some(model) = text(data, "model") {
                    self.model = Some(model.to_owned());
                }
            }
            Some("end" | "error") if self.runs(run) => {
                let ended = data.get("endedAt").and_then(Value::as_u64).unwrap_or(at);
                let aborted = data.get("aborted") == Some(&json!(true))
                    || text(data, "status") == Some("cancelled");
                if aborted {
                    self.cancelled = run.map(str::to_owned);
                }
                let error = if text(data, "phase") == Some("error") {
                    Some(
                        text(data, "error")
                            .or_else(|| text(data, "message"))
                            .unwrap_or("the run failed")
                            .to_owned(),
                    )
                } else if aborted {
                    Some(text(data, "stopReason").map_or_else(
                        || "aborted".to_owned(),
                        |reason| format!("aborted ({reason})"),
                    ))
                } else {
                    None
                };
                self.close(ended, error);
            }
            _ => {}
        }
    }

    /// Whether run `run` is the one that writes the session.
    fn runs(&self, run: Option<&str>) -> bool {
        self.turn
            .as_ref()
            .is_some_and(|turn| turn.run.is_none() || turn.run.as_deref() == run)
    }

    fn tool(&mut self, data: &Value, at: u64) {
        let Some(id) = text(data, "toolCallId") else {
            return;
        };
        match text(data, "phase") {
            Some("start") => {
                if self.calls.iter().any(|call| call.id == id) {
                    return;
                }
                let part = json!({"id": id, "name": text(data, "name").unwrap_or("tool"),
                                  "arguments": data.get("args").cloned().unwrap_or(Value::Null)});
                if let Some(call) = super::events::call(&part, None, &[]) {
                    self.calls.push(LiveCall {
                        id: call.id,
                        tool: call.tool,
                        since: at,
                        children: Vec::new(),
                        jobs: Vec::new(),
                    });
                }
            }
            Some("result") => {
                self.dialogs.retain(|(open, _)| open != id);
                if let Some(index) = self.calls.iter().position(|call| call.id == id) {
                    let call = self.calls.remove(index);
                    self.end(call.id, at);
                }
            }
            _ => {}
        }
    }

    /// An exec approval the gateway waits on (`stream: "approval"`), and its resolution.
    fn approval(&mut self, data: &Value, at: u64) {
        let Some(id) = text(data, "toolCallId").or_else(|| text(data, "itemId")) else {
            return;
        };
        match (text(data, "phase"), text(data, "status")) {
            (Some("requested"), Some("pending")) => {
                if self.dialogs.iter().any(|(open, _)| open == id) {
                    return;
                }
                let title = match text(data, "command").filter(|command| !command.is_empty()) {
                    Some(command) => format!("Allow {command}?"),
                    None => text(data, "title")
                        .unwrap_or("Allow this command?")
                        .to_owned(),
                };
                self.dialogs.push((
                    id.to_owned(),
                    Dialog {
                        kind: DialogKind::Confirm,
                        title: cut(&title, TITLE_AT_MOST),
                        since: at,
                    },
                ));
            }
            (Some("resolved"), _) => self.dialogs.retain(|(open, _)| open != id),
            _ => {}
        }
    }

    fn begin(&mut self, run: Option<String>, at: u64) {
        if self.turn.is_some() {
            return;
        }
        let says = run
            .as_deref()
            .and_then(|run| self.queued.remove(run).or_else(|| self.steered.remove(run)))
            .flatten()
            .into_iter()
            .collect();
        self.turn = Some(Turn {
            run,
            began_at: at,
            says,
        });
    }

    /// The run that wrote the session ended at `at`: a turn Steery's words began settles.
    fn close(&mut self, at: u64, error: Option<String>) {
        for call in std::mem::take(&mut self.calls) {
            self.end(call.id, at);
        }
        self.dialogs.clear();
        self.compacting = false;
        let Some(turn) = self.turn.take() else {
            return;
        };
        if turn.says.is_empty() {
            return;
        }
        self.settled.push_back(LiveSettled {
            began_at: turn.began_at,
            at,
            says: turn.says,
            failed: error.is_some(),
            error: error.map(|error| cut(&error, ERROR_AT_MOST)),
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
        let pending = !self.queued.is_empty();
        LiveState {
            turn: self.turn.as_ref().map(|turn| LiveTurn {
                began_at: turn.began_at,
                says: turn.says.clone(),
            }),
            idle: Some(self.turn.is_none() && !pending && !self.compacting),
            pending: Some(pending),
            model: self.model.clone(),
            thinking: self.thinking.clone(),
            context_left: self.context_left,
            compacting: self.compacting,
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

/// The gateway's port: `OPENCLAW_GATEWAY_PORT`, else `gateway.port`, else the default
/// (`resolveGatewayPort`).
fn port(config: &Value) -> u16 {
    std::env::var("OPENCLAW_GATEWAY_PORT")
        .ok()
        .and_then(|port| port.trim().parse().ok())
        .or_else(|| {
            config
                .pointer("/gateway/port")
                .and_then(Value::as_u64)
                .and_then(|port| u16::try_from(port).ok())
        })
        .filter(|port| *port > 0)
        .unwrap_or(PORT)
}

/// The gateway's shared secret as `connect.auth`: `gateway.auth.password` where its mode is
/// `password`, else `gateway.auth.token`; each with its environment variable where the file
/// holds no plain string (`resolveGatewayAuthToken`).
fn auth(config: &Value) -> Value {
    let auth = config.pointer("/gateway/auth").unwrap_or(&Value::Null);
    let secret = |field: &str, variable: &str| {
        text(auth, field)
            .map(str::to_owned)
            .or_else(|| std::env::var(variable).ok())
            .filter(|secret| !secret.trim().is_empty())
    };
    match text(auth, "mode") {
        Some("none") => json!({}),
        Some("password") => secret("password", "OPENCLAW_GATEWAY_PASSWORD")
            .map_or_else(|| json!({}), |password| json!({"password": password})),
        _ => secret("token", "OPENCLAW_GATEWAY_TOKEN")
            .map_or_else(|| json!({}), |token| json!({"token": token})),
    }
}

fn no_door(why: &str) -> Error {
    Error::refused(refusal::NO_DOOR, why.to_owned())
}

fn not_stopped(why: &str) -> Stopped {
    Stopped {
        stopped: false,
        proof: Some(why.to_owned()),
    }
}

fn no_answer(why: impl std::fmt::Display) -> Error {
    Error::refused(refusal::NO_ANSWER, why.to_string())
}

/// The code of the gateway's refusal of a mutation that raced a change of the session
/// (`sessionMutationTargetChanged`, `session-sharing-source`).
const MUTATION_RACED: &str = "SESSION_MUTATION_AUTHORIZATION_CHANGED";

/// The gateway's refusal of a request, or a request it never answered.
struct Refused {
    /// Its message, with its code.
    why: String,
    /// The code of its `details`, which names the refusal exactly.
    code: Option<String>,
}

impl Refused {
    fn unanswered(why: String) -> Self {
        Self { why, code: None }
    }
}

impl From<&Value> for Refused {
    fn from(error: &Value) -> Self {
        let message = text(error, "message").unwrap_or("OpenClaw's gateway refused the request");
        Self {
            why: match text(error, "code") {
                Some(code) => format!("{message} ({code})"),
                None => message.to_owned(),
            },
            code: error
                .pointer("/details/code")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.why)
    }
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn cut(text: &str, at_most: usize) -> String {
    text.chars().take(at_most).collect()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
