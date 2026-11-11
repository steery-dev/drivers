//! Hermes's own door (PROTOCOL.md §4.3): its gateway owner. One owner process per Hermes home
//! runs every turn of every client (a TUI window, Desktop, `hermes acp`), and serves a JSON-RPC
//! WebSocket that any client of the same OS user may join. The driver finds the owner through
//! its control socket, never starts one, and follows a conversation that the owner lists as
//! live: the owner takes words (`prompt.submit`, `session.steer`), a model or thinking level
//! (`session.mutate`) and a stop (`session.interrupt`) from any client, and its event stream
//! says what runs. The driver sends that as `session/changed`.
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use super::calls;
use super::store::Home;
use crate::turn::drive::now;

/// How long the owner has to answer a request.
const WITHIN: Duration = Duration::from_secs(10);
/// How long a stop waits for the turn to end: inside the runtime's 2.8 s for `stop`
/// (PROTOCOL.md §7).
const STOPPED_WITHIN: Duration = Duration::from_millis(2500);
/// How many ended calls and settled turns a state carries.
const ENDED: usize = 64;
const SETTLED: usize = 16;
/// The longest error and dialog title a state carries.
const ERROR_AT_MOST: usize = 600;
const TITLE_AT_MOST: usize = 200;
/// The WebSocket subprotocol of the owner, and the prefix of the one that carries a ticket
/// (`hermes_cli/gateway_client.py:39-45`).
const PROTOCOL: &str = "hermes-gateway-v1";
const TICKET: &str = "hermes-gateway-ticket.";
/// The owner's control protocol and runtime protocol this driver speaks.
const CONTROL: u64 = 1;
/// Why a conversation has no door.
const NO_OWNER: &str = "no Hermes gateway owner answers for this Hermes home: a window that \
     runs Hermes without one runs its turns itself";
const NOT_SHOWN: &str = "Hermes's gateway lists no live session of this conversation \
     (`session.list` scope `live`): no window has it open";

/// The owners the driver speaks to, one per Hermes home (a profile has its own), while each
/// connection lives.
pub struct Live {
    notifier: Notifier,
    owners: tokio::sync::Mutex<HashMap<PathBuf, Arc<Owner>>>,
}

/// One connection to Hermes's gateway owner, and what its events said of each conversation
/// followed.
struct Owner {
    /// The owner's instance, from `identify`.
    instance: String,
    out: mpsc::UnboundedSender<String>,
    next: AtomicU64,
    ended: AtomicBool,
    /// Replies awaited, by request id.
    replies: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    /// The conversations followed, by the session id the owner names them by.
    followed: Mutex<HashMap<String, Fold>>,
}

impl Live {
    pub fn new(notifier: Notifier) -> Self {
        Self {
            notifier,
            owners: tokio::sync::Mutex::default(),
        }
    }

    /// The owner's connection for Hermes home `home`: the one open, else a new one where an
    /// owner answers on the home's control socket, or on that of Hermes's own home `root`.
    async fn owner(self: &Arc<Self>, root: &Path, home: &Path) -> Option<Arc<Owner>> {
        let home = std::fs::canonicalize(home).ok()?;
        let mut held = self.owners.lock().await;
        if let Some(owner) = held
            .get(&home)
            .filter(|owner| !owner.ended.load(Ordering::Relaxed))
        {
            return Some(Arc::clone(owner));
        }
        let mut sockets = vec![home.clone()];
        if let Ok(root) = std::fs::canonicalize(root)
            && root != home
        {
            sockets.push(root);
        }
        for at in sockets {
            if let Some(owner) = self.connect(&at, &home).await {
                held.insert(home, Arc::clone(&owner));
                return Some(owner);
            }
        }
        None
    }

    /// Dial the owner whose control socket is in `at` for the profile of home `home`
    /// (`hermes_cli/gateway_client.py:245-320`): `identify`, a ticket, then the WebSocket.
    async fn connect(self: &Arc<Self>, at: &Path, home: &Path) -> Option<Arc<Owner>> {
        let socket = socket(at)?;
        let identity = control(&socket, "identify", json!({})).await?;
        if text(&identity, "state") != Some("ready")
            || identity.get("runtime_protocol").and_then(Value::as_u64) != Some(CONTROL)
        {
            return None;
        }
        let instance = text(&identity, "instance_id")?.to_owned();
        let profile = identity
            .get("served_profiles")?
            .as_array()?
            .iter()
            .find(|profile| {
                text(profile, "home")
                    .and_then(|served| std::fs::canonicalize(served).ok())
                    .is_some_and(|served| served == home)
            })
            .and_then(|profile| text(profile, "profile_id"))?
            .to_owned();
        let ticket = control(
            &socket,
            "session-ticket",
            json!({"profile_id": profile, "instance_id": instance, "purpose": "interactive"}),
        )
        .await?;
        if text(&ticket, "instance_id") != Some(&instance)
            || text(&ticket, "profile_id") != Some(&profile)
        {
            return None;
        }
        let ticket = text(&ticket, "ticket")?;
        // A loopback origin over plain HTTP; this driver has no TLS.
        let authority = text(&identity, "api_origin")?
            .strip_prefix("http://")?
            .trim_end_matches('/');
        let mut request = format!("ws://{authority}/api/ws")
            .into_client_request()
            .ok()?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(&format!("{PROTOCOL}, {TICKET}{ticket}")).ok()?,
        );
        let stream = tokio::time::timeout(WITHIN, tokio::net::TcpStream::connect(authority))
            .await
            .ok()?
            .ok()?;
        let (socket, response) =
            tokio::time::timeout(WITHIN, tokio_tungstenite::client_async(request, stream))
                .await
                .ok()?
                .ok()?;
        if response
            .headers()
            .get("Sec-WebSocket-Protocol")
            .and_then(|chosen| chosen.to_str().ok())
            != Some(PROTOCOL)
        {
            return None;
        }
        let (mut sink, mut source) = socket.split();
        let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
        let owner = Arc::new(Owner {
            instance,
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
        let live = Arc::clone(self);
        let reading = Arc::clone(&owner);
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
        Some(owner)
    }

    /// One frame of the owner: a reply to its request, or an event of a session.
    fn frame(&self, owner: &Owner, frame: Value) {
        if let Some(id) = frame.get("id").and_then(Value::as_u64) {
            if let Some(reply) = lock(&owner.replies).remove(&id) {
                let answer = match frame.get("error") {
                    Some(error) => Err(failure(error)),
                    None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = reply.send(answer);
            }
            return;
        }
        if text(&frame, "method") != Some("event") {
            return;
        }
        let Some(event) = frame.get("params") else {
            return;
        };
        let null = Value::Null;
        let payload = event.get("payload").unwrap_or(&null);
        let Some(session) = text(event, "session_id").or_else(|| text(payload, "session_id"))
        else {
            return;
        };
        let changed = {
            let mut followed = lock(&owner.followed);
            let Some(fold) = followed.get_mut(session) else {
                return;
            };
            if !fold.fresh(event) {
                return;
            }
            fold.apply(event, payload, now());
            fold.changed().map(|state| (fold.external.clone(), state))
        };
        if let Some((external, state)) = changed {
            self.changed(&external, state);
        }
    }

    /// The connection closed: nothing the owner ran is known to run.
    fn closed(&self, owner: &Owner) {
        owner.ended.store(true, Ordering::Relaxed);
        lock(&owner.replies).clear();
        let followed = std::mem::take(&mut *lock(&owner.followed));
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

    /// The owner of conversation `id` and the session it runs the conversation as, where the
    /// owner lists that session as live; the driver then follows the session's events.
    async fn door(self: &Arc<Self>, root: &Path, id: &str) -> Result<(Arc<Owner>, String), Error> {
        let standing = Home::new(root)
            .standing(id)?
            .ok_or_else(|| no_door("Hermes has no such conversation"))?;
        let owner = self
            .owner(root, &standing.home)
            .await
            .ok_or_else(|| no_door(NO_OWNER))?;
        let session = standing.tip;
        let live = owner
            .ask("session.list", json!({"scope": "live", "limit": 200}))
            .await
            .map_err(|why| {
                no_door(&format!(
                    "Hermes's gateway did not list its sessions: {why}"
                ))
            })?;
        let shown = live
            .get("sessions")
            .and_then(Value::as_array)
            .is_some_and(|sessions| {
                sessions
                    .iter()
                    .any(|listed| text(listed, "session_id") == Some(session.as_str()))
            });
        if !shown {
            return Err(no_door(NOT_SHOWN));
        }
        let first = {
            let mut followed = lock(&owner.followed);
            let first = !followed.contains_key(&session);
            if first {
                followed.insert(
                    session.clone(),
                    Fold {
                        external: standing.external_id.clone(),
                        ..Fold::default()
                    },
                );
            }
            first
        };
        if first {
            // Attaches this connection beside the window's; its events then stream.
            match owner
                .ask("session.resume", json!({"session_id": session}))
                .await
            {
                Ok(snapshot) => {
                    if let Some(fold) = lock(&owner.followed).get_mut(&session) {
                        fold.snapshot(&snapshot, now());
                    }
                }
                Err(why) => {
                    lock(&owner.followed).remove(&session);
                    return Err(no_door(&format!(
                        "Hermes's gateway did not attach to the session: {why}"
                    )));
                }
            }
        }
        let state = lock(&owner.followed)
            .get_mut(&session)
            .and_then(Fold::changed);
        if let Some(state) = state {
            self.changed(&standing.external_id, state);
        }
        Ok((owner, session))
    }

    /// `held`: a person's window on Hermes's gateway, with the turn that runs as its token;
    /// none where the owner lists no live session of the conversation.
    pub async fn held(self: &Arc<Self>, root: &Path, id: &str) -> Option<Holder> {
        let (owner, session) = self.door(root, id).await.ok()?;
        Some(Holder::Person {
            door: Some(harness::Door::Picks),
            stop: HolderStop::Owner(owner.token(&session)),
        })
    }

    /// `say`: the words into the conversation through the owner, its id named in the turn
    /// that takes them.
    pub async fn say(self: &Arc<Self>, root: &Path, say: Say) -> Result<Said, Error> {
        let (owner, session) = self.door(root, &say.external_id).await?;
        let (running, pending, generation) = owner.running(&session);
        match say.mode {
            SayMode::Immediate if running || pending => {
                return Err(Error::refused(
                    refusal::HARNESS_BUSY,
                    "a turn runs or words wait in Hermes",
                ));
            }
            SayMode::Steer if running => {
                let steered = owner
                    .ask(
                        "session.steer",
                        json!({"session_id": session, "text": say.words,
                               "execution_generation": generation}),
                    )
                    .await;
                match steered {
                    Ok(answer)
                        if matches!(text(&answer, "status"), Some("queued" | "redirected")) =>
                    {
                        if let Some(fold) = lock(&owner.followed).get_mut(&session) {
                            fold.steered(say.id.clone(), generation);
                        }
                        return Ok(Said { took: true });
                    }
                    // The turn ended before the words reached it: nothing live took them, and
                    // Hermes says to admit them as the next prompt
                    // (`gateway/session_busy_controls.py:204-211`).
                    Ok(answer) if answer.get("delivered") == Some(&json!(false)) => {}
                    Err(why) if why.contains("stale_generation") => {}
                    Ok(_) => return Ok(Said { took: false }),
                    Err(why) => return Err(no_answer(why)),
                }
            }
            // With no turn running, words to steer start one, as a queued prompt does.
            _ => {}
        }
        let admitted = owner
            .ask(
                "prompt.submit",
                json!({"session_id": session, "input_id": owner.input(), "text": say.words}),
            )
            .await
            .map_err(no_answer)?;
        let Some(admission) = text(&admitted, "admission_id") else {
            return Ok(Said { took: false });
        };
        if let Some(fold) = lock(&owner.followed).get_mut(&session) {
            fold.admitted(say.id.clone(), admission);
        }
        Ok(Said { took: true })
    }

    /// `pick`: the model or thinking level of the conversation's next request, through the
    /// owner, checked against the models Hermes last listed and read back from its answer.
    pub async fn pick(
        self: &Arc<Self>,
        root: &Path,
        listed: &[Model],
        pick: Pick,
    ) -> Result<Picked, Error> {
        crate::turn::drive::offered(
            "Hermes",
            listed,
            pick.model.as_deref(),
            pick.thinking.as_deref(),
        )?;
        let (owner, session) = self.door(root, &pick.external_id).await?;
        if let Some(choice) = &pick.model {
            // Hermes lists a model as `provider:model` (`acp_adapter/model_catalog.py:146-152`).
            let payload = match choice.split_once(':') {
                Some((provider, model)) => json!({"model": model, "provider": provider}),
                None => json!({"model": choice}),
            };
            let answer = owner.mutate(&session, "model", payload).await?;
            let model = text(&answer, "model").map(|model| {
                text(&answer, "provider")
                    .or_else(|| choice.split_once(':').map(|(provider, _)| provider))
                    .filter(|provider| !provider.is_empty())
                    .map_or_else(
                        || model.to_owned(),
                        |provider| format!("{provider}:{model}"),
                    )
            });
            if let Some(fold) = lock(&owner.followed).get_mut(&session) {
                fold.model = model.or_else(|| fold.model.take());
            }
        }
        if let Some(level) = &pick.thinking {
            let answer = owner
                .mutate(&session, "reasoning", json!({"effort": level}))
                .await?;
            if let Some(fold) = lock(&owner.followed).get_mut(&session)
                && let Some(effort) = text(&answer, "effort")
            {
                fold.thinking = Some(effort.to_owned());
            }
        }
        let followed = lock(&owner.followed);
        let fold = followed.get(&session);
        Ok(Picked {
            model: fold.and_then(|fold| fold.model.clone()),
            thinking: fold.and_then(|fold| fold.thinking.clone()),
        })
    }

    /// `stop` of a window's owner: the owner interrupts the turn that runs, where it is still
    /// the one `held` named, and where a say is named, the one its words went into.
    pub async fn stop(self: &Arc<Self>, root: &Path, stop: &Stop) -> Result<Stopped, Error> {
        let Holder::Person {
            stop: HolderStop::Owner(token),
            ..
        } = &stop.holder
        else {
            return Ok(not_stopped("no window on Hermes's gateway holds it"));
        };
        let id = &stop.external_id;
        let Ok((owner, session)) = self.door(root, id).await else {
            return Ok(not_stopped("Hermes's gateway no longer lists it as live"));
        };
        let (running, _, generation) = owner.running(&session);
        if !running {
            return Ok(not_stopped("no turn runs in Hermes"));
        }
        if let Some(say) = &stop.say {
            let took = lock(&owner.followed)
                .get(&session)
                .and_then(|fold| fold.turn.as_ref())
                .is_some_and(|turn| turn.says.contains(say));
            if !took {
                return Err(Error::refused(
                    refusal::OTHER_TURN,
                    "the turn that runs did not take these words",
                ));
            }
        } else if token.is_some() && *token != owner.token(&session) {
            return Ok(not_stopped("the turn it named ended; another runs"));
        }
        let answer = owner
            .ask(
                "session.interrupt",
                json!({"session_id": session, "execution_generation": generation}),
            )
            .await
            .map_err(no_answer)?;
        if text(&answer, "status") == Some("not_interrupted") {
            return Ok(not_stopped("Hermes's gateway did not interrupt the turn"));
        }
        // The turn's `message.complete` with outcome `cancelled` is the proof.
        let ended = tokio::time::timeout(STOPPED_WITHIN, async {
            while !owner.cancelled(&session, generation) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .is_ok();
        Ok(Stopped {
            stopped: ended,
            proof: Some(if ended {
                "Hermes's gateway interrupted the turn and it ended cancelled".into()
            } else {
                "Hermes's gateway took the interrupt, but the turn has not ended yet".into()
            }),
        })
    }
}

impl Owner {
    /// Send a request; its answer comes on the receiver.
    fn send(&self, method: &str, params: Value) -> (u64, oneshot::Receiver<Result<Value, String>>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (reply, answer) = oneshot::channel();
        lock(&self.replies).insert(id, reply);
        let _ = self.out.send(
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        (id, answer)
    }

    /// A request's result, or the owner's words where it refused.
    async fn ask(&self, method: &str, params: Value) -> Result<Value, String> {
        let (id, answer) = self.send(method, params);
        let answer = tokio::time::timeout(WITHIN, answer).await;
        lock(&self.replies).remove(&id);
        answer
            .map_err(|_| format!("Hermes's gateway did not answer `{method}`"))?
            .map_err(|_| "the connection to Hermes's gateway closed".to_owned())?
    }

    /// `session.mutate` at the revision and generation the owner last said; a stale one is
    /// read again with `session.resume` and tried once more.
    async fn mutate(&self, session: &str, operation: &str, payload: Value) -> Result<Value, Error> {
        let mut retried = false;
        loop {
            let (revision, generation) = lock(&self.followed)
                .get(session)
                .map_or((0, 0), |fold| (fold.revision, fold.generation));
            let answer = self
                .ask(
                    "session.mutate",
                    json!({"session_id": session, "request_id": self.input(),
                           "operation": operation, "expected_revision": revision,
                           "expected_generation": generation, "payload": payload}),
                )
                .await;
            match answer {
                Err(why) if why.contains("stale") && !retried => {
                    retried = true;
                    let snapshot = self
                        .ask("session.resume", json!({"session_id": session}))
                        .await
                        .map_err(no_answer)?;
                    if let Some(fold) = lock(&self.followed).get_mut(session) {
                        fold.positions(&snapshot);
                    }
                }
                // Hermes resolves no provider for the model (`gateway/session_mutation_model.py:72`).
                Err(why) if why.contains("model_resolution_failed") => {
                    return Err(Error::refused(
                        refusal::MODEL_MISSING,
                        format!("Hermes resolves no provider for the model: {why}"),
                    ));
                }
                Err(why) => return Err(Error::refused(refusal::SELECTION_UNSUPPORTED, why)),
                // A guarded model (cost or capability warnings) needs the person's own yes in
                // Hermes (`gateway/session_mutation_model.py:375`).
                Ok(answer) if text(&answer, "status") == Some("confirmation_required") => {
                    let why =
                        text(&answer, "confirm_message").unwrap_or("it warns about the model");
                    return Err(Error::refused(
                        refusal::SELECTION_UNSUPPORTED,
                        format!(
                            "Hermes asks its person to confirm this model: {}",
                            cut(why, ERROR_AT_MOST)
                        ),
                    ));
                }
                Ok(answer) => return Ok(answer),
            }
        }
    }

    /// A fresh id for a prompt or a mutation.
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

    /// Whether a turn of session `id` runs, whether words wait, and the owner's execution
    /// generation of the session.
    fn running(&self, id: &str) -> (bool, bool, u64) {
        lock(&self.followed)
            .get(id)
            .map_or((false, false, 0), |fold| {
                (
                    fold.turn.is_some(),
                    fold.pending == Some(true),
                    fold.turn
                        .as_ref()
                        .and_then(|turn| turn.generation)
                        .unwrap_or(fold.generation),
                )
            })
    }

    /// Whether the turn of `generation` ended cancelled.
    fn cancelled(&self, id: &str, generation: u64) -> bool {
        lock(&self.followed)
            .get(id)
            .is_some_and(|fold| fold.cancelled == Some(generation))
    }

    /// The token of the turn that runs: the owner and the turn's execution generation.
    fn token(&self, id: &str) -> Option<String> {
        let followed = lock(&self.followed);
        let fold = followed.get(id)?;
        let turn = fold.turn.as_ref()?;
        Some(format!(
            "{}:{}",
            self.instance,
            turn.generation.unwrap_or(fold.generation)
        ))
    }
}

/// The turn that runs.
struct Turn {
    began_at: u64,
    /// The owner's execution generation of the turn.
    generation: Option<u64>,
    /// The admission whose prompt began it.
    admission: Option<String>,
    /// The say ids it took.
    says: Vec<String>,
}

/// What the owner's events said of one session.
#[derive(Default)]
struct Fold {
    /// The conversation's id, as Steery knows it: the first of its chain.
    external: String,
    turn: Option<Turn>,
    /// Say ids by the admission of their words, until a turn takes them.
    admitted: HashMap<String, String>,
    pending: Option<bool>,
    revision: u64,
    generation: u64,
    /// The generation of the last turn that ended cancelled.
    cancelled: Option<u64>,
    model: Option<String>,
    thinking: Option<String>,
    context_left: Option<f64>,
    /// The owner's `status.update` says `compacting` (`gateway/run_turn_progress.py:1116-1119`)
    /// until its next status, `compacted` among them.
    compacting: bool,
    calls: Vec<LiveCall>,
    ended: VecDeque<LiveEnded>,
    settled: VecDeque<LiveSettled>,
    /// Approvals and clarify questions open in front of the person, by prompt id.
    dialogs: Vec<(String, Dialog)>,
    /// The newest event: its replay epoch and sequence.
    seen: Option<(String, u64)>,
    /// The state last sent.
    sent: Option<LiveState>,
}

impl Fold {
    /// Whether an event is newer than the last one applied: a resume sends a session's frames
    /// again.
    fn fresh(&mut self, event: &Value) -> bool {
        let (Some(epoch), Some(seq)) = (
            text(event, "replay_epoch"),
            event.get("seq").and_then(Value::as_u64),
        ) else {
            return true;
        };
        if let Some((seen, last)) = &self.seen
            && seen == epoch
            && seq <= *last
        {
            return false;
        }
        self.seen = Some((epoch.to_owned(), seq));
        true
    }

    /// The session as `session.resume` answers it: whether it runs, its admissions, its
    /// model and its open prompts.
    fn snapshot(&mut self, snapshot: &Value, at: u64) {
        self.positions(snapshot);
        // Events the answer already holds come again on the stream; only newer ones apply.
        if let (Some(epoch), Some(last)) = (
            text(snapshot, "replay_epoch"),
            snapshot.get("last_sequence").and_then(Value::as_u64),
        ) {
            self.seen = Some((epoch.to_owned(), last));
        }
        if let Some(pending) = snapshot.get("pending") {
            self.pending(pending);
        }
        if let Some(info) = snapshot.get("info") {
            self.info(info);
        }
        if let Some(prompts) = snapshot.get("prompts").and_then(Value::as_array) {
            for prompt in prompts {
                self.prompt(prompt, at);
            }
        }
        match snapshot.get("running").and_then(Value::as_bool) {
            Some(true) => {
                let admission = started(snapshot.get("pending"));
                self.begin(at, Some(self.generation), admission);
            }
            Some(false) => self.close(at, None),
            None => {}
        }
    }

    /// The revision and execution generation the owner said last.
    fn positions(&mut self, said: &Value) {
        if let Some(revision) = said.get("revision").and_then(Value::as_u64) {
            self.revision = revision;
        }
        if let Some(generation) = said.get("execution_generation").and_then(Value::as_u64) {
            self.generation = generation;
        }
    }

    /// Admissions not yet ended: words wait where one is still queued.
    fn pending(&mut self, pending: &Value) {
        let Some(admissions) = pending.as_array() else {
            return;
        };
        self.pending = Some(
            admissions
                .iter()
                .any(|admission| text(admission, "status") == Some("queued")),
        );
    }

    /// The model, provider and reasoning effort the owner says the session runs on.
    fn info(&mut self, info: &Value) {
        if let Some(model) = text(info, "model").filter(|model| !model.is_empty()) {
            // A payload without a provider names the same model the last one did.
            let known = self
                .model
                .as_deref()
                .and_then(|shown| shown.split_once(':'))
                .filter(|(_, shown)| *shown == model)
                .map(|(provider, _)| provider.to_owned());
            let provider = text(info, "provider")
                .filter(|provider| !provider.is_empty())
                .map(str::to_owned)
                .or(known);
            self.model = Some(provider.map_or_else(
                || model.to_owned(),
                |provider| format!("{provider}:{model}"),
            ));
        }
        if let Some(effort) = text(info, "reasoning_effort").filter(|effort| !effort.is_empty()) {
            self.thinking = Some(effort.to_owned());
        }
    }

    /// An approval or clarify question the owner opened (`gateway/session_pending_controls.py:18-51`).
    fn prompt(&mut self, prompt: &Value, at: u64) {
        let Some(id) = text(prompt, "prompt_id") else {
            return;
        };
        if self.dialogs.iter().any(|(open, _)| open == id) {
            return;
        }
        let dialog = match text(prompt, "kind") {
            Some("approval") => {
                let command = text(prompt, "command")
                    .filter(|command| !command.is_empty())
                    .or_else(|| text(prompt, "description"))
                    .unwrap_or("this command");
                Dialog {
                    kind: DialogKind::Confirm,
                    title: cut(&format!("Allow {command}?"), TITLE_AT_MOST),
                    since: at,
                }
            }
            Some("clarify") => {
                let choices = prompt
                    .get("choices")
                    .and_then(Value::as_array)
                    .is_some_and(|choices| !choices.is_empty());
                Dialog {
                    kind: if choices {
                        DialogKind::Select
                    } else {
                        DialogKind::Input
                    },
                    title: cut(
                        text(prompt, "question").unwrap_or("Hermes asks"),
                        TITLE_AT_MOST,
                    ),
                    since: at,
                }
            }
            _ => return,
        };
        self.dialogs.push((id.to_owned(), dialog));
    }

    /// Words admitted as a prompt: the turn they begin takes the say.
    fn admitted(&mut self, say: Option<String>, admission: &str) {
        let Some(say) = say else {
            return;
        };
        match &mut self.turn {
            // The turn began before the owner answered the submit.
            Some(turn) if turn.admission.as_deref() == Some(admission) => turn.says.push(say),
            _ => {
                self.admitted.insert(admission.to_owned(), say);
            }
        }
    }

    /// Words steered into the turn of `generation`.
    fn steered(&mut self, say: Option<String>, generation: u64) {
        if let (Some(say), Some(turn)) = (say, &mut self.turn)
            && turn.generation.is_none_or(|running| running == generation)
        {
            turn.says.push(say);
        }
    }

    fn apply(&mut self, event: &Value, payload: &Value, at: u64) {
        let generation = event.get("execution_generation").and_then(Value::as_u64);
        match text(event, "type").unwrap_or_default() {
            "message.start" => {
                let admission = text(event, "admission_id")
                    .or_else(|| text(payload, "admission_id"))
                    .map(str::to_owned);
                if self
                    .turn
                    .as_ref()
                    .is_some_and(|turn| turn.generation.is_some() && turn.generation != generation)
                {
                    self.close(at, None);
                }
                if let Some(generation) = generation {
                    self.generation = generation;
                }
                self.begin(at, generation, admission);
            }
            "message.complete" => {
                let outcome = text(payload, "outcome");
                let status = text(payload, "status");
                if outcome == Some("cancelled") || status == Some("interrupted") {
                    self.cancelled = generation.or(Some(self.generation));
                }
                let failed = match outcome {
                    Some(outcome) => outcome != "completed",
                    None => status.is_some_and(|status| status != "complete"),
                };
                let error = failed.then(|| {
                    text(payload, "error")
                        .or_else(|| text(payload, "failure_reason"))
                        .or(outcome)
                        .or(status)
                        .unwrap_or("the turn failed")
                        .to_owned()
                });
                self.close(at, error);
            }
            "session.info" => {
                self.positions(payload);
                if let Some(pending) = payload.get("pending") {
                    self.pending(pending);
                }
                self.info(payload);
                match payload.get("running").and_then(Value::as_bool) {
                    Some(true) if self.turn.is_none() => {
                        let admission = started(payload.get("pending"));
                        self.begin(at, generation.or(Some(self.generation)), admission);
                    }
                    Some(false) => self.close(at, None),
                    _ => {}
                }
            }
            "session.updated" => {
                self.positions(payload);
                match text(payload, "operation") {
                    Some("model") => self.info(payload),
                    Some("reasoning") => {
                        if let Some(effort) = text(payload, "effort") {
                            self.thinking = Some(effort.to_owned());
                        }
                    }
                    _ => {}
                }
            }
            "status.update" => self.compacting = text(payload, "kind") == Some("compacting"),
            "session.usage" => {
                let usage = payload.get("usage").unwrap_or(payload);
                let used = usage.get("context_used").and_then(Value::as_f64);
                let max = usage.get("context_max").and_then(Value::as_f64);
                if let (Some(used), Some(max)) = (used, max)
                    && max > 0.0
                {
                    self.context_left = Some((1.0 - used / max).clamp(0.0, 1.0));
                }
            }
            "tool.start" => {
                let Some(id) = text(payload, "tool_id") else {
                    return;
                };
                if self.calls.iter().any(|call| call.id == id) {
                    return;
                }
                // Named as the transcript reader names the call (`calls::call`).
                let entry = json!({"id": id, "function": {
                    "name": text(payload, "name").or_else(|| text(payload, "tool_name")).unwrap_or("tool"),
                    "arguments": payload.get("args").cloned().unwrap_or(Value::Null),
                }});
                if let Some(call) = calls::call(&entry, None, &[]) {
                    self.calls.push(LiveCall {
                        id: call.id,
                        tool: call.tool,
                        since: at,
                        children: Vec::new(),
                        jobs: Vec::new(),
                    });
                }
            }
            "tool.complete" => {
                let id = text(payload, "tool_id").unwrap_or_default();
                if let Some(index) = self.calls.iter().position(|call| call.id == id) {
                    let call = self.calls.remove(index);
                    self.end(call.id, at);
                }
            }
            "approval.request" | "clarify.request" => self.prompt(payload, at),
            "approval.settled" | "clarify.settled" => {
                let id = text(payload, "prompt_id").unwrap_or_default();
                self.dialogs.retain(|(open, _)| open != id);
            }
            // Interrupt, reap or teardown dropped the session's approvals
            // (`tui_gateway/session_lifecycle.py:585`).
            "approval.cancelled" => match payload.get("request_ids").and_then(Value::as_array) {
                Some(ids) => self
                    .dialogs
                    .retain(|(open, _)| !ids.iter().any(|id| id.as_str() == Some(open))),
                None => self.dialogs.clear(),
            },
            _ => {}
        }
    }

    fn begin(&mut self, at: u64, generation: Option<u64>, admission: Option<String>) {
        if self.turn.is_some() {
            return;
        }
        let says = admission
            .as_deref()
            .and_then(|admission| self.admitted.remove(admission))
            .into_iter()
            .collect();
        self.turn = Some(Turn {
            began_at: at,
            generation,
            admission,
            says,
        });
    }

    /// The turn that ran ended at `at`: a turn Steery's words began settles.
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
        LiveState {
            turn: self.turn.as_ref().map(|turn| LiveTurn {
                began_at: turn.began_at,
                says: turn.says.clone(),
            }),
            idle: Some(self.turn.is_none() && self.pending != Some(true) && !self.compacting),
            pending: self.pending,
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

/// The admission that runs now, among a session's admissions.
fn started(pending: Option<&Value>) -> Option<String> {
    pending?
        .as_array()?
        .iter()
        .find(|admission| text(admission, "status") == Some("started"))
        .and_then(|admission| text(admission, "admission_id"))
        .map(str::to_owned)
}

/// The owner's control socket in Hermes home `home`: `gateway.sock`, else the path that
/// `gateway.sock.path` names (`hermes_cli/gateway_runtime_discovery.py:143-160`).
fn socket(home: &Path) -> Option<PathBuf> {
    let socket = home.join("gateway.sock");
    if socket.exists() {
        return Some(socket);
    }
    let named = std::fs::read_to_string(home.join("gateway.sock.path")).ok()?;
    let named = PathBuf::from(named.trim());
    named.exists().then_some(named)
}

/// One request line on the owner's control socket; its `result` where it answered `ok`.
async fn control(socket: &Path, verb: &str, params: Value) -> Option<Value> {
    let asked = async {
        let mut stream = tokio::net::UnixStream::connect(socket).await.ok()?;
        let request = json!({"protocol": CONTROL, "id": 1, "verb": verb, "params": params});
        stream
            .write_all(format!("{request}\n").as_bytes())
            .await
            .ok()?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.ok()?;
        let answer: Value = serde_json::from_str(&line).ok()?;
        (answer.get("ok") == Some(&json!(true))).then(|| answer.get("result").cloned())?
    };
    tokio::time::timeout(WITHIN, asked).await.ok()?
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

fn no_answer(why: String) -> Error {
    Error::refused(refusal::NO_ANSWER, why)
}

/// The owner's words for a refused request: its reason, and the detail where it gave one.
fn failure(error: &Value) -> String {
    let message = text(error, "message").unwrap_or("Hermes's gateway refused the request");
    match error
        .pointer("/data/detail")
        .or_else(|| error.pointer("/data/details"))
        .and_then(Value::as_str)
    {
        Some(detail) if !detail.is_empty() => format!("{message}: {detail}"),
        _ => message.to_owned(),
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
