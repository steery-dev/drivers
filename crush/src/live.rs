//! Crush's own door (PROTOCOL.md §4.3): its server. A Crush TUI opened with
//! `CRUSH_CLIENT_SERVER=1` runs its sessions in `crush server`, one process for the person,
//! on a Unix socket ([`door::socket`]). The server keeps one workspace for each project, and
//! any client of it takes words (`POST /agent`), a stop (`POST /agent/sessions/<id>/cancel`)
//! and an answer to a permission. It sends what runs in a workspace as server-sent events,
//! which the driver sends as `session/changed`.
//!
//! Words with a run id go in as their own turn, after any that runs; words without one fold
//! into the turn that runs at its next step (`drainQueueForStep`, 0.100.0). Crush picks the
//! model for a whole workspace, so the door keeps its own.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::wire::harness::{
    self, Dialog, DialogKind, Holder, HolderStop, LiveCall, LiveEnded, LiveSettled, LiveState,
    LiveTurn, Pick, Picked, Said, Say, SayMode, SessionChanged, Stop, Stopped,
};
use steery_sdk::{Error, Notifier, method, refusal};
use tokio::sync::{Notify, oneshot};

use crate::door::{self, Answer};
use crate::drive::now;
use crate::store::Store;
use crate::{calls, mcp};

/// How long Crush has to take words and to end a stopped turn.
const WITHIN: Duration = Duration::from_secs(8);
/// How often the driver asks the server whether a followed session works, while it works.
const EVERY: Duration = Duration::from_secs(1);
/// How often the driver asks whether a person still has a followed session open.
const VIEWED_EVERY: Duration = Duration::from_secs(5);
/// How many ended calls and settled turns a state carries.
const ENDED: usize = 64;
const SETTLED: usize = 16;
/// The longest error and dialog title a state carries.
const ERROR_AT_MOST: usize = 600;
const TITLE_AT_MOST: usize = 200;

/// The workspaces of the person's Crush server that the driver follows.
pub struct Live {
    notifier: Notifier,
    workspaces: tokio::sync::Mutex<HashMap<String, Arc<Workspace>>>,
}

/// One workspace of the server, its event stream, and what its events said of each session
/// followed.
struct Workspace {
    socket: PathBuf,
    id: String,
    /// The MCP servers Crush loads in the project, which name its calls' tools.
    servers: Vec<String>,
    ended: AtomicBool,
    /// Wakes the loop that asks the server what runs.
    poke: Notify,
    /// Closes the event stream, once no session is followed.
    shut: Notify,
    next: AtomicU64,
    /// Says that wait for Crush to take their words, by their ask.
    takes: Mutex<HashMap<u64, oneshot::Sender<()>>>,
    followed: Mutex<HashMap<String, Fold>>,
}

impl Live {
    pub fn new(notifier: Notifier) -> Self {
        Self {
            notifier,
            workspaces: tokio::sync::Mutex::default(),
        }
    }

    /// The workspace that runs session `id`, where a person's Crush window shows it through
    /// the server; the driver then follows the session's events.
    async fn door(self: &Arc<Self>, home: &Path, id: &str) -> Option<Arc<Workspace>> {
        let socket = door::socket(home)?;
        let place = Store::new(home).place(id).ok()?;
        let place = std::fs::canonicalize(&place).unwrap_or(place);
        let workspace = self.workspace(home, &socket, &place).await?;
        if !workspace.followed().contains_key(id) {
            let session = workspace.get(&format!("sessions/{id}")).await?;
            if session["attached_clients"].as_u64().unwrap_or(0) == 0 {
                return None;
            }
            workspace.followed().insert(id.to_owned(), Fold::default());
            workspace.refresh(self, id).await;
        }
        workspace.send(self, id);
        Some(workspace)
    }

    /// The followed workspace of `place`, else the server's workspace there with its events
    /// opened; none where no server runs or none has the project open.
    async fn workspace(
        self: &Arc<Self>,
        home: &Path,
        socket: &Path,
        place: &Path,
    ) -> Option<Arc<Workspace>> {
        let listed = door::request(socket, "GET", "/v1/workspaces", None)
            .await
            .ok()?
            .json()?;
        let id = listed.as_array()?.iter().find(|workspace| {
            workspace["path"].as_str().is_some_and(|path| {
                let path = Path::new(path);
                path == place || std::fs::canonicalize(path).is_ok_and(|path| path == place)
            })
        })?["id"]
            .as_str()?
            .to_owned();
        let mut workspaces = self.workspaces.lock().await;
        if let Some(workspace) = workspaces
            .get(&id)
            .filter(|workspace| !workspace.ended.load(Ordering::Relaxed))
        {
            return Some(Arc::clone(workspace));
        }
        let workspace = Arc::new(Workspace {
            socket: socket.to_owned(),
            id: id.clone(),
            servers: mcp::names(home, place),
            ended: AtomicBool::new(false),
            poke: Notify::new(),
            shut: Notify::new(),
            next: AtomicU64::new(0),
            takes: Mutex::default(),
            followed: Mutex::default(),
        });
        let (opened, open) = oneshot::channel();
        let (live, reading) = (Arc::clone(self), Arc::clone(&workspace));
        tokio::spawn(async move {
            let path = format!(
                "/v1/workspaces/{}/events?client_id={}",
                reading.id,
                door::uuid()
            );
            let mut opened = Some(opened);
            let reading_events = door::events(&reading.socket, &path, |event| {
                if let Some(opened) = opened.take() {
                    let _ = opened.send(());
                }
                live.event(&reading, &event);
            });
            tokio::select! {
                _ = reading_events => {}
                () = reading.shut.notified() => {}
            }
            live.closed(&reading);
        });
        let (live, asking) = (Arc::clone(self), Arc::clone(&workspace));
        tokio::spawn(async move { live.ask_loop(&asking).await });
        // The stream answers its head before its first event; a moment lets it open, so
        // words said next are seen taken.
        let _ = tokio::time::timeout(Duration::from_millis(300), open).await;
        workspaces.insert(id, Arc::clone(&workspace));
        Some(workspace)
    }

    /// One event of the workspace, folded into the session it names where the driver follows
    /// it.
    fn event(&self, workspace: &Workspace, event: &Value) {
        let kind = event["type"].as_str().unwrap_or_default();
        let change = event["payload"]["type"].as_str().unwrap_or_default();
        let body = &event["payload"]["payload"];
        let session = body["session_id"].as_str().unwrap_or_default();
        let at = now();
        match kind {
            "message" => {
                let took = workspace.apply(self, session, |fold| {
                    fold.message(body, change == "created", at, &workspace.servers)
                });
                if let Some(Some(ask)) = took {
                    workspace.taken(ask);
                    workspace.poke.notify_one();
                }
            }
            "permission_request" => {
                workspace.apply(self, session, |fold| fold.permission(body, at));
            }
            "permission_notification" => {
                if [&body["granted"], &body["denied"], &body["cancelled"]]
                    .iter()
                    .any(|done| done.as_bool() == Some(true))
                {
                    let call = body["tool_call_id"].as_str().unwrap_or_default();
                    workspace.apply(self, session, |fold| fold.answered(call));
                }
            }
            "question_batch_request" => {
                workspace.apply(self, session, |fold| fold.question(body, at));
            }
            "question_batch_notification" => {
                let batch = body["batch_id"].as_str().unwrap_or_default();
                workspace.apply(self, session, |fold| fold.answered(batch));
            }
            "run_complete" => {
                workspace.apply(self, session, |fold| fold.completed(body, at));
                workspace.poke.notify_one();
            }
            "session" | "agent_event" => workspace.poke.notify_one(),
            _ => {}
        }
    }

    /// Asks the server what runs: at once when an event says something changed, every second
    /// while a followed session works or words wait, and every five whether a person still
    /// has each open. A session nobody shows and where nothing runs is let go; a workspace
    /// with none left closes its stream, so the server can end when its windows close.
    async fn ask_loop(self: &Arc<Self>, workspace: &Arc<Workspace>) {
        let mut viewed_at = tokio::time::Instant::now();
        while !workspace.ended.load(Ordering::Relaxed) {
            let poked = tokio::select! {
                () = workspace.poke.notified() => true,
                () = tokio::time::sleep(EVERY) => false,
            };
            let viewing = viewed_at.elapsed() >= VIEWED_EVERY;
            if viewing {
                viewed_at = tokio::time::Instant::now();
            }
            let ids: Vec<String> = workspace
                .followed()
                .iter()
                .filter(|(_, fold)| poked || viewing || fold.works())
                .map(|(id, _)| id.clone())
                .collect();
            for id in ids {
                workspace.refresh(self, &id).await;
                if viewing {
                    let viewed =
                        workspace
                            .get(&format!("sessions/{id}"))
                            .await
                            .is_some_and(|session| {
                                session["attached_clients"].as_u64().unwrap_or(0) > 0
                            });
                    let gone = !viewed
                        && workspace
                            .followed()
                            .get(&id)
                            .is_some_and(|fold| !fold.works());
                    if gone {
                        workspace.followed().remove(&id);
                        self.changed(&id, LiveState::default());
                    }
                }
            }
            let mut workspaces = self.workspaces.lock().await;
            if workspace.followed().is_empty() {
                workspaces.remove(&workspace.id);
                workspace.ended.store(true, Ordering::Relaxed);
                workspace.shut.notify_one();
            }
        }
    }

    /// The stream closed: nothing the server ran is known to run.
    fn closed(&self, workspace: &Workspace) {
        workspace.ended.store(true, Ordering::Relaxed);
        workspace.poke.notify_one();
        lock(&workspace.takes).clear();
        let followed = std::mem::take(&mut *workspace.followed());
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

    /// `held`: a person's Crush window that shows the session through the server, with the
    /// turn that runs as its token; none where no window does.
    pub async fn held(self: &Arc<Self>, home: &Path, id: &str) -> Option<Holder> {
        let workspace = self.door(home, id).await?;
        Some(Holder::Person {
            door: Some(harness::Door::Keeps),
            stop: HolderStop::Owner(workspace.token(id)),
        })
    }

    /// `say`: the words into the session through the server. Words that steer go into the
    /// turn that runs, at its next step; others carry a run id, so Crush runs them as their
    /// own turn, after any that runs.
    pub async fn say(self: &Arc<Self>, home: &Path, say: Say) -> Result<Said, Error> {
        let id = &say.external_id;
        let Some(workspace) = self.door(home, id).await else {
            return Err(no_door());
        };
        workspace.refresh(self, id).await;
        let (running, pending) = workspace.running(id);
        if say.mode == SayMode::Immediate && (running || pending) {
            return Err(Error::refused(
                refusal::HARNESS_BUSY,
                "a turn runs or words wait in Crush",
            ));
        }
        let run = (!(running && say.mode == SayMode::Steer)).then(door::uuid);
        let ask = workspace.next.fetch_add(1, Ordering::Relaxed);
        let (take, taken) = oneshot::channel();
        lock(&workspace.takes).insert(ask, take);
        if let Some(fold) = workspace.followed().get_mut(id) {
            fold.asked.push(Asked {
                ask,
                words: say.words.clone(),
                say: say.id.clone(),
                run: run.clone(),
            });
        }
        let mut message = json!({"session_id": id, "prompt": say.words});
        if let Some(run) = &run {
            message["run_id"] = json!(run);
        }
        let sent = workspace.post("agent", &message).await;
        if let Err(why) = sent {
            lock(&workspace.takes).remove(&ask);
            if let Some(fold) = workspace.followed().get_mut(id) {
                fold.asked.retain(|asked| asked.ask != ask);
            }
            return Err(Error::refused(refusal::NO_ANSWER, why));
        }
        // Crush holds words that wait in its queue, and takes them when it writes their
        // message.
        let took = tokio::select! {
            _ = taken => true,
            () = workspace.queued(id, &say.words) => true,
            () = tokio::time::sleep(WITHIN) => false,
        };
        lock(&workspace.takes).remove(&ask);
        Ok(Said { took })
    }

    /// `pick`: Crush sets its model for the whole project, every session in it, so the door
    /// keeps the session's own; it reads back the model of its last answer.
    pub async fn pick(self: &Arc<Self>, home: &Path, pick: Pick) -> Result<Picked, Error> {
        if pick.model.is_some() || pick.thinking.is_some() {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Crush sets its model for a whole project, not for one session",
            ));
        }
        let id = &pick.external_id;
        let Some(workspace) = self.door(home, id).await else {
            return Err(no_door());
        };
        let model = workspace
            .followed()
            .get(id)
            .and_then(|fold| fold.model.clone());
        Ok(Picked {
            model,
            thinking: None,
        })
    }

    /// `stop` of a window's owner: the server cancels the turn that runs, where it is still
    /// the one `held` named, and where a say is named, the one its words went into. Crush
    /// drops the words that wait with it.
    pub async fn stop(self: &Arc<Self>, home: &Path, stop: &Stop) -> Result<Stopped, Error> {
        let Holder::Person {
            stop: HolderStop::Owner(token),
            ..
        } = &stop.holder
        else {
            return Ok(not_stopped("no Crush window holds it through the server"));
        };
        let id = &stop.external_id;
        let Some(workspace) = self.door(home, id).await else {
            return Ok(not_stopped("the Crush window that held it closed"));
        };
        workspace.refresh(self, id).await;
        if !workspace.running(id).0 {
            return Ok(not_stopped("no turn runs in Crush"));
        }
        if let Some(say) = &stop.say {
            let took = workspace
                .followed()
                .get(id)
                .is_some_and(|fold| fold.says.iter().any(|taken| &taken.say == say));
            if !took {
                return Err(Error::refused(
                    refusal::OTHER_TURN,
                    "the turn that runs did not take these words",
                ));
            }
        } else if token.is_some() && *token != workspace.token(id) {
            return Ok(not_stopped("the turn it named ended; another runs"));
        }
        workspace
            .post(&format!("agent/sessions/{id}/cancel"), &json!({}))
            .await
            .map_err(|why| Error::refused(refusal::NO_ANSWER, why))?;
        let ended = tokio::time::timeout(WITHIN, async {
            loop {
                workspace.refresh(self, id).await;
                if !workspace.running(id).0 {
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
                "Crush's server cancelled the turn and the session went idle".into()
            } else {
                "Crush's server took the cancel, but the turn still runs".into()
            }),
        })
    }
}

impl Workspace {
    fn followed(&self) -> std::sync::MutexGuard<'_, HashMap<String, Fold>> {
        lock(&self.followed)
    }

    /// `GET /v1/workspaces/<id>/<path>`, where the server answers 200.
    async fn get(&self, path: &str) -> Option<Value> {
        let answer = door::request(
            &self.socket,
            "GET",
            &format!("/v1/workspaces/{}/{path}", self.id),
            None,
        )
        .await
        .ok()?;
        (answer.status == 200).then(|| answer.json()).flatten()
    }

    /// `POST /v1/workspaces/<id>/<path>`; the server's words where it refused.
    async fn post(&self, path: &str, body: &Value) -> Result<Answer, String> {
        let answer = door::request(
            &self.socket,
            "POST",
            &format!("/v1/workspaces/{}/{path}", self.id),
            Some(body),
        )
        .await?;
        if !(200..300).contains(&answer.status) {
            return Err(answer.failure());
        }
        Ok(answer)
    }

    /// Ask the server whether session `id` works and how many words wait, and send its state
    /// where that changed.
    async fn refresh(&self, live: &Live, id: &str) {
        let (Some(session), Some(queued)) = (
            self.get(&format!("agent/sessions/{id}")).await,
            self.get(&format!("agent/sessions/{id}/prompts/queued"))
                .await,
        ) else {
            return;
        };
        let busy = session["is_busy"].as_bool() == Some(true);
        let waiting = queued.as_u64().unwrap_or(0) > 0;
        self.apply(live, id, |fold| fold.asked_server(busy, waiting, now()));
    }

    /// Resolves once Crush's queue for session `id` holds `words`.
    async fn queued(&self, id: &str, words: &str) {
        loop {
            if let Some(listed) = self.get(&format!("agent/sessions/{id}/prompts/list")).await
                && listed
                    .as_array()
                    .is_some_and(|listed| listed.iter().any(|queued| queued == words))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Fold one change into session `id` where the driver follows it, and send its state
    /// where that changed.
    fn apply<T>(&self, live: &Live, id: &str, change: impl FnOnce(&mut Fold) -> T) -> Option<T> {
        let (out, state) = {
            let mut followed = self.followed();
            let fold = followed.get_mut(id)?;
            let out = change(fold);
            (out, fold.changed())
        };
        if let Some(state) = state {
            live.changed(id, state);
        }
        Some(out)
    }

    /// Send session `id`'s state where it changed since last sent.
    fn send(&self, live: &Live, id: &str) {
        self.apply(live, id, |_| ());
    }

    fn taken(&self, ask: u64) {
        if let Some(take) = lock(&self.takes).remove(&ask) {
            let _ = take.send(());
        }
    }

    /// Whether a turn of session `id` runs, and whether words wait.
    fn running(&self, id: &str) -> (bool, bool) {
        self.followed().get(id).map_or((false, false), |fold| {
            (fold.began.is_some(), fold.pending == Some(true))
        })
    }

    /// The token of the turn that runs: the run id of Steery's words that began it, else when
    /// it began.
    fn token(&self, id: &str) -> Option<String> {
        let followed = self.followed();
        let fold = followed.get(id)?;
        let began = fold.began?;
        Some(fold.run.clone().unwrap_or_else(|| began.to_string()))
    }
}

/// Words the driver sent, until Crush writes their message.
struct Asked {
    ask: u64,
    words: String,
    say: Option<String>,
    run: Option<String>,
}

/// A say that a turn took: the run it went in with, and when.
struct Taken {
    say: String,
    run: Option<String>,
    at: u64,
}

/// What the server's events and answers said of one session.
#[derive(Default)]
struct Fold {
    /// When the turn that runs began, and the run id of Steery's words that runs it.
    began: Option<u64>,
    run: Option<String>,
    says: Vec<Taken>,
    /// Words sent that Crush has not written yet.
    asked: Vec<Asked>,
    pending: Option<bool>,
    /// The failure of a run that ended while the turn went on.
    failure: Option<String>,
    model: Option<String>,
    calls: Vec<LiveCall>,
    ended: VecDeque<LiveEnded>,
    settled: VecDeque<LiveSettled>,
    /// Permissions and questions open in front of the person, by the call or batch that asks.
    dialogs: Vec<(String, Dialog)>,
    sent: Option<LiveState>,
}

impl Fold {
    fn works(&self) -> bool {
        self.began.is_some() || self.pending == Some(true)
    }

    /// One message of the session as Crush writes it; the ask of the driver's words that it
    /// took, where it is their message.
    fn message(&mut self, body: &Value, created: bool, at: u64, servers: &[String]) -> Option<u64> {
        let parts = body["parts"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        match body["role"].as_str().unwrap_or_default() {
            "user" if created => {
                self.begin(at);
                let words: Vec<&str> = parts
                    .iter()
                    .filter(|part| part["type"] == "text")
                    .filter_map(|part| part["data"]["text"].as_str())
                    .collect();
                let words = words.join("\n");
                let index = self.asked.iter().position(|asked| asked.words == words)?;
                let asked = self.asked.remove(index);
                if asked.run.is_some() {
                    self.run.clone_from(&asked.run);
                }
                if let Some(say) = asked.say {
                    self.says.push(Taken {
                        say,
                        run: asked.run.or_else(|| self.run.clone()),
                        at,
                    });
                }
                Some(asked.ask)
            }
            "assistant" => {
                if let Some(model) = body["model"].as_str().filter(|model| !model.is_empty()) {
                    self.model = Some(model.to_owned());
                }
                for part in parts.iter().filter(|part| part["type"] == "tool_call") {
                    let data = &part["data"];
                    let id = data["id"].as_str().unwrap_or_default();
                    let known = self.calls.iter().any(|call| call.id == id)
                        || self.ended.iter().any(|call| call.id == id);
                    if id.is_empty() || known {
                        continue;
                    }
                    let tool = data["name"].as_str().unwrap_or_default();
                    self.calls.push(LiveCall {
                        id: id.to_owned(),
                        tool: calls::live_tool(tool, servers),
                        since: at,
                        children: Vec::new(),
                        jobs: Vec::new(),
                    });
                }
                None
            }
            "tool" => {
                for part in parts.iter().filter(|part| part["type"] == "tool_result") {
                    let id = part["data"]["tool_call_id"].as_str().unwrap_or_default();
                    if let Some(index) = self.calls.iter().position(|call| call.id == id) {
                        let call = self.calls.remove(index);
                        self.end(call.id, at);
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn permission(&mut self, body: &Value, at: u64) {
        let call = body["tool_call_id"].as_str().unwrap_or_default();
        let tool = body["tool_name"].as_str().unwrap_or("a tool");
        self.dialogs.push((
            call.to_owned(),
            Dialog {
                kind: DialogKind::Confirm,
                title: cut(&format!("Allow {tool}?"), TITLE_AT_MOST),
                since: at,
            },
        ));
    }

    fn question(&mut self, body: &Value, at: u64) {
        let batch = body["id"].as_str().unwrap_or_default();
        let title = body["questions"][0]["question"]
            .as_str()
            .or_else(|| body["confirm_title"].as_str())
            .unwrap_or("Crush asks");
        self.dialogs.push((
            batch.to_owned(),
            Dialog {
                kind: DialogKind::Select,
                title: cut(title, TITLE_AT_MOST),
                since: at,
            },
        ));
    }

    fn answered(&mut self, asking: &str) {
        self.dialogs.retain(|(open, _)| open != asking);
    }

    /// A run ended (`RunComplete`): the says that went in with it settle. A run of
    /// another's words ends with the turn, which the server's answer then says.
    fn completed(&mut self, body: &Value, at: u64) {
        let failure = body["error"]
            .as_str()
            .filter(|error| !error.is_empty() && body["cancelled"].as_bool() != Some(true))
            .map(|error| cut(error, ERROR_AT_MOST));
        let Some(run) = body["run_id"].as_str().filter(|run| !run.is_empty()) else {
            if failure.is_some() {
                self.failure = failure;
            }
            return;
        };
        if self.run.as_deref() == Some(run) {
            self.run = None;
        }
        let (ran, rest) = std::mem::take(&mut self.says)
            .into_iter()
            .partition(|taken| taken.run.as_deref() == Some(run));
        self.says = rest;
        self.settle(ran, at, failure);
    }

    /// What the server answered: whether the session works, and whether words wait. A turn
    /// that the events did not open opens; one that no longer works ends.
    fn asked_server(&mut self, busy: bool, waiting: bool, at: u64) {
        self.pending = Some(waiting);
        if busy {
            self.begin(at);
            return;
        }
        if self.began.take().is_none() {
            return;
        }
        for call in std::mem::take(&mut self.calls) {
            self.end(call.id, at);
        }
        self.dialogs.clear();
        self.run = None;
        let says = std::mem::take(&mut self.says);
        let failure = self.failure.take();
        self.settle(says, at, failure);
    }

    fn begin(&mut self, at: u64) {
        if self.began.is_none() {
            self.began = Some(at);
            self.failure = None;
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
            turn: self.began.map(|began_at| LiveTurn {
                began_at,
                says: self.says.iter().map(|taken| taken.say.clone()).collect(),
            }),
            idle: self.pending.map(|pending| self.began.is_none() && !pending),
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
        "no Crush window shows this session through Crush's server: a window runs its \
         sessions there only when Crush starts with `CRUSH_CLIENT_SERVER=1`",
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
