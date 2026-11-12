//! A turn kept past its words: the copy of a conversation behind Steery's plugin door
//! (PROTOCOL.md §4.4), for a harness that loads no plugin of Steery's. The runtime starts it
//! held (`Stdin::Held`). It opens the conversation in the harness's ACP server, dials the
//! door as that conversation's copy (`mode: rpc`, its own pid), and stays until the runtime
//! ends it. Every word comes through the door.
//!
//! The harness runs one prompt of a session at a time, and a second prompt aborts the one
//! that runs. So words said while a turn runs, `steer` or `queue`, are held here
//! (`{queued_at}`), and go in together as the next prompt once that turn ends. The copy
//! keeps no records of its own: it answers a welcome that asks for entries with `lost`,
//! and the runtime reads the harness's transcript. It sends no `live` frames, as the
//! harness's ACP names no message that its records name again. It runs no compaction of
//! Steery's: it answers `compact` with what it would keep. Steery's own hooks of the harness
//! make a turn go on at its end, so the copy sends no `settle`.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use steery_sdk::refusal;
use steery_sdk::wire::door::{
    self, Ask, DoorCompacted, DoorMode, DoorSaid, DoorStopped, PluginFrame, RuntimeFrame,
};
use steery_sdk::wire::harness::{LiveCall, LiveEnded, LiveSettled, LiveState, LiveTurn, Picked};
use tokio::net::TcpStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use super::{Acp, Asked, CANCELLED, CANCELLED_WITHIN, Host, abandon, ask, open, rejected, word};

/// How long a stop waits for the turn to end before it answers `no-answer`.
const STOP_WITHIN: Duration = Duration::from_millis(2500);
/// How long a dial and its handshake, and then the runtime's welcome, may take.
const DIAL_WITHIN: Duration = Duration::from_secs(5);
/// The shortest time between two `state` frames.
const STATE_EVERY: Duration = Duration::from_millis(100);
/// The first wait before a redial after a drop, and the longest.
const REDIAL_FIRST: Duration = Duration::from_millis(250);
const REDIAL_MOST: Duration = Duration::from_secs(2);
/// The wait before a redial to a runtime that does not speak the door yet (4409).
const REDIAL_OLD: Duration = Duration::from_secs(5);
/// How many answers the copy keeps to answer an ask asked again.
const ANSWERS_KEPT: usize = 256;
/// How many settled turns and ended calls the state carries.
const SETTLED_KEPT: usize = 16;
const ENDED_KEPT: usize = 64;

/// Keep the turn: open the conversation, be its copy behind the door until the runtime
/// signals the end; the error is the line for standard error.
pub async fn keep(host: &impl Host, asked: Asked) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("{}: {e}", refusal::START_FAILED);
    let url = asked.door.clone().unwrap_or_default();
    let cwd = std::env::current_dir().map_err(failed)?;
    let mut terms = signal(SignalKind::terminate()).map_err(failed)?;
    let mut interrupts = signal(SignalKind::interrupt()).map_err(failed)?;
    let mut acp = Acp::open(host, &asked.binary).await.map_err(failed)?;
    let outcome = async {
        let (session, opened) = open(host, &asked, &mut acp, &cwd).await?;
        let opened_at = super::drive::now();
        let began = host.began(&session).unwrap_or(opened_at);
        if let Some(file) = std::env::var_os(door::COMPACT_VAR) {
            compacted(Path::new(&file), began);
        }
        let (out, outgoing) = mpsc::unbounded_channel();
        let (state, watched) = watch::channel(LiveState::default());
        let (told, mut heard) = mpsc::unbounded_channel();
        let mut copy = Copy::new(
            host,
            session,
            asked.hook.clone(),
            &opened,
            began,
            out,
            state,
        );
        copy.publish();
        let hello = Hello {
            harness: host.harness().id().to_owned(),
            session: copy.session.clone(),
            cwd: cwd.clone(),
        };
        let linked = tokio::spawn(link(url, hello, watched, outgoing, told));
        // The permissions that wait on the hook, by their request ids.
        let mut asking = JoinSet::new();
        let mut waiting: Vec<Value> = Vec::new();
        let mut ending: Option<Instant> = None;
        let result = loop {
            if ending.is_some() && copy.turn.is_none() {
                break Ok(());
            }
            for request in std::mem::take(&mut acp.asks) {
                let Some(id) = request.get("id").cloned() else {
                    continue;
                };
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                let cancelled = copy.turn.as_ref().is_some_and(|turn| turn.cancelled);
                match (&copy.hook, cancelled) {
                    (_, true) => acp.answer(&id, CANCELLED.clone()).await?,
                    (Some(hook), false) => {
                        waiting.push(id.clone());
                        asking.spawn(ask(hook.clone(), id, params));
                    }
                    (None, false) => {
                        let rejection = json!({"outcome": rejected(params.get("options"))});
                        acp.answer(&id, rejection).await?;
                    }
                }
            }
            let stopping = copy.stops.iter().map(|(_, at)| *at).min();
            let stops = async {
                match stopping {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            let ended = async {
                match ending {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                message = acp.next() => {
                    if let Some(message) = message? {
                        let cancelled = copy.message(&mut acp, message).await?;
                        if cancelled {
                            abandon(&mut acp, &mut asking, &mut waiting).await?;
                        }
                    }
                }
                Some(Ok((id, answer))) = asking.join_next() => {
                    waiting.retain(|waits| *waits != id);
                    acp.answer(&id, answer).await?;
                }
                heard = heard.recv() => match heard {
                    Some(Heard::Ask(asked)) => {
                        if copy.ask(&mut acp, asked).await? {
                            abandon(&mut acp, &mut asking, &mut waiting).await?;
                        }
                    }
                    Some(Heard::Gone(why)) => {
                        break Err(format!("{}: the door closed: {why}", refusal::REFUSED));
                    }
                    None => break Err(format!("{}: the door's link ended", refusal::REFUSED)),
                },
                () = stops => copy.unstopped(),
                _ = terms.recv() => match ending {
                    Some(_) => break Ok(()),
                    None => {
                        ending = Some(Instant::now() + CANCELLED_WITHIN);
                        if copy.end(&mut acp).await? {
                            abandon(&mut acp, &mut asking, &mut waiting).await?;
                        }
                    }
                },
                _ = interrupts.recv() => match ending {
                    Some(_) => break Ok(()),
                    None => {
                        ending = Some(Instant::now() + CANCELLED_WITHIN);
                        if copy.end(&mut acp).await? {
                            abandon(&mut acp, &mut asking, &mut waiting).await?;
                        }
                    }
                },
                () = ended => break Ok(()),
            }
        };
        // Dropping the copy drops the link's input, which closes the door with 1000.
        drop(copy);
        let _ = tokio::time::timeout(DIAL_WITHIN, linked).await;
        result
    }
    .await;
    acp.close().await;
    outcome
}

/// Answer the `compact` ask in `file` as the door would, before any words go in: the
/// harness runs no compaction of Steery's, so the context starts where it did. The answer
/// frame goes to `compacted.json` beside the ask, 0600; an ask that does not read gets none.
fn compacted(file: &Path, began: u64) {
    let Some(asked) = std::fs::read(file)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return;
    };
    let Some(id) = asked.get("id").and_then(Value::as_str).map(str::to_owned) else {
        return;
    };
    let answer = match serde_json::from_value::<RuntimeFrame>(asked) {
        Ok(RuntimeFrame::Ask(Ask::Compact { .. })) => PluginFrame::Answer {
            id,
            ok: serde_json::to_value(DoorCompacted {
                from: began,
                forgot: 0,
            })
            .ok(),
            refused: None,
            error: None,
        },
        _ => PluginFrame::Answer {
            id,
            ok: None,
            refused: Some(refusal::REFUSED.into()),
            error: Some("the ask is not a compact this copy reads".into()),
        },
    };
    let Some(dir) = file.parent() else {
        return;
    };
    let Ok(bytes) = serde_json::to_vec(&answer) else {
        return;
    };
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dir.join(door::COMPACTED_FILE))
        .and_then(|mut written| written.write_all(&bytes));
}

/// The turn that runs: its prompt's id, when it began, and the says whose words began it.
struct Turn {
    prompt: u64,
    began_at: u64,
    says: Vec<String>,
    /// The copy asked the harness to cancel it.
    cancelled: bool,
}

/// What became of a say's words.
#[derive(Clone, Copy)]
enum Said {
    /// Held here since then, for the next prompt.
    Held(u64),
    /// Held since the first time, and taken into a prompt at the second.
    Taken(u64, u64),
    /// Let go unseen: the conversation was paused, or the turn ended, before they went in.
    Dropped,
}

/// A refusal: the word, and the harness's or the copy's own line.
type Refusal = (&'static str, String);

/// The conversation's copy: what runs, what waits, and what the door was answered.
struct Copy<'h, H: Host> {
    host: &'h H,
    session: String,
    hook: Option<String>,
    /// The models the harness offered when it opened the session; empty where it named none.
    models: Vec<String>,
    model: Option<String>,
    /// When the context starts.
    began: u64,
    turn: Option<Turn>,
    /// Words said while a turn runs, oldest first, by their say ids.
    held: Vec<(String, String)>,
    said: HashMap<String, Said>,
    /// The say that first carried each throw; a later say of the same throw is that say.
    throws: HashMap<String, String>,
    aliases: HashMap<String, String>,
    /// Says asked again while their words are held, answered once they are taken: the ask
    /// id and the say.
    owed: Vec<(String, String)>,
    /// Stops that wait for the turn to end, and until when.
    stops: Vec<(String, Instant)>,
    /// Picks that wait for the harness's answer, by the request id: the ask id and the model.
    picks: HashMap<u64, (String, String)>,
    /// The answers kept for asks asked again, newest last.
    answers: VecDeque<(String, PluginFrame)>,
    calls: Vec<LiveCall>,
    ended: VecDeque<LiveEnded>,
    settled: VecDeque<LiveSettled>,
    context_left: Option<f64>,
    /// The person paused the conversation: words held for it are let go at the turn's end.
    parked: bool,
    ending: bool,
    out: mpsc::UnboundedSender<String>,
    state: watch::Sender<LiveState>,
}

impl<'h, H: Host> Copy<'h, H> {
    fn new(
        host: &'h H,
        session: String,
        hook: Option<String>,
        opened: &Value,
        began: u64,
        out: mpsc::UnboundedSender<String>,
        state: watch::Sender<LiveState>,
    ) -> Self {
        let models = opened
            .pointer("/models/availableModels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|model| model.get("modelId")?.as_str().map(str::to_owned))
            .collect();
        let model = opened
            .pointer("/models/currentModelId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Self {
            host,
            session,
            hook,
            models,
            model,
            began,
            turn: None,
            held: Vec::new(),
            said: HashMap::new(),
            throws: HashMap::new(),
            aliases: HashMap::new(),
            owed: Vec::new(),
            stops: Vec::new(),
            picks: HashMap::new(),
            answers: VecDeque::new(),
            calls: Vec::new(),
            ended: VecDeque::new(),
            settled: VecDeque::new(),
            context_left: None,
            parked: false,
            ending: false,
            out,
            state,
        }
    }

    fn idle(&self) -> bool {
        self.turn.is_none() && self.held.is_empty()
    }

    /// The say that `id` names: the first say of its throw.
    fn say<'a>(&'a self, id: &'a str) -> &'a str {
        self.aliases.get(id).map_or(id, String::as_str)
    }

    /// Send the state where it changed; the link sends it at most every [`STATE_EVERY`].
    fn publish(&self) {
        let state = LiveState {
            turn: self.turn.as_ref().map(|turn| LiveTurn {
                began_at: turn.began_at,
                says: turn.says.clone(),
            }),
            idle: Some(self.idle()),
            pending: (!self.held.is_empty()).then_some(true),
            typing: Some(false),
            model: self.model.clone(),
            context_left: self.context_left,
            calls: self.calls.clone(),
            ended: self.ended.iter().cloned().collect(),
            settled: self.settled.iter().cloned().collect(),
            ..LiveState::default()
        };
        self.state.send_if_modified(|sent| {
            let changed = *sent != state;
            *sent = state;
            changed
        });
    }

    /// Answer the ask `id`; `kept` keeps the answer for the same id asked again.
    fn reply(&mut self, id: &str, answer: Result<Value, Refusal>) {
        let kept = match &answer {
            Ok(_) => true,
            Err((word, _)) => *word == refusal::NO_ANSWER,
        };
        let frame = match answer {
            Ok(ok) => PluginFrame::Answer {
                id: id.to_owned(),
                ok: Some(ok),
                refused: None,
                error: None,
            },
            Err((word, line)) => PluginFrame::Answer {
                id: id.to_owned(),
                ok: None,
                refused: Some(word.to_owned()),
                error: Some(line),
            },
        };
        if let Ok(text) = serde_json::to_string(&frame) {
            let _ = self.out.send(text);
        }
        if kept {
            self.answers.push_back((id.to_owned(), frame));
            if self.answers.len() > ANSWERS_KEPT {
                self.answers.pop_front();
            }
        }
    }

    /// The answer to a say whose words went into a prompt.
    fn taken(queued: u64, taken: u64) -> Value {
        json!(DoorSaid {
            queued_at: Some(queued),
            taken_at: Some(taken),
        })
    }

    /// Prompt the harness with the words of `says`, held or new, joined.
    async fn prompt(&mut self, acp: &mut Acp, says: Vec<(String, String)>) -> Result<(), String> {
        let words = says
            .iter()
            .map(|(_, words)| words.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        let prompt = acp.prompt(&self.session, &words).await?;
        let at = super::drive::now();
        let ids: Vec<String> = says.into_iter().map(|(id, _)| id).collect();
        for id in &ids {
            let queued = match self.said.get(id) {
                Some(Said::Held(queued)) => *queued,
                _ => at,
            };
            self.said.insert(id.clone(), Said::Taken(queued, at));
        }
        for (ask, say) in std::mem::take(&mut self.owed) {
            match self.said.get(&say) {
                Some(Said::Taken(queued, taken)) => {
                    let answer = Self::taken(*queued, *taken);
                    self.reply(&ask, Ok(answer));
                }
                _ => self.owed.push((ask, say)),
            }
        }
        self.turn = Some(Turn {
            prompt,
            began_at: at,
            says: ids,
            cancelled: false,
        });
        self.parked = false;
        self.publish();
        Ok(())
    }

    /// Ask the harness to cancel the running turn, once; true where it asked now.
    async fn cancel(&mut self, acp: &mut Acp) -> Result<bool, String> {
        let Some(turn) = self.turn.as_mut().filter(|turn| !turn.cancelled) else {
            return Ok(false);
        };
        turn.cancelled = true;
        acp.cancel(&self.session).await?;
        Ok(true)
    }

    /// Let the held words go unseen: re-asks of them hear `not-taken`.
    fn drop_held(&mut self) {
        for (id, _) in std::mem::take(&mut self.held) {
            self.said.insert(id, Said::Dropped);
        }
        for (ask, say) in std::mem::take(&mut self.owed) {
            let line = "the conversation let the words go before they went in".to_owned();
            match self.said.get(&say) {
                Some(Said::Dropped) => self.reply(&ask, Err((door::refusal::NOT_TAKEN, line))),
                _ => self.owed.push((ask, say)),
            }
        }
    }

    /// The runtime ends the copy: from here on every ask is answered `ending`, held words are
    /// let go, and a running turn is cancelled; true where it was cancelled now.
    async fn end(&mut self, acp: &mut Acp) -> Result<bool, String> {
        self.ending = true;
        self.drop_held();
        self.publish();
        self.cancel(acp).await
    }

    /// One message of the harness: the answer to a prompt or a pick, or an update of the
    /// session; true where the copy cancelled the turn on it.
    async fn message(&mut self, acp: &mut Acp, message: Value) -> Result<bool, String> {
        let id = message.get("id").and_then(Value::as_u64);
        if let Some(id) = id
            && self.turn.as_ref().is_some_and(|turn| turn.prompt == id)
        {
            self.turn_ended(acp, &message).await?;
            return Ok(false);
        }
        if let Some((ask, model)) = id.and_then(|id| self.picks.remove(&id)) {
            let answer = match message.get("error") {
                Some(error) => Err((word(error), said(error))),
                None => {
                    self.model = Some(model.clone());
                    self.publish();
                    Ok(json!(Picked {
                        model: Some(model),
                        thinking: None,
                    }))
                }
            };
            self.reply(&ask, answer);
            return Ok(false);
        }
        if message.get("method").and_then(Value::as_str) == Some("session/update")
            && message.pointer("/params/sessionId").and_then(Value::as_str)
                == Some(self.session.as_str())
            && let Some(update) = message.pointer("/params/update")
        {
            self.update(update);
        }
        Ok(false)
    }

    /// The harness answered the running turn's prompt: the turn settled. Held words go in
    /// as the next prompt, unless the person paused the conversation or it ends.
    async fn turn_ended(&mut self, acp: &mut Acp, answer: &Value) -> Result<(), String> {
        let Some(turn) = self.turn.take() else {
            return Ok(());
        };
        let label = self.host.harness().label();
        let (failed, error) = match (answer.get("error"), answer.pointer("/result/stopReason")) {
            (Some(error), _) => (true, Some(said(error))),
            (None, Some(Value::String(reason)))
                if reason == "end_turn" || reason == "cancelled" =>
            {
                (false, None)
            }
            (None, reason) => (
                true,
                Some(format!(
                    "{label} stopped the turn: {}",
                    reason.and_then(Value::as_str).unwrap_or("no reason")
                )),
            ),
        };
        self.settled.push_back(LiveSettled {
            began_at: turn.began_at,
            at: super::drive::now(),
            says: turn.says,
            failed,
            error,
        });
        if self.settled.len() > SETTLED_KEPT {
            self.settled.pop_front();
        }
        self.calls.clear();
        let stopped = json!(DoorStopped {
            stopped: true,
            running: Vec::new(),
        });
        for (ask, _) in std::mem::take(&mut self.stops) {
            self.reply(&ask, Ok(stopped.clone()));
        }
        if self.parked || self.ending {
            self.drop_held();
        } else if !self.held.is_empty() {
            let held = std::mem::take(&mut self.held);
            self.prompt(acp, held).await?;
        }
        self.publish();
        Ok(())
    }

    /// Stops whose turn still runs past [`STOP_WITHIN`] hear `no-answer`.
    fn unstopped(&mut self) {
        let now = Instant::now();
        let (late, waits): (Vec<_>, Vec<_>) = std::mem::take(&mut self.stops)
            .into_iter()
            .partition(|(_, at)| *at <= now);
        self.stops = waits;
        for (ask, _) in late {
            let line = "a turn still runs".to_owned();
            self.reply(&ask, Err((refusal::NO_ANSWER, line)));
        }
    }

    /// A `session/update` of the session: the calls that run and end, and the context left.
    fn update(&mut self, update: &Value) {
        let text = |key: &str| update.get(key).and_then(Value::as_str);
        match text("sessionUpdate") {
            Some("tool_call" | "tool_call_update") => {
                let Some(id) = text("toolCallId") else {
                    return;
                };
                let running = self.calls.iter().position(|call| call.id == id);
                match (text("status"), running) {
                    (Some("completed" | "failed"), Some(at)) => {
                        self.calls.remove(at);
                        self.ended.push_back(LiveEnded {
                            id: id.to_owned(),
                            at: super::drive::now(),
                        });
                        if self.ended.len() > ENDED_KEPT {
                            self.ended.pop_front();
                        }
                    }
                    (Some("pending" | "in_progress"), None) => {
                        self.calls.push(LiveCall {
                            id: id.to_owned(),
                            tool: self.host.tool(id, text("title").unwrap_or_default()),
                            since: super::drive::now(),
                            children: Vec::new(),
                            jobs: Vec::new(),
                        });
                    }
                    _ => return,
                }
            }
            Some("usage_update") => {
                let used = update.get("used").and_then(Value::as_f64);
                let size = update.get("size").and_then(Value::as_f64);
                let (Some(used), Some(size)) = (used, size.filter(|size| *size > 0.0)) else {
                    return;
                };
                self.context_left = Some((1.0 - used / size).clamp(0.0, 1.0));
            }
            _ => return,
        }
        self.publish();
    }

    /// One ask of the runtime; true where the copy cancelled the turn on it.
    async fn ask(&mut self, acp: &mut Acp, ask: Ask) -> Result<bool, String> {
        let id = match &ask {
            Ask::Say { id, .. }
            | Ask::Pick { id, .. }
            | Ask::Stop { id, .. }
            | Ask::Force { id, .. }
            | Ask::Compact { id, .. } => id.clone(),
        };
        if !matches!(ask, Ask::Say { .. })
            && let Some((_, frame)) = self.answers.iter().find(|(kept, _)| *kept == id)
        {
            if let Ok(text) = serde_json::to_string(frame) {
                let _ = self.out.send(text);
            }
            return Ok(false);
        }
        if self.ending {
            let line = "the copy lets the conversation go".to_owned();
            self.reply(&id, Err((door::refusal::ENDING, line)));
            return Ok(false);
        }
        let label = self.host.harness().label();
        match ask {
            Ask::Say {
                words, idle, throw, ..
            } => {
                if let Some(first) = throw.as_ref().and_then(|throw| self.throws.get(throw))
                    && *first != id
                {
                    self.aliases.insert(id.clone(), first.clone());
                }
                let say = self.say(&id).to_owned();
                match self.said.get(&say).copied() {
                    Some(Said::Taken(queued, taken)) => {
                        self.reply(&id, Ok(Self::taken(queued, taken)));
                    }
                    Some(Said::Held(_)) => self.owed.push((id, say)),
                    Some(Said::Dropped) => {
                        let line = "the conversation let the words go before they went in";
                        self.reply(&id, Err((door::refusal::NOT_TAKEN, line.into())));
                    }
                    None if words.is_empty() => {
                        let line = "the copy remembers no say of this id".to_owned();
                        self.reply(&id, Err((refusal::REFUSED, line)));
                    }
                    None if idle && !self.idle() => {
                        let line = format!("{label} runs a turn of the conversation");
                        self.reply(&id, Err((refusal::HARNESS_BUSY, line)));
                    }
                    None => {
                        if let Some(throw) = throw {
                            self.throws.insert(throw, id.clone());
                        }
                        if self.turn.is_none() {
                            self.prompt(acp, vec![(id.clone(), words)]).await?;
                            if let Some(Said::Taken(queued, taken)) = self.said.get(&id).copied() {
                                self.reply(&id, Ok(Self::taken(queued, taken)));
                            }
                        } else {
                            let at = super::drive::now();
                            self.held.push((id.clone(), words));
                            self.said.insert(id.clone(), Said::Held(at));
                            let held = DoorSaid {
                                queued_at: Some(at),
                                taken_at: None,
                            };
                            self.reply(&id, Ok(json!(held)));
                            self.publish();
                        }
                    }
                }
                Ok(false)
            }
            Ask::Pick {
                model, thinking, ..
            } => {
                if thinking.is_some() {
                    let line = format!("{label} takes no thinking level from its ACP client");
                    self.reply(&id, Err((refusal::SELECTION_UNSUPPORTED, line)));
                    return Ok(false);
                }
                let Some(model) = model else {
                    let picked = Picked {
                        model: self.model.clone(),
                        thinking: None,
                    };
                    self.reply(&id, Ok(json!(picked)));
                    return Ok(false);
                };
                if !self.models.is_empty() && !self.models.contains(&model) {
                    let line = format!("{label} offers no model {model} here");
                    self.reply(&id, Err((refusal::MODEL_MISSING, line)));
                    return Ok(false);
                }
                let (method, params) = self.host.pick(&self.session, "model", &model);
                let request = acp.request(method, params).await?;
                self.picks.insert(request, (id, model));
                Ok(false)
            }
            Ask::Stop { say, .. } => {
                let Some(turn) = &self.turn else {
                    let stopped = DoorStopped {
                        stopped: false,
                        running: Vec::new(),
                    };
                    self.reply(&id, Ok(json!(stopped)));
                    return Ok(false);
                };
                if let Some(say) = &say
                    && !turn.says.iter().any(|began| began == self.say(say))
                {
                    let line = "the running turn is not one that the say began".to_owned();
                    self.reply(&id, Err((refusal::OTHER_TURN, line)));
                    return Ok(false);
                }
                if say.is_none() {
                    self.parked = true;
                }
                self.stops.push((id, Instant::now() + STOP_WITHIN));
                self.cancel(acp).await
            }
            Ask::Force { say, .. } => {
                let say = self.say(&say).to_owned();
                let Some(at) = self.held.iter().position(|(held, _)| *held == say) else {
                    let answer = match self.said.get(&say) {
                        Some(Said::Taken(..)) => Ok(json!(DoorStopped {
                            stopped: false,
                            running: Vec::new(),
                        })),
                        _ => Err((
                            refusal::REFUSED,
                            "no words of that say wait here".to_owned(),
                        )),
                    };
                    self.reply(&id, answer);
                    return Ok(false);
                };
                let forced = self.held.remove(at);
                self.held.insert(0, forced);
                self.parked = false;
                if self.turn.is_none() {
                    let held = std::mem::take(&mut self.held);
                    self.prompt(acp, held).await?;
                    let stopped = DoorStopped {
                        stopped: false,
                        running: Vec::new(),
                    };
                    self.reply(&id, Ok(json!(stopped)));
                    return Ok(false);
                }
                self.stops.push((id, Instant::now() + STOP_WITHIN));
                self.cancel(acp).await
            }
            Ask::Compact { .. } => {
                let answer = match self.turn {
                    Some(_) => Err((
                        refusal::HARNESS_BUSY,
                        format!("{label} runs a turn of the conversation"),
                    )),
                    None => Ok(json!(DoorCompacted {
                        from: self.began,
                        forgot: 0,
                    })),
                };
                self.reply(&id, answer);
                Ok(false)
            }
        }
    }
}

/// The harness's own line of a JSON-RPC error.
fn said(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("refused")
        .to_owned()
}

/// What the link heard from the door.
enum Heard {
    Ask(Ask),
    /// The runtime closed the door for good: it will not take this copy again.
    Gone(String),
}

/// Who the copy says it is in its hello.
struct Hello {
    harness: String,
    session: String,
    cwd: PathBuf,
}

/// How one connection ended.
enum Closed {
    /// The copy closed it: the turn ends.
    Ours,
    /// The runtime will not take this copy again (4400, 4410).
    Never(String),
    /// The runtime does not speak the door yet (4409).
    Later,
    /// Any other drop, after a welcome or before one.
    Dropped { welcomed: bool },
}

/// Hold the door to the runtime at `url` for the copy: dial, say hello, pass asks in and
/// frames out, and redial after a drop as PROTOCOL.md §4.4 says. It ends once the copy drops
/// `outgoing`.
async fn link(
    url: String,
    hello: Hello,
    mut state: watch::Receiver<LiveState>,
    mut outgoing: mpsc::UnboundedReceiver<String>,
    told: mpsc::UnboundedSender<Heard>,
) {
    let mut wait = REDIAL_FIRST;
    loop {
        if let Some(socket) = dial(&url).await {
            match talk(socket, &hello, &mut state, &mut outgoing, &told).await {
                Closed::Ours => return,
                Closed::Never(why) => {
                    let _ = told.send(Heard::Gone(why));
                    return;
                }
                Closed::Later => wait = REDIAL_OLD,
                Closed::Dropped { welcomed: true } => wait = REDIAL_FIRST,
                Closed::Dropped { welcomed: false } => {}
            }
        }
        // Frames meant for a connection that dropped go nowhere: the runtime asks again.
        let pause = tokio::time::sleep(wait);
        tokio::pin!(pause);
        loop {
            tokio::select! {
                () = &mut pause => break,
                frame = outgoing.recv() => if frame.is_none() {
                    return;
                },
            }
        }
        if wait != REDIAL_OLD {
            wait = (wait * 2).min(REDIAL_MOST);
        } else {
            wait = REDIAL_FIRST;
        }
    }
}

/// A connection to the door at `url`, a `ws://` address on the loopback.
async fn dial(url: &str) -> Option<WebSocketStream<TcpStream>> {
    let authority = url.strip_prefix("ws://")?.split('/').next()?;
    let request = url.into_client_request().ok()?;
    let stream = tokio::time::timeout(DIAL_WITHIN, TcpStream::connect(authority))
        .await
        .ok()?
        .ok()?;
    let (socket, _) = tokio::time::timeout(
        DIAL_WITHIN,
        tokio_tungstenite::client_async(request, stream),
    )
    .await
    .ok()?
    .ok()?;
    Some(socket)
}

/// How the runtime closed the door, by its code.
fn closed(frame: Option<CloseFrame>, welcomed: bool) -> Closed {
    match frame.map(|frame| (u16::from(frame.code), frame.reason.to_string())) {
        Some((4400, why)) => Closed::Never(format!("4400 {why}")),
        Some((4410, why)) => Closed::Never(format!("4410 {why}")),
        Some((4409, _)) => Closed::Later,
        _ => Closed::Dropped { welcomed },
    }
}

/// Speak the door on one connection until it ends.
async fn talk(
    socket: WebSocketStream<TcpStream>,
    hello: &Hello,
    state: &mut watch::Receiver<LiveState>,
    outgoing: &mut mpsc::UnboundedReceiver<String>,
    told: &mpsc::UnboundedSender<Heard>,
) -> Closed {
    let (mut sink, mut source) = socket.split();
    let frame = PluginFrame::Hello {
        door: door::DOOR,
        harness: hello.harness.clone(),
        pid: std::process::id(),
        session_id: hello.session.clone(),
        mode: DoorMode::Rpc,
        lead: None,
        state: Some(state.borrow_and_update().clone()),
        cwd: Some(hello.cwd.clone()),
    };
    if send(&mut sink, &frame).await.is_err() {
        return Closed::Dropped { welcomed: false };
    }
    // The runtime's welcome; a welcome to another door version leaves the copy silent
    // until the runtime closes.
    let welcome = tokio::time::timeout(DIAL_WITHIN, async {
        loop {
            match source.next().await {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(RuntimeFrame::Welcome { door, entries, .. }) =
                        serde_json::from_str::<RuntimeFrame>(text.as_str())
                    {
                        return Ok((door == door::DOOR, entries.is_some()));
                    }
                }
                Some(Ok(Message::Close(frame))) => return Err(closed(frame, false)),
                Some(Ok(_)) => {}
                _ => return Err(Closed::Dropped { welcomed: false }),
            }
        }
    })
    .await;
    let (speaks, entries) = match welcome {
        Ok(Ok(welcomed)) => welcomed,
        Ok(Err(closed)) => return closed,
        Err(_) => return Closed::Dropped { welcomed: false },
    };
    if speaks && entries {
        let lost = PluginFrame::Lost {
            why: "the copy keeps no records: the harness's transcript has them".into(),
        };
        if send(&mut sink, &lost).await.is_err() {
            return Closed::Dropped { welcomed: true };
        }
    }
    let mut sent_at: Option<Instant> = None;
    let mut watching = speaks;
    if speaks {
        state.mark_changed();
    }
    loop {
        tokio::select! {
            message = source.next() => match message {
                Some(Ok(Message::Text(text))) if speaks => {
                    if let Ok(RuntimeFrame::Ask(ask)) = serde_json::from_str::<RuntimeFrame>(text.as_str()) {
                        let _ = told.send(Heard::Ask(ask));
                    }
                }
                Some(Ok(Message::Close(frame))) => return closed(frame, true),
                Some(Ok(_)) => {}
                _ => return Closed::Dropped { welcomed: true },
            },
            frame = outgoing.recv() => match frame {
                Some(text) if speaks => {
                    if sink.send(Message::text(text)).await.is_err() {
                        return Closed::Dropped { welcomed: true };
                    }
                }
                Some(_) => {}
                None => {
                    let close = CloseFrame {
                        code: CloseCode::Normal,
                        reason: "".into(),
                    };
                    let _ = sink.send(Message::Close(Some(close))).await;
                    let _ = sink.close().await;
                    return Closed::Ours;
                }
            },
            changed = state.changed(), if watching => {
                if changed.is_err() {
                    watching = false;
                    continue;
                }
                if let Some(at) = sent_at {
                    tokio::time::sleep_until(at + STATE_EVERY).await;
                }
                let frame = PluginFrame::State {
                    state: state.borrow_and_update().clone(),
                };
                if send(&mut sink, &frame).await.is_err() {
                    return Closed::Dropped { welcomed: true };
                }
                sent_at = Some(Instant::now());
            }
        }
    }
}

/// Send one frame.
async fn send(
    sink: &mut futures_util::stream::SplitSink<WebSocketStream<TcpStream>, Message>,
    frame: &PluginFrame,
) -> Result<(), ()> {
    let text = serde_json::to_string(frame).map_err(drop)?;
    sink.send(Message::text(text)).await.map_err(drop)
}
