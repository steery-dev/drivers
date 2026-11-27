//! Steery's plugin door, version 1 (PROTOCOL.md §4.4; API.md §8.4), as a turn speaks it: one
//! WebSocket per turn process to the runtime at `<CANVAS_HOME>/steery.json`'s `door`. It
//! carries the transcript's records (`entries`), each drawer's lines as they stream (`live`),
//! the turn's state, and its end (`settle`).
//!
//! The turn hands the door notes in its own order ([`Door::append`], [`Door::live`],
//! [`Door::state`]); one thread dials, sends them in that order, answers the runtime's asks,
//! and redials after a drop. A one-shot turn takes no words, picks or compactions: `stop`
//! ends it as `interrupted`, and every other ask is refused.

use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use steery_sdk::refusal;
use steery_sdk::wire::door::{
    Ask, DOOR, DoorMode, LIVE_EVERY_MS, Live, LiveEnd, LivePart, LiveStop, PluginFrame,
    RECORDS_MAX, RuntimeFrame, close,
};
use steery_sdk::wire::harness::{LiveState, Record, TurnState, Voice};
use tokio::sync::Notify;
use tungstenite::protocol::frame::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::{Message, WebSocket};

/// The harness a hello names.
const HARNESS: &str = "canvas";
const LIVE_EVERY: Duration = Duration::from_millis(LIVE_EVERY_MS);
/// How long a read waits before the thread looks at what it has to send.
const POLL: Duration = Duration::from_millis(10);
/// How long the runtime has to welcome a hello.
const WELCOMED_WITHIN: Duration = Duration::from_secs(5);
/// Redials after a drop: from 250 ms, doubling to 2 s; 5 s after 4409.
const REDIAL_MIN: Duration = Duration::from_millis(250);
const REDIAL_MAX: Duration = Duration::from_secs(2);
const VERSION_WAIT: Duration = Duration::from_secs(5);
/// How long a settle waits for its answer, and the close for the runtime's.
const SETTLE_WAIT: Duration = Duration::from_secs(1);
const CLOSED_WITHIN: Duration = Duration::from_millis(500);
const ANSWERS_KEPT: usize = 256;

/// The door that `steery.json` in `root` names, where it names one.
pub fn url(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("steery.json")).ok()?;
    let json: Value = serde_json::from_str(&text).ok()?;
    json["door"]
        .as_str()
        .filter(|door| !door.is_empty())
        .map(str::to_owned)
}

/// What the turn hands the door, in its order.
enum Note {
    /// The record at this index was appended.
    Record(usize),
    /// A drawer's lines so far, and its end with the last of them.
    Live {
        message: String,
        at: u64,
        text: String,
        end: Option<LiveStop>,
    },
    /// The state changed.
    State,
}

struct Inner {
    /// Every record of the transcript, in order.
    records: Vec<Value>,
    notes: VecDeque<Note>,
    state: LiveState,
    /// The turn ended so: settle and close.
    settle: Option<TurnState>,
}

struct Shared {
    inner: Mutex<Inner>,
    url: String,
    session: String,
    cwd: PathBuf,
    /// The runtime's `stop`.
    stop: Arc<Notify>,
}

fn locked(shared: &Shared) -> MutexGuard<'_, Inner> {
    shared.inner.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A connected, or redialling, door.
pub struct Door {
    shared: Arc<Shared>,
    io: Option<JoinHandle<()>>,
}

impl Door {
    /// Dials the door at `url` for the session `session`, whose transcript holds `records`;
    /// the runtime's `stop` notifies `stop`.
    pub fn open(
        url: String,
        session: String,
        cwd: PathBuf,
        records: Vec<Value>,
        state: LiveState,
        stop: Arc<Notify>,
    ) -> Self {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                records,
                notes: VecDeque::new(),
                state,
                settle: None,
            }),
            url,
            session,
            cwd,
            stop,
        });
        let io = std::thread::spawn({
            let shared = Arc::clone(&shared);
            move || run(&shared)
        });
        Self {
            shared,
            io: Some(io),
        }
    }

    /// A record the turn appended to the transcript.
    pub fn append(&self, record: Value) {
        let mut inner = locked(&self.shared);
        inner.records.push(record);
        let at = inner.records.len() - 1;
        inner.notes.push_back(Note::Record(at));
    }

    /// The lines of the model call `message` so far, joined with `"\n"`; `end` with the last.
    pub fn live(&self, message: &str, text: &str, end: Option<LiveStop>) {
        locked(&self.shared).notes.push_back(Note::Live {
            message: message.to_owned(),
            at: crate::transcript::now(),
            text: text.to_owned(),
            end,
        });
    }

    pub fn state(&self, state: LiveState) {
        let mut inner = locked(&self.shared);
        inner.state = state;
        inner.notes.push_back(Note::State);
    }

    /// The turn's end, after its last record: `settle`, up to 1 s for its answer, then the
    /// close with 1000.
    pub fn settle(mut self, outcome: TurnState) {
        locked(&self.shared).settle = Some(outcome);
        if let Some(io) = self.io.take() {
            let _ = io.join();
        }
    }
}

/// One message's lines on the door: what of them went out on this connection.
struct Text {
    message: String,
    first_at: u64,
    at: u64,
    text: String,
    /// Unicode scalars sent.
    sent: usize,
    end: Option<LiveStop>,
    dirty: bool,
}

/// What the thread keeps across connections.
struct Io {
    texts: Vec<Text>,
    answers: HashMap<String, String>,
    order: VecDeque<String>,
    /// The runtime spoke another door version: nothing more goes out.
    mute: bool,
}

impl Io {
    /// A note, where no connection carries it: the records and the state go again after
    /// the next welcome.
    fn keep(&mut self, note: Note) {
        if let Note::Live {
            message,
            at,
            text,
            end,
        } = note
        {
            match self.texts.iter_mut().find(|t| t.message == message) {
                Some(held) => {
                    held.at = at;
                    held.text = text;
                    held.end = end;
                    held.dirty = true;
                }
                None => self.texts.push(Text {
                    message,
                    first_at: at,
                    at,
                    text,
                    sent: 0,
                    end,
                    dirty: true,
                }),
            }
        }
    }
}

enum Served {
    /// The turn settled.
    Ended,
    /// The runtime closed it with this code.
    Closed(u16),
    Dropped,
}

/// Dials, serves and redials until the turn settles, or the runtime says not to dial again.
fn run(shared: &Shared) {
    let mut io = Io {
        texts: Vec::new(),
        answers: HashMap::new(),
        order: VecDeque::new(),
        mute: false,
    };
    let mut wait = REDIAL_MIN;
    loop {
        match dial(shared, &mut io) {
            Ok(mut conn) => {
                wait = REDIAL_MIN;
                match conn.serve(shared, &mut io) {
                    Served::Ended => return,
                    // No dial again: the turn's notes from here on reach no one.
                    Served::Closed(close::HELLO | close::REPLACED | 1000) => return,
                    Served::Closed(close::VERSION) => wait = VERSION_WAIT,
                    Served::Closed(_) | Served::Dropped => {}
                }
            }
            Err(why) => eprintln!("steery door: {why}"),
        }
        if io.mute || idle(shared, &mut io, Instant::now() + wait) {
            return;
        }
        wait = (wait * 2).min(REDIAL_MAX);
    }
}

/// Keeps the turn's notes with no connection until `until`: whether the turn settled
/// meanwhile.
fn idle(shared: &Shared, io: &mut Io, until: Instant) -> bool {
    loop {
        let (notes, settled) = {
            let mut inner = locked(shared);
            (std::mem::take(&mut inner.notes), inner.settle.is_some())
        };
        for note in notes {
            io.keep(note);
        }
        if settled {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(POLL);
    }
}

/// A welcomed connection.
struct Conn {
    socket: WebSocket<TcpStream>,
    /// The runtime takes records: the index past the last one sent.
    entries: Option<usize>,
    live: bool,
    lived_at: Instant,
}

fn frame(frame: &PluginFrame) -> String {
    serde_json::to_string(frame).expect("a door frame writes")
}

/// The hello said and the welcome read, and what the welcome asks sent: the records after
/// its `after`, the state, every message's lines from their start.
fn dial(shared: &Shared, io: &mut Io) -> Result<Conn, String> {
    let host = shared
        .url
        .strip_prefix("ws://")
        .and_then(|rest| rest.split('/').next())
        .ok_or_else(|| format!("door {} is no ws:// URL", shared.url))?;
    let address = host
        .to_socket_addrs()
        .map_err(|e| format!("door {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("door {host} names no address"))?;
    let stream = TcpStream::connect_timeout(&address, WELCOMED_WITHIN)
        .map_err(|e| format!("door {host}: {e}"))?;
    stream
        .set_read_timeout(Some(WELCOMED_WITHIN))
        .map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);
    let (mut socket, _) = tungstenite::client(shared.url.as_str(), stream)
        .map_err(|e| format!("door handshake: {e}"))?;
    let hello = PluginFrame::Hello {
        door: DOOR,
        harness: HARNESS.into(),
        pid: std::process::id(),
        session_id: shared.session.clone(),
        mode: DoorMode::Print,
        lead: None,
        state: Some(locked(shared).state.clone()),
        cwd: Some(shared.cwd.clone()),
    };
    socket
        .send(Message::text(frame(&hello)))
        .map_err(|e| format!("door hello: {e}"))?;
    let welcome = loop {
        match socket.read().map_err(|e| format!("door welcome: {e}"))? {
            Message::Text(text) => break text,
            Message::Close(closed) => {
                return Err(format!(
                    "door closed before its welcome: {}",
                    closed.map_or(1005, |c| u16::from(c.code))
                ));
            }
            _ => {}
        }
    };
    let (entries, live) = match serde_json::from_str::<RuntimeFrame>(welcome.as_str()) {
        Ok(RuntimeFrame::Welcome {
            door: DOOR,
            entries,
            live,
        }) => (entries, live),
        Ok(RuntimeFrame::Welcome { door, .. }) => {
            io.mute = true;
            return Err(format!("the runtime speaks door {door}, not {DOOR}"));
        }
        _ => return Err(format!("door welcome unread: {}", welcome.as_str())),
    };
    socket
        .get_ref()
        .set_read_timeout(Some(POLL))
        .map_err(|e| e.to_string())?;
    let mut conn = Conn {
        socket,
        entries: None,
        live,
        lived_at: Instant::now() - LIVE_EVERY,
    };
    let (notes, from, state) = {
        let mut inner = locked(shared);
        let from = entries.map(|entries| match entries.after {
            None => Ok(0),
            Some(after) => inner
                .records
                .iter()
                .position(|record| record["id"].as_str() == Some(after.as_str()))
                .map(|at| at + 1)
                .ok_or(after),
        });
        (std::mem::take(&mut inner.notes), from, inner.state.clone())
    };
    // What came before the welcome goes out whole below.
    for note in notes {
        io.keep(note);
    }
    match from {
        Some(Ok(from)) => {
            conn.entries = Some(from);
            conn.records(shared, None)?;
        }
        Some(Err(after)) => conn.send(&PluginFrame::Lost {
            why: format!("this transcript holds no record {after}"),
        })?,
        None => {}
    }
    conn.send(&PluginFrame::State { state })?;
    for text in &mut io.texts {
        text.sent = 0;
        text.dirty = true;
    }
    conn.lives(io, false)?;
    Ok(conn)
}

impl Conn {
    fn send(&mut self, said: &PluginFrame) -> Result<(), String> {
        self.socket
            .send(Message::text(frame(said)))
            .map_err(|e| format!("door: {e}"))
    }

    /// The records not sent yet, as `entries` frames of at most [`RECORDS_MAX`] bytes; a
    /// larger record goes as its id alone.
    fn records(&mut self, shared: &Shared, through: Option<usize>) -> Result<(), String> {
        let Some(mut at) = self.entries else {
            return Ok(());
        };
        let frames = {
            let inner = locked(shared);
            let end = through.map_or(inner.records.len(), |through| through + 1);
            let records = &inner.records[..end];
            let mut frames = Vec::new();
            while at < records.len() {
                let after = at
                    .checked_sub(1)
                    .and_then(|before| records[before]["id"].as_str().map(str::to_owned));
                let (mut batch, mut bytes) = (Vec::new(), 0);
                while at < records.len() {
                    let record = &records[at];
                    let size = record.to_string().len();
                    if !batch.is_empty() && bytes + size > RECORDS_MAX {
                        break;
                    }
                    bytes += size;
                    batch.push(Record {
                        id: record["id"].as_str().unwrap_or_default().to_owned(),
                        value: (size <= RECORDS_MAX).then(|| record.clone()),
                    });
                    at += 1;
                }
                frames.push(frame(&PluginFrame::Entries {
                    after,
                    records: batch,
                }));
            }
            frames
        };
        self.entries = Some(at);
        for said in frames {
            self.socket
                .send(Message::text(said))
                .map_err(|e| format!("door: {e}"))?;
        }
        Ok(())
    }

    /// Each message's lines that changed, from where its last frame ended. `now` sends them
    /// even within [`LIVE_EVERY`] of the last, after waiting out the rest of it.
    fn lives(&mut self, io: &mut Io, now: bool) -> Result<(), String> {
        if !self.live || !io.texts.iter().any(|t| t.dirty) {
            return Ok(());
        }
        let since = self.lived_at.elapsed();
        if since < LIVE_EVERY {
            if !now {
                return Ok(());
            }
            std::thread::sleep(LIVE_EVERY - since);
        }
        self.lived_at = Instant::now();
        for text in io.texts.iter_mut().filter(|t| t.dirty) {
            text.dirty = false;
            let new: String = text.text.chars().skip(text.sent).collect();
            let from = u32::try_from(text.sent).unwrap_or(u32::MAX);
            text.sent += new.chars().count();
            let said = PluginFrame::Live(Live {
                message: text.message.clone(),
                at: text.at,
                first_at: text.first_at,
                parts: vec![LivePart {
                    voice: Voice::Agent,
                    from,
                    text: new,
                }],
                forming: None,
                end: text.end.map(|stop| LiveEnd { stop }),
            });
            self.socket
                .send(Message::text(frame(&said)))
                .map_err(|e| format!("door: {e}"))?;
        }
        io.texts.retain(|t| t.end.is_none() || t.dirty);
        Ok(())
    }

    /// The turn's notes in their order, the runtime's asks, and at the turn's end its settle.
    fn serve(&mut self, shared: &Shared, io: &mut Io) -> Served {
        loop {
            let (notes, settle) = {
                let mut inner = locked(shared);
                (std::mem::take(&mut inner.notes), inner.settle)
            };
            if self.notes(shared, io, notes).is_err() {
                return Served::Dropped;
            }
            if let Some(outcome) = settle {
                return self.settle(shared, io, outcome);
            }
            if let Some(served) = self.read(shared, io, None) {
                return served;
            }
        }
    }

    fn notes(&mut self, shared: &Shared, io: &mut Io, notes: VecDeque<Note>) -> Result<(), String> {
        for note in notes {
            match note {
                Note::Record(at) => {
                    // A message's lines, and its end, go before the record after them.
                    self.lives(io, true)?;
                    self.records(shared, Some(at))?;
                }
                Note::State => {
                    self.lives(io, true)?;
                    let state = locked(shared).state.clone();
                    self.send(&PluginFrame::State { state })?;
                }
                live @ Note::Live { .. } => io.keep(live),
            }
        }
        self.lives(io, false)
    }

    /// One frame of the runtime's, or none within [`POLL`]; how the connection ended, where
    /// it did. `settle` names the settle whose answer is awaited: whether it came.
    fn read(&mut self, shared: &Shared, io: &mut Io, settle: Option<&str>) -> Option<Served> {
        match self.socket.read() {
            Ok(Message::Text(text)) => match serde_json::from_str::<RuntimeFrame>(text.as_str()) {
                Ok(RuntimeFrame::Ask(ask)) => {
                    let answer = asked(shared, io, ask);
                    if self.socket.send(Message::text(answer)).is_err() {
                        return Some(Served::Dropped);
                    }
                    None
                }
                Ok(RuntimeFrame::Answer { id, .. }) if Some(id.as_str()) == settle => {
                    Some(Served::Ended)
                }
                _ => None,
            },
            Ok(Message::Close(closed)) => Some(Served::Closed(
                closed.map_or(1005, |c| u16::from(c.code)),
            )),
            Ok(_) => None,
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                // A pong owed to the runtime's ping goes out here.
                self.socket.flush().err().map(|_| Served::Dropped)
            }
            Err(_) => Some(Served::Dropped),
        }
    }

    /// `settle`, up to [`SETTLE_WAIT`] for its answer, then the close with 1000.
    fn settle(&mut self, shared: &Shared, io: &mut Io, outcome: TurnState) -> Served {
        let id = uuid::Uuid::now_v7().to_string();
        let said = self.lives(io, true).and_then(|()| {
            self.send(&PluginFrame::Settle {
                id: id.clone(),
                outcome: Some(outcome),
                continued: false,
                asked: false,
                parked: false,
                waiting: false,
            })
        });
        if said.is_ok() {
            let until = Instant::now() + SETTLE_WAIT;
            while Instant::now() < until {
                if self.read(shared, io, Some(&id)).is_some() {
                    break;
                }
            }
        }
        let _ = self.socket.close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "".into(),
        }));
        let until = Instant::now() + CLOSED_WITHIN;
        while Instant::now() < until {
            match self.socket.read() {
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => break,
            }
        }
        Served::Ended
    }
}

/// The answer to an ask: `stop` ends the turn; an id asked again gets its first answer.
fn asked(shared: &Shared, io: &mut Io, ask: Ask) -> String {
    let (id, op) = match ask {
        Ask::Say { id, .. } => (id, "say"),
        Ask::Pick { id, .. } => (id, "pick"),
        Ask::Stop { id, .. } => (id, "stop"),
        Ask::Force { id, .. } => (id, "force"),
        Ask::Compact { id, .. } => (id, "compact"),
    };
    if let Some(answer) = io.answers.get(&id) {
        return answer.clone();
    }
    let answer = match op {
        "stop" => {
            shared.stop.notify_one();
            PluginFrame::Answer {
                id: id.clone(),
                ok: Some(json!({"stopped": true, "running": []})),
                refused: None,
                error: None,
            }
        }
        _ => PluginFrame::Answer {
            id: id.clone(),
            ok: None,
            refused: Some(refusal::REFUSED.into()),
            error: Some(format!("a one-shot canvas turn takes no {op} ask")),
        },
    };
    let answer = frame(&answer);
    io.answers.insert(id.clone(), answer.clone());
    io.order.push_back(id.clone());
    if io.order.len() > ANSWERS_KEPT
        && let Some(old) = io.order.pop_front()
    {
        io.answers.remove(&old);
    }
    answer
}
