//! Grok Build's own door (PROTOCOL.md §4.3): its leader. A Grok TUI in leader mode
//! (`[cli] use_leader = true`) runs its conversations in `grok agent leader`, a process that
//! listens on `<root>/leader.sock` and serves every client that registers there. Each frame
//! is a 4-byte big-endian length and a JSON object; ACP rides in `{"type": "acp", "payload"}`.
//! Any client takes words (`session/prompt`, `_x.ai/interject`), a model
//! (`session/set_model`) and a stop (`session/cancel`) into the TUI's conversation, and the
//! leader broadcasts what runs to every client attached to it. The driver sends that as
//! `session/changed`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    self, Dialog, DialogKind, Holder, HolderStop, LiveCall, LiveEnded, LiveSettled, LiveState,
    LiveTurn, Pick, Picked, Said, Say, SayMode, SessionChanged, Stop, Stopped,
};
use steery_sdk::{Error, Notifier, method, refusal};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

use super::{drive, events};

/// How long the leader has to answer a request, take words, and end a stopped turn.
const WITHIN: Duration = Duration::from_secs(10);
/// The largest frame the leader sends (`MAX_MESSAGE_SIZE`, 1.0.77).
const FRAME_AT_MOST: u32 = 64 * 1024 * 1024;
/// How many ended calls and settled turns a state carries.
const ENDED: usize = 64;
const SETTLED: usize = 16;
/// The longest error and dialog title a state carries.
const ERROR_AT_MOST: usize = 600;
const TITLE_AT_MOST: usize = 200;
/// Overrides the leader's socket, as Grok's own `GROK_LEADER_SOCKET` does.
const SOCKET_ENV: &str = "GROK_LEADER_SOCKET";

/// The leader the driver speaks to, while its connection lives.
pub struct Live {
    notifier: Notifier,
    leader: tokio::sync::Mutex<Option<Arc<Leader>>>,
}

/// One connection to Grok's leader, and what its broadcasts said of each conversation
/// followed.
struct Leader {
    out: mpsc::UnboundedSender<Vec<u8>>,
    next: AtomicU64,
    ended: AtomicBool,
    /// Replies awaited, by request id.
    replies: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    /// Says that wait for Grok to take their words, by their prompt's request id.
    takes: Mutex<HashMap<u64, oneshot::Sender<()>>>,
    followed: Mutex<HashMap<String, Fold>>,
}

impl Live {
    pub fn new(notifier: Notifier) -> Self {
        Self {
            notifier,
            leader: tokio::sync::Mutex::default(),
        }
    }

    /// The leader's connection: the one open, else a new one where its socket listens.
    async fn leader(self: &Arc<Self>, root: &Path) -> Option<Arc<Leader>> {
        let mut held = self.leader.lock().await;
        if let Some(leader) = held
            .as_ref()
            .filter(|leader| !leader.ended.load(Ordering::Relaxed))
        {
            return Some(Arc::clone(leader));
        }
        let stream = UnixStream::connect(socket(root)).await.ok()?;
        let (mut reader, mut writer) = stream.into_split();
        let (out, mut outbox) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(frame) = outbox.recv().await {
                if writer.write_all(&frame).await.is_err() {
                    break;
                }
            }
        });
        let _ = out.send(frame(&json!({
            "type": "register",
            "client_type": "steery",
            "mode": "stdio",
            "capabilities": {},
        })));
        let registered = tokio::time::timeout(WITHIN, read_frame(&mut reader))
            .await
            .ok()??;
        if registered["type"] != "registered" {
            return None;
        }
        let leader = Arc::new(Leader {
            out,
            next: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            replies: Mutex::default(),
            takes: Mutex::default(),
            followed: Mutex::default(),
        });
        let live = Arc::clone(self);
        let reading = Arc::clone(&leader);
        tokio::spawn(async move {
            while let Some(message) = read_frame(&mut reader).await {
                if message["type"] == "acp"
                    && let Some(payload) = message["payload"].as_str()
                    && let Ok(payload) = serde_json::from_str::<Value>(payload)
                {
                    live.frame(&reading, payload);
                }
            }
            live.closed(&reading);
        });
        leader
            .ask(
                "initialize",
                json!({"protocolVersion": 1, "clientCapabilities": {}}),
            )
            .await
            .ok()?;
        *held = Some(Arc::clone(&leader));
        Some(leader)
    }

    /// One ACP message of the leader: a reply to the driver's request, or a broadcast of a
    /// conversation. A request of the leader (a permission, a question) is for the person's
    /// TUI to answer, so the driver leaves it be.
    fn frame(&self, leader: &Leader, message: Value) {
        let Some(name) = message["method"].as_str() else {
            if let Some(id) = message["id"].as_u64()
                && let Some(reply) = lock(&leader.replies).remove(&id)
            {
                let _ = reply.send(message);
            }
            return;
        };
        if message.get("id").is_some() {
            return;
        }
        let params = &message["params"];
        let at = params["_meta"]["agentTimestampMs"]
            .as_u64()
            .unwrap_or_else(now);
        if name == "_x.ai/sessions/changed" {
            for row in params["upserted"].as_array().into_iter().flatten() {
                if let Some(id) = row["sessionId"].as_str() {
                    self.apply(leader, id, |fold| fold.runs_on(row));
                }
            }
            return;
        }
        let Some(id) = params["sessionId"].as_str() else {
            return;
        };
        let took = self.apply(leader, id, |fold| fold.apply(name, params, at));
        for request in took.into_iter().flatten() {
            if let Some(take) = lock(&leader.takes).remove(&request) {
                let _ = take.send(());
            }
        }
    }

    /// Fold one broadcast into the conversation `id` where the driver follows it, and send
    /// its state where that changed.
    fn apply<T>(
        &self,
        leader: &Leader,
        id: &str,
        change: impl FnOnce(&mut Fold) -> T,
    ) -> Option<T> {
        let (out, state) = {
            let mut followed = lock(&leader.followed);
            let fold = followed.get_mut(id)?;
            let out = change(fold);
            (out, fold.changed())
        };
        if let Some(state) = state {
            self.changed(id, state);
        }
        Some(out)
    }

    /// The connection closed: nothing the leader ran is known to run.
    fn closed(&self, leader: &Leader) {
        leader.ended.store(true, Ordering::Relaxed);
        lock(&leader.replies).clear();
        lock(&leader.takes).clear();
        let followed = std::mem::take(&mut *lock(&leader.followed));
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

    /// The leader of conversation `id`, where a person's Grok TUI shows it and the leader
    /// runs it; the driver then follows the conversation's broadcasts.
    async fn door(self: &Arc<Self>, root: &Path, id: &str) -> Option<Arc<Leader>> {
        drive::registered(root, id)?;
        let leader = self.leader(root).await?;
        if !lock(&leader.followed).contains_key(id) {
            let listed = leader.ask("_x.ai/sessions/list", json!({})).await.ok()?;
            let row = listed["result"]["sessions"]
                .as_array()?
                .iter()
                .find(|row| row["sessionId"] == id && row["resident"] == true)?
                .clone();
            let mut fold = Fold::default();
            fold.row(&row, row["lastChangeUnixMs"].as_u64().unwrap_or_else(now));
            lock(&leader.followed).insert(id.to_owned(), fold);
            // A resume attaches the driver to the conversation's broadcasts with no replay.
            if leader
                .ask(
                    "session/resume",
                    json!({"sessionId": id, "cwd": row["cwd"], "mcpServers": []}),
                )
                .await
                .is_err()
            {
                lock(&leader.followed).remove(id);
                return None;
            }
        }
        let state = lock(&leader.followed).get_mut(id).and_then(Fold::changed);
        if let Some(state) = state {
            self.changed(id, state);
        }
        Some(leader)
    }

    /// `held`: a person's Grok TUI whose conversation the leader runs, with the turn that
    /// runs as its token; none where no TUI in leader mode shows it.
    pub async fn held(self: &Arc<Self>, root: &Path, id: &str) -> Option<Holder> {
        let leader = self.door(root, id).await?;
        Some(Holder::Person {
            door: Some(harness::Door::Picks),
            stop: HolderStop::Owner(leader.token(id)),
        })
    }

    /// `say`: the words into the conversation through the leader. Words that steer go into
    /// the turn that runs (`_x.ai/interject`); others are a prompt, which Grok queues behind
    /// a turn that runs.
    pub async fn say(self: &Arc<Self>, root: &Path, say: Say) -> Result<Said, Error> {
        let id = &say.external_id;
        let Some(leader) = self.door(root, id).await else {
            return Err(no_door());
        };
        let (running, pending) = leader.running(id);
        if say.mode == SayMode::Immediate && (running || pending) {
            return Err(Error::refused(
                refusal::HARNESS_BUSY,
                "a turn runs or words wait in Grok",
            ));
        }
        if running && say.mode == SayMode::Steer {
            let named = say
                .id
                .clone()
                .unwrap_or_else(|| format!("steery-{}", now()));
            if let Some(fold) = lock(&leader.followed).get_mut(id) {
                fold.steers.push((named.clone(), say.id.clone()));
            }
            let reply = leader
                .ask(
                    "_x.ai/interject",
                    json!({"sessionId": id, "text": say.words, "interjectionId": named}),
                )
                .await
                .map_err(no_answer)?;
            return Ok(Said {
                took: reply["result"]["status"] == "queued",
            });
        }
        // A prompt is answered only when its turn ends; Grok's queue naming the words, or
        // the turn that runs them, says it took them.
        let (request, reply) = leader.send(
            "session/prompt",
            json!({"sessionId": id, "prompt": [{"type": "text", "text": say.words}]}),
        );
        let (take, taken) = oneshot::channel();
        lock(&leader.takes).insert(request, take);
        if let Some(fold) = lock(&leader.followed).get_mut(id) {
            fold.asked.push(Asked {
                words: say.words.clone(),
                say: say.id.clone(),
                request,
            });
        }
        let took = tokio::select! {
            _ = taken => true,
            answer = reply => match answer {
                Ok(message) if message.get("error").is_some() => {
                    lock(&leader.takes).remove(&request);
                    return Err(no_answer(failure(&message)));
                }
                Ok(_) => true,
                Err(_) => false,
            },
            _ = tokio::time::sleep(WITHIN) => false,
        };
        lock(&leader.takes).remove(&request);
        Ok(Said { took })
    }

    /// `pick`: the model and reasoning effort the conversation's next request runs on,
    /// through the leader, as Grok reads them back.
    pub async fn pick(self: &Arc<Self>, root: &Path, pick: Pick) -> Result<Picked, Error> {
        if let Some(thinking) = &pick.thinking
            && !drive::EFFORT.contains(&thinking.as_str())
        {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                format!("Grok Build has no reasoning effort {thinking}"),
            ));
        }
        let id = &pick.external_id;
        let Some(leader) = self.door(root, id).await else {
            return Err(no_door());
        };
        let model = pick.model.clone().or_else(|| {
            lock(&leader.followed)
                .get(id)
                .and_then(|fold| fold.model.clone())
        });
        if let Some(model) = model
            && (pick.model.is_some() || pick.thinking.is_some())
        {
            let mut params = json!({"sessionId": id, "modelId": model});
            if let Some(thinking) = &pick.thinking {
                params["_meta"] = json!({"reasoningEffort": thinking});
            }
            leader
                .ask("session/set_model", params)
                .await
                .map_err(|why| Error::refused(refusal::MODEL_MISSING, why))?;
        }
        let followed = lock(&leader.followed);
        let fold = followed.get(id);
        Ok(Picked {
            model: fold.and_then(|fold| fold.model.clone()),
            thinking: fold.and_then(|fold| fold.thinking.clone()),
        })
    }

    /// `stop` of a TUI's owner: the leader cancels the turn that runs, where it is still the
    /// one `held` named, and where a say is named, the one its words went into.
    pub async fn stop(self: &Arc<Self>, root: &Path, stop: &Stop) -> Result<Stopped, Error> {
        let Holder::Person {
            stop: HolderStop::Owner(token),
            ..
        } = &stop.holder
        else {
            return Ok(not_stopped("no Grok TUI in leader mode holds it"));
        };
        let id = &stop.external_id;
        let Some(leader) = self.door(root, id).await else {
            return Ok(not_stopped("the Grok TUI that held it closed"));
        };
        if !leader.running(id).0 {
            return Ok(not_stopped("no turn runs in Grok"));
        }
        if let Some(say) = &stop.say {
            let took = lock(&leader.followed)
                .get(id)
                .is_some_and(|fold| fold.says.contains(say));
            if !took {
                return Err(Error::refused(
                    refusal::OTHER_TURN,
                    "the turn that runs did not take these words",
                ));
            }
        } else if token.is_some() && *token != leader.token(id) {
            return Ok(not_stopped("the turn it named ended; another runs"));
        }
        leader.notify("session/cancel", json!({"sessionId": id}));
        let ended = tokio::time::timeout(WITHIN, async {
            while leader.running(id).0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .is_ok();
        Ok(Stopped {
            stopped: ended,
            proof: Some(if ended {
                "Grok's leader cancelled the turn and the conversation went idle".into()
            } else {
                "Grok's leader took the cancel, but the turn still runs".into()
            }),
        })
    }
}

impl Leader {
    /// Send an ACP request; its reply comes on the receiver.
    fn send(&self, name: &str, params: Value) -> (u64, oneshot::Receiver<Value>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (reply, answer) = oneshot::channel();
        lock(&self.replies).insert(id, reply);
        self.acp(&json!({"jsonrpc": "2.0", "id": id, "method": name, "params": params}));
        (id, answer)
    }

    fn notify(&self, name: &str, params: Value) {
        self.acp(&json!({"jsonrpc": "2.0", "method": name, "params": params}));
    }

    fn acp(&self, message: &Value) {
        let _ = self.out.send(frame(
            &json!({"type": "acp", "payload": message.to_string()}),
        ));
    }

    /// A request's result, or the leader's words where it refused.
    async fn ask(&self, name: &str, params: Value) -> Result<Value, String> {
        let (id, answer) = self.send(name, params);
        let message = tokio::time::timeout(WITHIN, answer).await;
        lock(&self.replies).remove(&id);
        let message = message
            .map_err(|_| format!("Grok's leader did not answer `{name}`"))?
            .map_err(|_| "the connection to Grok's leader closed".to_owned())?;
        if message.get("error").is_some() {
            return Err(failure(&message));
        }
        Ok(message.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Whether a turn of conversation `id` runs, and whether words wait.
    fn running(&self, id: &str) -> (bool, bool) {
        lock(&self.followed).get(id).map_or((false, false), |fold| {
            (fold.began.is_some(), fold.pending == Some(true))
        })
    }

    /// The token of the turn that runs: its prompt's id, else when it began.
    fn token(&self, id: &str) -> Option<String> {
        let followed = lock(&self.followed);
        let fold = followed.get(id)?;
        let began = fold.began?;
        Some(fold.running.clone().unwrap_or_else(|| began.to_string()))
    }
}

/// A prompt the driver sent, until Grok's queue names it.
struct Asked {
    words: String,
    say: Option<String>,
    request: u64,
}

/// What the leader's broadcasts said of one conversation.
#[derive(Default)]
struct Fold {
    /// The prompt id of the turn that runs, and when it began.
    running: Option<String>,
    began: Option<u64>,
    /// The say ids the turn that runs took.
    says: Vec<String>,
    /// Prompts sent that Grok's queue has not named yet.
    asked: Vec<Asked>,
    /// The say of each of the driver's prompts that waits in Grok's queue, by its entry id.
    queued: HashMap<String, Option<String>>,
    /// Interjections sent: their id, and their say's id.
    steers: Vec<(String, Option<String>)>,
    pending: Option<bool>,
    model: Option<String>,
    thinking: Option<String>,
    calls: Vec<LiveCall>,
    ended: VecDeque<LiveEnded>,
    settled: VecDeque<LiveSettled>,
    /// Permissions and questions open in front of the person, by the call that asks.
    dialogs: Vec<(String, Dialog)>,
    /// The state last sent.
    sent: Option<LiveState>,
}

impl Fold {
    /// A row of Grok's roster when the driver first follows the conversation: whether it
    /// works, and what it runs on. Later rows name only what it runs on: the roster can say
    /// `idle` after the queue started the next turn.
    fn row(&mut self, row: &Value, at: u64) {
        self.runs_on(row);
        if matches!(row["activity"].as_str(), Some("working" | "needs_input")) {
            self.begin(None, at);
        }
    }

    fn runs_on(&mut self, row: &Value) {
        if let Some(model) = row["modelId"].as_str() {
            self.model = Some(model.to_owned());
        }
        if let Some(effort) = row["reasoningEffort"].as_str() {
            self.thinking = Some(effort.to_owned());
        }
    }

    /// One broadcast of the conversation; the request ids of the driver's prompts that
    /// Grok took with it.
    fn apply(&mut self, name: &str, params: &Value, at: u64) -> Vec<u64> {
        match name {
            "_x.ai/queue/changed" => return self.queue(params, at),
            "_x.ai/session/interjection" => {
                let named = params["interjectionId"].as_str().unwrap_or_default();
                if let Some(at_steer) = self.steers.iter().position(|(id, _)| id == named) {
                    let (_, say) = self.steers.remove(at_steer);
                    self.says.extend(say);
                }
            }
            "session/update" | "_x.ai/session_notification" => {
                let update = &params["update"];
                match update["sessionUpdate"].as_str().unwrap_or_default() {
                    "turn_completed" => {
                        let reason = update["stop_reason"].as_str().unwrap_or("end_turn");
                        let failed = !matches!(reason, "end_turn" | "cancelled");
                        self.close(at, failed.then(|| cut(reason, ERROR_AT_MOST)));
                    }
                    "model_changed" => {
                        if let Some(model) = update["model_id"].as_str() {
                            self.model = Some(model.to_owned());
                        }
                        if let Some(effort) = update["reasoning_effort"].as_str() {
                            self.thinking = Some(effort.to_owned());
                        }
                    }
                    "tool_call" => {
                        let call = events::call(update);
                        if !call.id.is_empty() && !self.calls.iter().any(|c| c.id == call.id) {
                            self.calls.push(LiveCall {
                                id: call.id,
                                tool: call.tool,
                                since: at,
                                children: Vec::new(),
                                jobs: Vec::new(),
                            });
                        }
                    }
                    "tool_call_update"
                        if matches!(update["status"].as_str(), Some("completed" | "failed")) =>
                    {
                        let id = update["toolCallId"].as_str().unwrap_or_default();
                        if let Some(index) = self.calls.iter().position(|call| call.id == id) {
                            let call = self.calls.remove(index);
                            self.end(call.id, at);
                        }
                    }
                    "pending_interaction" => {
                        let call = update["tool_call_id"].as_str().unwrap_or_default();
                        let tool = self
                            .calls
                            .iter()
                            .find(|open| open.id == call)
                            .map_or("a tool", |open| open.tool.as_str());
                        let (kind, title) = match update["kind"].as_str() {
                            Some("permission") => (DialogKind::Confirm, format!("Allow {tool}?")),
                            Some(other) => (DialogKind::Custom, format!("Grok asks: {other}")),
                            None => (DialogKind::Custom, "Grok asks".to_owned()),
                        };
                        self.dialogs.push((
                            call.to_owned(),
                            Dialog {
                                kind,
                                title: cut(&title, TITLE_AT_MOST),
                                since: at,
                            },
                        ));
                    }
                    "interaction_resolved" => {
                        let call = update["tool_call_id"].as_str().unwrap_or_default();
                        self.dialogs.retain(|(open, _)| open != call);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Grok's queue: the words that wait, and the prompt that runs.
    fn queue(&mut self, params: &Value, at: u64) -> Vec<u64> {
        let mut took = Vec::new();
        let entries = params["entries"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        self.pending = Some(!entries.is_empty());
        for entry in entries {
            let (Some(id), Some(words)) = (entry["id"].as_str(), entry["text"].as_str()) else {
                continue;
            };
            if self.queued.contains_key(id) {
                continue;
            }
            if let Some(asked) = self.claim(words) {
                took.push(asked.request);
                self.queued.insert(id.to_owned(), asked.say);
            }
        }
        if let Some(prompt) = params["runningPromptId"].as_str()
            && self.running.as_deref() != Some(prompt)
        {
            if self.running.is_some() {
                self.close(at, None);
            }
            self.begin(Some(prompt.to_owned()), at);
            let say = match self.queued.remove(prompt) {
                Some(say) => say,
                None => params["runningText"]
                    .as_str()
                    .and_then(|words| self.claim(words))
                    .and_then(|asked| {
                        took.push(asked.request);
                        asked.say
                    }),
            };
            self.says.extend(say);
        }
        took
    }

    /// The driver's prompt of exactly these words, which Grok's queue now names.
    fn claim(&mut self, words: &str) -> Option<Asked> {
        let at = self.asked.iter().position(|asked| asked.words == words)?;
        Some(self.asked.remove(at))
    }

    fn begin(&mut self, prompt: Option<String>, at: u64) {
        if prompt.is_some() {
            self.running = prompt;
        }
        if self.began.is_none() {
            self.began = Some(at);
        }
    }

    /// The turn that ran ended at `at`: a turn Steery's words began settles.
    fn close(&mut self, at: u64, error: Option<String>) {
        for call in std::mem::take(&mut self.calls) {
            self.end(call.id, at);
        }
        self.dialogs.clear();
        self.running = None;
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
            thinking: self.thinking.clone(),
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

/// The leader's socket: `GROK_LEADER_SOCKET`, else `<root>/leader.sock`.
fn socket(root: &Path) -> PathBuf {
    std::env::var_os(SOCKET_ENV)
        .filter(|socket| !socket.is_empty())
        .map_or_else(|| root.join("leader.sock"), PathBuf::from)
}

/// One frame: its length, big-endian, then its JSON.
fn frame(message: &Value) -> Vec<u8> {
    let body = message.to_string().into_bytes();
    let mut frame = u32::try_from(body.len())
        .unwrap_or(u32::MAX)
        .to_be_bytes()
        .to_vec();
    frame.extend(body);
    frame
}

async fn read_frame(reader: &mut (impl AsyncReadExt + Unpin)) -> Option<Value> {
    let mut length = [0u8; 4];
    reader.read_exact(&mut length).await.ok()?;
    let length = u32::from_be_bytes(length);
    if length > FRAME_AT_MOST {
        return None;
    }
    let mut body = vec![0u8; length as usize];
    reader.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

fn no_door() -> Error {
    Error::refused(
        refusal::NO_DOOR,
        "no Grok TUI in leader mode shows this conversation: a TUI runs its turns through \
         Grok's leader only when opened with `--leader`, or with `use_leader = true` under \
         `[cli]` in Grok's config.toml",
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

/// The leader's words for a refused request.
fn failure(message: &Value) -> String {
    let error = &message["error"];
    let words = error["data"]
        .as_str()
        .or_else(|| error["message"].as_str())
        .unwrap_or("Grok's leader refused the request");
    cut(words, ERROR_AT_MOST)
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
