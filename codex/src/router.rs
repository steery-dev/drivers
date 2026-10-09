//! The Codex app's router, `<CODEX_HOME>/ipc/ipc.sock`: the door the app's own windows use
//! to hand a thread's input to the window that owns it. Frames are a 4-byte little-endian
//! length and JSON. `thread-owner-discovery` names the window that owns a thread;
//! `thread-follower-start-turn` is words typed into that thread's composer, which the app
//! runs as the next turn or steers into the one running; `thread-follower-interrupt-turn`
//! stops the turn it runs. A client of Steery's owns no thread and declines every request
//! the router passes it.
//!
//! The [`Followers`] keep a subscription to each thread the runtime asks about
//! (`thread-stream-following-changed`), so the owning window's own state of the thread
//! — running or idle — is known as it changes, and said to the runtime as
//! `session/changed`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use steery_sdk::Notifier;
use steery_sdk::wire::harness::{Execution, LiveState, LiveTurn, SessionChanged};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// The versions of its requests the Codex app speaks, read off its own bundle (ChatGPT
/// 26.917).
const VERSIONS: [(&str, u64); 4] = [
    ("initialize", 0),
    ("thread-owner-discovery", 1),
    ("thread-follower-start-turn", 2),
    ("thread-follower-interrupt-turn", 4),
];
/// How long the app has to answer one request: its own windows give owner discovery 15 s.
const WITHIN: Duration = Duration::from_secs(15);
/// How long owner discovery has to name a window before none is taken to own the thread:
/// an owner answers within a second; the router looks for ten before it says none does.
pub const OWNED_WITHIN: Duration = Duration::from_secs(3);
const FRAME_MAX: usize = 64 * 1024 * 1024;
/// What every client of Steery's tells the router it is.
const CLIENT_TYPE: &str = "steery";

/// A client of the router.
pub struct App {
    stream: UnixStream,
    client: Option<String>,
    pending: Vec<u8>,
}

/// What the router answered words for a thread with.
pub enum Took {
    /// The window took them.
    Yes,
    /// No window took them: none owns the thread now.
    Nobody,
    /// The app refused them, or may have taken them and said nothing: the refusal word
    /// and the app's own line.
    No(&'static str, String),
}

impl App {
    /// Connected and introduced: the router names this client for what it sends next.
    pub async fn open(router: &Path) -> std::io::Result<Self> {
        let mut app = Self {
            stream: UnixStream::connect(router).await?,
            client: None,
            pending: Vec::new(),
        };
        let initialized = app
            .request("initialize", json!({"clientType": CLIENT_TYPE}), None)
            .await?;
        app.client = initialized
            .pointer("/result/clientId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(app)
    }

    /// The window that owns `thread`, if one does.
    pub async fn owner(&mut self, thread: &str) -> Option<String> {
        let found = self
            .request(
                "thread-owner-discovery",
                json!({"hostId": "local", "conversationId": thread}),
                None,
            )
            .await
            .ok()?;
        found
            .get("handledByClientId")
            .and_then(Value::as_str)
            .filter(|_| found.get("resultType").and_then(Value::as_str) == Some("success"))
            .map(str::to_owned)
    }

    /// Words into `thread`, through the window that owns it, with the turn's `model` and
    /// `effort` where picked.
    pub async fn say(
        &mut self,
        thread: &str,
        words: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Took {
        let Some(owner) = self.owner(thread).await else {
            return Took::Nobody;
        };
        let mut request = json!({
            "threadId": thread,
            "input": [{"type": "text", "text": words, "text_elements": []}],
        });
        if let Some(model) = model {
            request["model"] = json!(model);
        }
        if let Some(effort) = effort {
            request["effort"] = json!(effort);
        }
        let started = self
            .request(
                "thread-follower-start-turn",
                json!({"conversationId": thread, "turnStart": {"request": request, "context": {}}}),
                Some(&owner),
            )
            .await;
        match started {
            Ok(reply) if reply.get("resultType").and_then(Value::as_str) == Some("success") => {
                Took::Yes
            }
            Ok(reply) => {
                let said = reply.get("error").cloned().unwrap_or_default().to_string();
                match refusal(&said) {
                    None => Took::Nobody,
                    Some(word) => Took::No(word, said),
                }
            }
            Err(e) => Took::No(steery_sdk::refusal::NO_ANSWER, e.to_string()),
        }
    }

    /// Interrupt the turn `expected` the window that owns `thread` runs.
    pub async fn interrupt(&mut self, thread: &str, expected: &str) -> Result<(), String> {
        let owner = self
            .owner(thread)
            .await
            .ok_or("no Codex window owns the thread")?;
        let reply = self
            .request(
                "thread-follower-interrupt-turn",
                json!({"conversationId": thread, "mode": "system", "expectedTurnId": expected}),
                Some(&owner),
            )
            .await
            .map_err(|e| e.to_string())?;
        let result = reply
            .get("result")
            .filter(|_| reply.get("resultType").and_then(Value::as_str) == Some("success"));
        match result {
            Some(result)
                if result.get("ok").and_then(Value::as_bool) == Some(true)
                    && result.get("goalPauseError").is_none_or(Value::is_null)
                    && result.get("interruptedTurnId").and_then(Value::as_str)
                        == Some(expected) =>
            {
                Ok(())
            }
            _ => Err(format!(
                "the Codex app did not interrupt {expected}: {reply}"
            )),
        }
    }

    async fn send(&mut self, message: &Value) -> std::io::Result<()> {
        let body = serde_json::to_vec(message)?;
        if body.len() > FRAME_MAX {
            return Err(std::io::Error::other("router frame exceeds its bound"));
        }
        let length = u32::try_from(body.len()).map_err(std::io::Error::other)?;
        tokio::time::timeout(Duration::from_secs(10), async {
            self.stream.write_all(&length.to_le_bytes()).await?;
            self.stream.write_all(&body).await
        })
        .await
        .map_err(|_| std::io::Error::other("router send timed out"))?
    }

    /// Decline what the router asks of a client that owns nothing.
    async fn refuse(&mut self, frame: &Value) -> std::io::Result<()> {
        match frame.get("type").and_then(Value::as_str) {
            Some("client-discovery-request") => {
                self.send(&json!({"type": "client-discovery-response",
                    "requestId": frame.get("requestId"), "response": {"canHandle": false}}))
                    .await
            }
            Some("request") => {
                self.send(
                    &json!({"type": "response", "requestId": frame.get("requestId"),
                    "resultType": "error", "error": "no-handler-for-request"}),
                )
                .await
            }
            _ => Ok(()),
        }
    }

    async fn frame(&mut self) -> std::io::Result<Value> {
        loop {
            if self.pending.len() >= 4 {
                let length =
                    u32::from_le_bytes(self.pending[..4].try_into().expect("4 bytes")) as usize;
                if length > FRAME_MAX {
                    return Err(std::io::Error::other("router frame exceeds its bound"));
                }
                if self.pending.len() >= 4 + length {
                    let frame = serde_json::from_slice(&self.pending[4..4 + length])?;
                    self.pending.drain(..4 + length);
                    return Ok(frame);
                }
            }
            let mut chunk = [0u8; 8192];
            let received = self.stream.read(&mut chunk).await?;
            if received == 0 {
                return Err(std::io::Error::other("router disconnected"));
            }
            self.pending.extend_from_slice(&chunk[..received]);
        }
    }

    /// A request, and the router's response to it; what the router asks meanwhile is
    /// declined.
    async fn request(
        &mut self,
        method: &str,
        params: Value,
        target: Option<&str>,
    ) -> std::io::Result<Value> {
        let id = uuid::Uuid::new_v4().to_string();
        let version = VERSIONS
            .iter()
            .find(|(name, _)| *name == method)
            .map_or(0, |(_, version)| *version);
        let mut message = json!({"type": "request", "requestId": id, "sourceClientId": self.client,
            "version": version, "method": method, "params": params});
        if let Some(target) = target {
            message["targetClientId"] = json!(target);
        }
        self.send(&message).await?;
        tokio::time::timeout(WITHIN, async {
            loop {
                let frame = self.frame().await?;
                match frame.get("type").and_then(Value::as_str) {
                    Some("response")
                        if frame.get("requestId").and_then(Value::as_str) == Some(&id) =>
                    {
                        return Ok(frame);
                    }
                    _ => self.refuse(&frame).await?,
                }
            }
        })
        .await
        .map_err(|_| std::io::Error::other("the Codex app did not answer in time"))?
    }
}

/// What the app's router answered words with. `no-client-found` is no window to pass them
/// to: nothing took them. A window gone or the router closing while it held them, or no
/// answer in time, may have taken them: `no-answer`. Anything else is the app refusing.
fn refusal(error: &str) -> Option<&'static str> {
    const MAY_HAVE_TAKEN: [&str; 3] = ["client-disconnected", "server-closed", "request-timeout"];
    if error.contains("no-client-found") {
        None
    } else if MAY_HAVE_TAKEN.iter().any(|code| error.contains(code)) {
        Some(steery_sdk::refusal::NO_ANSWER)
    } else {
        Some(steery_sdk::refusal::REFUSED)
    }
}

/// Whether a window of the Codex app owns `thread`, within [`OWNED_WITHIN`].
pub async fn owns(router: &Path, thread: &str) -> bool {
    tokio::time::timeout(OWNED_WITHIN, async {
        let mut app = App::open(router).await.ok()?;
        app.owner(thread).await
    })
    .await
    .ok()
    .flatten()
    .is_some()
}

/// Whether a broadcast is the window that owns the thread speaking of this machine's
/// thread: anything another client says of it is no state of the owner's.
fn of_owner(frame: &Value, owner: &str) -> bool {
    frame.get("sourceClientId").and_then(Value::as_str) == Some(owner)
        && frame.pointer("/params/hostId").and_then(Value::as_str) == Some("local")
}

/// How long a thread nobody asked about is still followed.
const FOLLOWER_IDLE: Duration = Duration::from_secs(60);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(10);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const DISCOVERY_LIMIT: usize = 8;
/// How many threads are followed at once.
const FOLLOWED_MAX: usize = 256;

/// The threads the runtime asked about, each followed through the window that owns it.
pub struct Followers {
    router: Option<PathBuf>,
    notifier: Notifier,
    threads: Mutex<HashMap<String, Follow>>,
    started: Mutex<bool>,
}

struct Follow {
    requested: Instant,
    owner: Option<String>,
    projection: Projection,
    last_discover: Instant,
    /// What was last said of it, and since when it ran.
    said: Option<(Execution, u64)>,
}

impl Follow {
    fn new() -> Self {
        Self {
            requested: Instant::now(),
            owner: None,
            projection: Projection::default(),
            last_discover: Instant::now() - DISCOVERY_INTERVAL,
            said: None,
        }
    }

    fn forget(&mut self) {
        self.owner = None;
        self.projection = Projection::default();
        self.last_discover = Instant::now() - DISCOVERY_INTERVAL;
    }
}

impl Followers {
    pub fn new(router: Option<PathBuf>, notifier: Notifier) -> Arc<Self> {
        Arc::new(Self {
            router,
            notifier,
            threads: Mutex::new(HashMap::new()),
            started: Mutex::new(false),
        })
    }

    fn threads(&self) -> std::sync::MutexGuard<'_, HashMap<String, Follow>> {
        self.threads.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Follow `thread` from now, and start following at all on first use.
    fn follow(self: &Arc<Self>, thread: &str) {
        if self.router.is_none() {
            return;
        }
        {
            let mut threads = self.threads();
            if !threads.contains_key(thread) && threads.len() >= FOLLOWED_MAX {
                return;
            }
            threads
                .entry(thread.to_owned())
                .or_insert_with(Follow::new)
                .requested = Instant::now();
        }
        let mut started = self.started.lock().unwrap_or_else(|e| e.into_inner());
        if !*started {
            *started = true;
            tokio::spawn(Arc::clone(self).run());
        }
    }

    /// The thread's execution as the window that owns it reports it.
    pub fn execution(self: &Arc<Self>, thread: &str) -> Option<Execution> {
        self.follow(thread);
        let threads = self.threads();
        let follow = threads.get(thread)?;
        follow.owner.as_ref()?;
        follow.projection.execution()
    }

    /// The router's path, where the app's router is here.
    pub fn router(&self) -> Option<&Path> {
        self.router.as_deref().filter(|path| path.exists())
    }

    /// Say a thread's state where it changed.
    fn changed(&self, thread: &str) {
        let state = {
            let mut threads = self.threads();
            let Some(follow) = threads.get_mut(thread) else {
                return;
            };
            let now = follow
                .owner
                .as_ref()
                .and_then(|_| follow.projection.execution());
            let before = follow.said.map(|(state, _)| state);
            if now == before {
                return;
            }
            let since = match (now, follow.said) {
                (Some(Execution::Running), Some((Execution::Running, since))) => since,
                _ => crate::home::now(),
            };
            follow.said = now.map(|state| (state, since));
            match now {
                Some(Execution::Running) => LiveState {
                    turn: Some(LiveTurn {
                        began_at: since,
                        says: Vec::new(),
                    }),
                    idle: Some(false),
                    ..LiveState::default()
                },
                Some(_) => LiveState {
                    idle: Some(true),
                    ..LiveState::default()
                },
                None => LiveState::default(),
            }
        };
        if let Ok(params) = serde_json::to_value(SessionChanged {
            external_id: thread.to_owned(),
            state,
        }) {
            self.notifier
                .notify(steery_sdk::method::SESSION_CHANGED, params);
        }
    }

    fn clear(&self) {
        let threads: Vec<String> = {
            let mut followed = self.threads();
            for follow in followed.values_mut() {
                follow.forget();
            }
            followed.keys().cloned().collect()
        };
        for thread in threads {
            self.changed(&thread);
        }
    }

    async fn run(self: Arc<Self>) {
        loop {
            let Some(path) = self.router().map(Path::to_path_buf) else {
                self.clear();
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            };
            let Ok(mut app) = App::open(&path).await else {
                self.clear();
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            };
            self.clear();
            let mut pending: HashMap<String, (String, Instant)> = HashMap::new();
            let mut ticker = tokio::time::interval(Duration::from_millis(100));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let result = tokio::select! {
                    frame = app.frame() => match frame {
                        Ok(frame) => self.handle(&mut app, &frame, &mut pending).await,
                        Err(error) => Err(error),
                    },
                    _ = ticker.tick() => self.tick(&mut app, &mut pending).await,
                };
                if result.is_err() {
                    break;
                }
            }
            self.clear();
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    async fn subscribe(
        app: &mut App,
        thread: &str,
        owner: &str,
        following: bool,
    ) -> std::io::Result<()> {
        let client = app.client.clone();
        app.send(
            &json!({"type": "broadcast", "sourceClientId": client, "targetClientIds": [owner],
            "version": 1, "method": "thread-stream-following-changed",
            "params": {"conversationId": thread, "hostId": "local", "following": following}}),
        )
        .await
    }

    async fn tick(
        &self,
        app: &mut App,
        pending: &mut HashMap<String, (String, Instant)>,
    ) -> std::io::Result<()> {
        let now = Instant::now();
        let mut lapsed = Vec::new();
        pending.retain(|_, (thread, sent)| {
            let keep = now.duration_since(*sent) < DISCOVERY_TIMEOUT;
            if !keep {
                lapsed.push(thread.clone());
            }
            keep
        });
        for thread in lapsed {
            if let Some(follow) = self.threads().get_mut(&thread) {
                follow.owner = None;
                follow.projection = Projection::default();
            }
            self.changed(&thread);
        }
        let retired: Vec<(String, String)> = {
            let mut threads = self.threads();
            let mut retired = Vec::new();
            threads.retain(|thread, follow| {
                let keep = now.duration_since(follow.requested) < FOLLOWER_IDLE;
                if !keep && let Some(owner) = &follow.owner {
                    retired.push((thread.clone(), owner.clone()));
                }
                keep
            });
            retired
        };
        for (thread, owner) in retired {
            Self::subscribe(app, &thread, &owner, false).await?;
        }
        while pending.len() < DISCOVERY_LIMIT {
            let candidate = {
                let mut threads = self.threads();
                let thread = threads
                    .iter()
                    .filter(|(thread, follow)| {
                        !pending.values().any(|(waiting, _)| waiting == *thread)
                            && now.duration_since(follow.last_discover) >= DISCOVERY_INTERVAL
                    })
                    .min_by_key(|(_, follow)| follow.last_discover)
                    .map(|(thread, _)| thread.clone());
                if let Some(thread) = &thread
                    && let Some(follow) = threads.get_mut(thread)
                {
                    follow.last_discover = now;
                }
                thread
            };
            let Some(thread) = candidate else { break };
            let id = uuid::Uuid::new_v4().to_string();
            let client = app.client.clone();
            app.send(
                &json!({"type": "request", "requestId": id, "sourceClientId": client,
                "version": 1, "method": "thread-owner-discovery",
                "params": {"hostId": "local", "conversationId": thread}}),
            )
            .await?;
            pending.insert(id, (thread, now));
        }
        Ok(())
    }

    async fn handle(
        &self,
        app: &mut App,
        frame: &Value,
        pending: &mut HashMap<String, (String, Instant)>,
    ) -> std::io::Result<()> {
        let method = frame.get("method").and_then(Value::as_str);
        if method == Some("ipc-connection-reset") {
            pending.clear();
            self.clear();
        } else if method == Some("client-status-changed")
            && frame.pointer("/params/status").and_then(Value::as_str) == Some("disconnected")
        {
            if let Some(gone) = frame.pointer("/params/clientId").and_then(Value::as_str) {
                let lost: Vec<String> = {
                    let mut threads = self.threads();
                    threads
                        .iter_mut()
                        .filter(|(_, follow)| follow.owner.as_deref() == Some(gone))
                        .map(|(thread, follow)| {
                            follow.forget();
                            thread.clone()
                        })
                        .collect()
                };
                for thread in lost {
                    pending.retain(|_, (waiting, _)| *waiting != thread);
                    self.changed(&thread);
                }
            }
        } else if method == Some("client-status-changed")
            && frame.pointer("/params/clientType").and_then(Value::as_str) != Some(CLIENT_TYPE)
        {
            // A window that joined may own a thread nobody owned: each is asked again.
            for follow in self.threads().values_mut() {
                if follow.owner.is_none() {
                    follow.last_discover = Instant::now() - DISCOVERY_INTERVAL;
                }
            }
        } else if method == Some("thread-stream-following-status-requested") {
            let requester = frame.get("sourceClientId").and_then(Value::as_str);
            let thread = frame
                .pointer("/params/conversationId")
                .and_then(Value::as_str);
            let again: Vec<(String, String)> = {
                let mut threads = self.threads();
                threads
                    .iter_mut()
                    .filter_map(|(name, follow)| {
                        let owner = follow.owner.clone()?;
                        (thread.is_none_or(|t| t == name) && requester.is_none_or(|r| r == owner))
                            .then(|| {
                                follow.projection = Projection::default();
                                (name.clone(), owner)
                            })
                    })
                    .collect()
            };
            for (thread, owner) in again {
                Self::subscribe(app, &thread, &owner, true).await?;
            }
        } else if frame.get("type").and_then(Value::as_str) == Some("response")
            && let Some(id) = frame.get("requestId").and_then(Value::as_str)
            && let Some((thread, _)) = pending.remove(id)
        {
            let owner = frame
                .get("handledByClientId")
                .and_then(Value::as_str)
                .filter(|_| frame.get("resultType").and_then(Value::as_str) == Some("success"))
                .map(str::to_owned);
            let change = {
                let mut threads = self.threads();
                threads.get_mut(&thread).map(|follow| {
                    let previous = follow.owner.clone();
                    if previous != owner {
                        follow.projection = Projection::default();
                        follow.owner = owner.clone();
                    }
                    previous
                })
            };
            if let Some(previous) = change
                && previous != owner
            {
                if let Some(previous) = previous {
                    Self::subscribe(app, &thread, &previous, false).await?;
                }
                if let Some(owner) = &owner {
                    Self::subscribe(app, &thread, owner, true).await?;
                }
                self.changed(&thread);
            }
        } else if frame.get("type").and_then(Value::as_str) == Some("broadcast")
            && method == Some("thread-stream-state-changed")
            && let Some(thread) = frame
                .pointer("/params/conversationId")
                .and_then(Value::as_str)
        {
            let resubscribe = {
                let mut threads = self.threads();
                match threads.get_mut(thread) {
                    Some(follow) => match follow.owner.clone() {
                        Some(owner) if of_owner(frame, &owner) => {
                            if frame.get("version").and_then(Value::as_u64) != Some(11) {
                                follow.projection = Projection::default();
                                None
                            } else if follow.projection.change(&frame["params"]["change"]) {
                                None
                            } else {
                                Some(owner)
                            }
                        }
                        _ => None,
                    },
                    None => None,
                }
            };
            if let Some(owner) = resubscribe {
                Self::subscribe(app, thread, &owner, false).await?;
                Self::subscribe(app, thread, &owner, true).await?;
            }
            self.changed(thread);
        }
        app.refuse(frame).await
    }
}

/// The owning window's own state of a thread, patched as it changes.
#[derive(Default)]
struct Projection {
    revision: Option<u64>,
    runtime: Option<String>,
    history_valid: bool,
    latest: Option<String>,
    status: Option<String>,
}

impl Projection {
    fn execution(&self) -> Option<Execution> {
        self.revision?;
        if self.runtime.as_deref() == Some("active") || self.status.as_deref() == Some("inProgress")
        {
            Some(Execution::Running)
        } else if self.runtime.as_deref() == Some("idle")
            && self.history_valid
            && (self.latest.is_none()
                || matches!(
                    self.status.as_deref(),
                    Some("completed" | "failed" | "interrupted" | "cancelled")
                ))
        {
            Some(match self.status.as_deref() {
                Some("failed") => Execution::Failed,
                Some("interrupted" | "cancelled") => Execution::Interrupted,
                _ => Execution::Finished,
            })
        } else {
            None
        }
    }

    /// Apply one `thread-stream-state-changed` change; false where it could not be read,
    /// which leaves nothing known.
    fn change(&mut self, change: &Value) -> bool {
        let Some(revision) = change.get("revision").and_then(Value::as_u64) else {
            *self = Self::default();
            return false;
        };
        match change.get("type").and_then(Value::as_str) {
            Some("snapshot") => {
                let Some(state) = change.get("conversationState") else {
                    *self = Self::default();
                    return false;
                };
                self.revision = Some(revision);
                self.runtime = state
                    .pointer("/threadRuntimeStatus/type")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let history = (state.pointer("/turnHistory/kind").and_then(Value::as_str)
                    == Some("canonical"))
                .then(|| state.pointer("/turnHistory/history"))
                .flatten();
                let newest = history
                    .and_then(|history| history.get("islands"))
                    .and_then(Value::as_array)
                    .and_then(|islands| islands.last());
                self.history_valid = newest.is_some_and(|island| {
                    island
                        .pointer("/newerBoundary/status")
                        .and_then(Value::as_str)
                        == Some("exhausted")
                        && island
                            .get("entries")
                            .and_then(Value::as_array)
                            .is_some_and(|entries| {
                                entries.last().is_none_or(|entry| {
                                    entry.get("value").and_then(Value::as_str).is_some()
                                })
                            })
                }) && history
                    .and_then(|history| history.get("entitiesByKey"))
                    .and_then(Value::as_object)
                    .is_some();
                self.latest = newest
                    .and_then(|island| island.get("entries"))
                    .and_then(Value::as_array)
                    .and_then(|entries| entries.last())
                    .and_then(|entry| entry.get("value"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.status = self.latest.as_ref().and_then(|key| {
                    history?
                        .get("entitiesByKey")?
                        .get(key)?
                        .get("status")?
                        .as_str()
                        .map(str::to_owned)
                });
                true
            }
            Some("patches")
                if self.revision == change.get("baseRevision").and_then(Value::as_u64) =>
            {
                let Some(patches) = change.get("patches").and_then(Value::as_array) else {
                    *self = Self::default();
                    return false;
                };
                for patch in patches {
                    let op = patch.get("op").and_then(Value::as_str);
                    let Some(path) = patch.get("path").and_then(Value::as_array) else {
                        *self = Self::default();
                        return false;
                    };
                    if !matches!(op, Some("add" | "replace" | "remove")) {
                        *self = Self::default();
                        return false;
                    }
                    let path: Vec<&str> = path
                        .iter()
                        .map(|part| part.as_str().unwrap_or(""))
                        .collect();
                    if path.is_empty() {
                        *self = Self::default();
                        return false;
                    }
                    let remove = op == Some("remove");
                    let value = patch.get("value");
                    if path[0] == "turnHistory"
                        && (path.len() < 5
                            || (path.get(1) == Some(&"history")
                                && path.get(2) == Some(&"islands")
                                && path.get(4) != Some(&"entries")))
                    {
                        *self = Self::default();
                        return false;
                    }
                    if path.as_slice() == ["threadRuntimeStatus"]
                        || path.as_slice() == ["threadRuntimeStatus", "type"]
                    {
                        self.runtime = if remove {
                            None
                        } else {
                            value
                                .and_then(|value| value.get("type").or(Some(value)))
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        };
                    } else if path.len() >= 4
                        && path[0..3] == ["turnHistory", "history", "entitiesByKey"]
                    {
                        if self.latest.as_deref() == Some(path[3])
                            && (path.len() == 4 || path.get(4) == Some(&"status"))
                        {
                            self.status = if remove {
                                None
                            } else {
                                value
                                    .and_then(|value| value.get("status").or(Some(value)))
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                            };
                        }
                    } else if path.len() >= 5
                        && path[0..3] == ["turnHistory", "history", "islands"]
                        && path[4] == "entries"
                    {
                        if remove || path.len() != 6 || op != Some("add") {
                            *self = Self::default();
                            return false;
                        }
                        let Some(key) = value
                            .and_then(|value| value.get("value"))
                            .and_then(Value::as_str)
                        else {
                            *self = Self::default();
                            return false;
                        };
                        self.latest = Some(key.to_owned());
                        self.status = None;
                        self.runtime = None;
                    }
                }
                self.revision = Some(revision);
                true
            }
            _ => {
                *self = Self::default();
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(runtime: &str, turn: &str) -> Value {
        json!({"type":"snapshot","revision":7,"conversationState":{
            "threadRuntimeStatus":{"type":runtime},
            "turnHistory":{"kind":"canonical","history":{
                "islands":[{"entries":[{"key":"turn:older","value":"turn:older"},{"key":"turn:newest","value":"turn:newest"}],"newerBoundary":{"status":"exhausted"}}],
                "entitiesByKey":{"turn:newest":{"status":turn}}
            }}
        }})
    }

    /// The window's own state of a thread: running while a turn is in progress, ended as
    /// its newest turn ended once the thread is idle, unknown where its history is not
    /// whole, and patched as it changes.
    #[test]
    fn a_window_s_state_is_read_from_its_snapshot_and_patches() {
        let mut projection = Projection::default();
        assert!(projection.change(&snapshot("active", "inProgress")));
        assert_eq!(projection.execution(), Some(Execution::Running));
        assert!(projection.change(&snapshot("idle", "completed")));
        assert_eq!(projection.execution(), Some(Execution::Finished));
        assert!(projection.change(&snapshot("idle", "failed")));
        assert_eq!(projection.execution(), Some(Execution::Failed));

        assert!(projection.change(&json!({"type":"patches","revision":8,"baseRevision":7,"patches":[
            {"op":"add","path":["turnHistory","history","islands","0","entries","2"],"value":{"key":"turn:next","value":"turn:next"}},
            {"op":"replace","path":["threadRuntimeStatus"],"value":{"type":"active"}}
        ]})));
        assert_eq!(projection.execution(), Some(Execution::Running));
        // A patch on another base is out of step: nothing is known until a snapshot.
        assert!(
            !projection
                .change(&json!({"type":"patches","revision":10,"baseRevision":3,"patches":[]}))
        );
        assert_eq!(projection.execution(), None);

        let mut partial = snapshot("idle", "completed");
        partial["conversationState"]["turnHistory"]["history"]["islands"][0]["newerBoundary"]["status"] =
            json!("more");
        assert!(projection.change(&partial));
        assert_eq!(
            projection.execution(),
            None,
            "a history not whole proves nothing"
        );
    }

    #[test]
    fn the_router_s_refusals_are_read_by_their_code() {
        assert_eq!(refusal("\"no-client-found\""), None);
        assert_eq!(
            refusal("\"client-disconnected\""),
            Some(steery_sdk::refusal::NO_ANSWER)
        );
        assert_eq!(
            refusal("\"thread-not-loaded\""),
            Some(steery_sdk::refusal::REFUSED)
        );
    }

    #[test]
    fn owner_projection_requires_ordered_patches_and_explicit_idle() {
        let mut state = Projection::default();
        assert!(state.change(&snapshot("active", "inProgress")));
        assert_eq!(state.execution(), Some(Execution::Running));
        assert_eq!(state.latest.as_deref(), Some("turn:newest"));
        assert!(state.change(&json!({"type":"patches","baseRevision":7,"revision":8,"patches":[
            {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:newest","status"],"value":"completed"},
            {"op":"replace","path":["threadRuntimeStatus","type"],"value":"idle"}
        ]})));
        assert_eq!(state.execution(), Some(Execution::Finished));
        assert!(
            !state.change(&json!({"type":"patches","baseRevision":7,"revision":9,"patches":[]}))
        );
        assert_eq!(state.execution(), None);
    }

    #[test]
    fn new_turn_and_unsupported_history_patch_revoke_idle() {
        let mut state = Projection::default();
        assert!(state.change(&snapshot("idle", "completed")));
        assert_eq!(state.execution(), Some(Execution::Finished));
        assert!(state.change(
            &json!({"type":"patches","baseRevision":7,"revision":8,"patches":[
                {"op":"add","path":["turnHistory","history","islands",0,"entries","-"],
                 "value":{"key":"turn:next","value":"turn:next"}}
            ]})
        ));
        assert_eq!(state.execution(), None);
        assert!(!state.change(
            &json!({"type":"patches","baseRevision":8,"revision":9,"patches":[
                {"op":"replace","path":["turnHistory","history","islands",0,"entries"],
                 "value":[{"key":"turn:newest"}]}
            ]})
        ));
        assert_eq!(state.execution(), None);
        assert!(state.change(&snapshot("idle", "completed")));
        assert!(!state.change(
            &json!({"type":"patches","baseRevision":7,"revision":8,"patches":[
                {"op":"move","path":["threadRuntimeStatus"],"value":{"type":"active"}}
            ]})
        ));
        assert_eq!(state.execution(), None);
    }

    #[test]
    fn idle_requires_complete_history_and_known_latest_status() {
        let mut state = Projection::default();
        let idle = |revision: u64, history: Option<Value>| {
            let mut change = json!({"type":"snapshot","revision":revision,"conversationState":{
                "threadRuntimeStatus":{"type":"idle"}
            }});
            if let Some(history) = history {
                change["conversationState"]["turnHistory"] =
                    json!({"kind":"canonical","history":history});
            }
            change
        };
        assert!(state.change(&idle(1, None)));
        assert_eq!(state.execution(), None);
        assert!(state.change(&idle(
            2,
            Some(
                json!({"islands":[{"entries":[{"key":"turn:new","value":"turn:new"}],"newerBoundary":{"status":"exhausted"}}],"entitiesByKey":{}})
            )
        )));
        assert_eq!(state.execution(), None);
        assert!(state.change(&idle(3, Some(json!({"islands":[],"entitiesByKey":{}})))));
        assert_eq!(state.execution(), None);
        assert!(state.change(&idle(
            4,
            Some(
                json!({"islands":[{"entries":[],"newerBoundary":{"status":"exhausted"}}],"entitiesByKey":{}})
            )
        )));
        assert_eq!(state.execution(), Some(Execution::Finished));
    }

    #[test]
    fn partial_newest_boundary_and_local_entry_never_borrow_older_idle() {
        let mut state = Projection::default();
        let mut partial = snapshot("idle", "completed");
        partial["conversationState"]["turnHistory"]["history"]["islands"][0]["newerBoundary"] =
            json!({"status":"available","boundaryId":"tail:0:newer","handle":"next"});
        assert!(state.change(&partial));
        assert_eq!(state.execution(), None);

        let mut missing = snapshot("idle", "completed");
        missing["conversationState"]["turnHistory"]["history"]["islands"][0]
            .as_object_mut()
            .unwrap()
            .remove("newerBoundary");
        assert!(state.change(&missing));
        assert_eq!(state.execution(), None);

        let mut legacy = snapshot("idle", "completed");
        legacy["conversationState"]["turnHistory"]["kind"] = json!("legacy");
        assert!(state.change(&legacy));
        assert_eq!(state.execution(), None);

        let mut malformed = snapshot("idle", "completed");
        malformed["conversationState"]["turnHistory"]["history"]["islands"][0]["entries"] =
            json!([{"key":"turn:newest"}]);
        assert!(state.change(&malformed));
        assert_eq!(state.execution(), None);

        let mut local = snapshot("idle", "completed");
        local["conversationState"]["turnHistory"]["history"]["islands"][0]["entries"]
            .as_array_mut()
            .unwrap()
            .push(json!({"key":"tail:0:local:1","value":"tail:0:local:1"}));
        local["conversationState"]["turnHistory"]["history"]["entitiesByKey"]["tail:0:local:1"] =
            json!({"status":"inProgress"});
        assert!(state.change(&local));
        assert_eq!(state.latest.as_deref(), Some("tail:0:local:1"));
        assert_eq!(state.execution(), Some(Execution::Running));
    }

    #[test]
    fn only_the_exact_owner_of_this_machine_s_thread_says_its_state() {
        let frame = |owner: &str, host: &str| {
            json!({"type":"broadcast","sourceClientId":owner,"version":11,
                "method":"thread-stream-state-changed",
                "params":{"conversationId":"thread","hostId":host,"change":snapshot("active","inProgress")}})
        };
        assert!(of_owner(&frame("owner", "local"), "owner"));
        assert!(!of_owner(&frame("wrong", "local"), "owner"));
        assert!(!of_owner(&frame("owner", "remote"), "owner"));
    }

    /// A router that introduces its client, names `owner-1` as every thread's owner, and
    /// answers each follower request with the next of `replies`; the frames it was sent.
    async fn router(replies: Vec<Value>) -> (PathBuf, tokio::task::JoinHandle<Vec<Value>>) {
        let dir = std::env::temp_dir().join(format!(
            "sr{}",
            &uuid::Uuid::now_v7().simple().to_string()[24..]
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("ipc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut replies = replies.into_iter();
            let mut frames = Vec::new();
            loop {
                let mut length = [0u8; 4];
                if stream.read_exact(&mut length).await.is_err() {
                    break;
                }
                let mut body = vec![0u8; u32::from_le_bytes(length) as usize];
                stream.read_exact(&mut body).await.unwrap();
                let frame: Value = serde_json::from_slice(&body).unwrap();
                let id = frame["requestId"].clone();
                let mut answer = match frame["method"].as_str() {
                    Some("initialize") => {
                        json!({"resultType":"success","result":{"clientId":"steery-1"}})
                    }
                    Some("thread-owner-discovery") => {
                        json!({"resultType":"success","handledByClientId":"owner-1"})
                    }
                    _ => replies.next().expect("a reply for every request"),
                };
                answer["type"] = json!("response");
                answer["requestId"] = id;
                frames.push(frame);
                let body = serde_json::to_vec(&answer).unwrap();
                stream
                    .write_all(&(body.len() as u32).to_le_bytes())
                    .await
                    .unwrap();
                stream.write_all(&body).await.unwrap();
            }
            std::fs::remove_dir_all(dir).ok();
            frames
        });
        (socket, served)
    }

    /// An interrupt goes to the window that owns the thread, in the version the app
    /// speaks, and stands only where the app says it interrupted exactly that turn.
    #[tokio::test]
    async fn an_interrupt_stands_only_for_the_turn_it_named() {
        let ok = |result: Value| json!({"resultType":"success","result":result});
        let (socket, served) = router(vec![
            ok(json!({"interruptedTurnId":"t1","ok":true})),
            ok(json!({"interruptedTurnId":null,"ok":true})),
            ok(json!({"interruptedTurnId":"t2","ok":true})),
            ok(json!({"interruptedTurnId":"t1","ok":true,"goalPauseError":"failed"})),
            json!({"resultType":"error","error":"not-owner"}),
        ])
        .await;
        let mut app = App::open(&socket).await.unwrap();
        assert_eq!(app.interrupt("thread-1", "t1").await, Ok(()));
        for _ in 0..4 {
            assert!(app.interrupt("thread-1", "t1").await.is_err());
        }
        drop(app);
        let frames = served.await.unwrap();
        let interrupt = frames
            .iter()
            .find(|frame| frame["method"] == "thread-follower-interrupt-turn")
            .unwrap();
        assert_eq!(interrupt["version"], 4);
        assert_eq!(interrupt["sourceClientId"], "steery-1");
        assert_eq!(interrupt["targetClientId"], "owner-1");
        assert_eq!(interrupt["params"]["conversationId"], "thread-1");
        assert_eq!(interrupt["params"]["expectedTurnId"], "t1");
    }

    /// Words go to the owning window as its composer's next turn, on what its person
    /// picked, and nothing of a pick where none was.
    #[tokio::test]
    async fn words_start_the_window_s_turn_on_what_its_person_picked() {
        let ok = json!({"resultType":"success","result":{}});
        let (socket, served) = router(vec![ok.clone(), ok]).await;
        let mut app = App::open(&socket).await.unwrap();
        assert!(matches!(
            app.say("thread-1", "go", Some("gpt-6-sol"), Some("xhigh"))
                .await,
            Took::Yes
        ));
        assert!(matches!(
            app.say("thread-1", "go", None, None).await,
            Took::Yes
        ));
        drop(app);
        let frames = served.await.unwrap();
        let started: Vec<&Value> = frames
            .iter()
            .filter(|frame| frame["method"] == "thread-follower-start-turn")
            .map(|frame| &frame["params"]["turnStart"]["request"])
            .collect();
        assert_eq!(started.len(), 2);
        assert_eq!(
            (&started[0]["model"], &started[0]["effort"]),
            (&json!("gpt-6-sol"), &json!("xhigh"))
        );
        assert!(started[1].get("model").is_none() && started[1].get("effort").is_none());
        assert_eq!(started[0]["threadId"], "thread-1");
    }
}
