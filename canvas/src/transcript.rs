//! One canvas session's transcript, read as events, and the same records folded from
//! Steery's plugin door.
//!
//! `<CANVAS_HOME>/sessions/<id>.jsonl` is append-only, one JSON record per line, each with
//! `id` (UUIDv7) and `at` (RFC 3339):
//!
//! - `session` (`cwd`, `title`) once, first: the listing's folder and title.
//! - `turn_began` (`turn`): a `turn_began` event under the turn's id.
//! - `words`: the person's words, verbatim.
//! - `line` (`message`, `voice` sketcher or plotter, `text`): one finished stroke line, the
//!   agent's words. `message` (`<turn id>/<voice>`) is the key the door's `live` frames
//!   give the model call that wrote it.
//! - `tokens` (`voice`, `model`, `in`, `cached`, `out`): one model call's count, under the
//!   record's id as its response. The plotter writes the reply, so its model is the
//!   session's `model`.
//! - `turn_ended` (`state`, `failure`): the end of the turn that is open.
//!
//! A `turn_began` while a turn is open ends that one `interrupted`: its process wrote no end.
//!
//! A read's cursor is a byte offset, the last bytes before it to see the file rewritten, and
//! what the records before it left ([`State`]): the open turn, the plotter's last model and
//! the last record's id. A fold's cursor has no offset: it continues after the last record's
//! id, so a read from it finds that record in the transcript first.

use std::fs::File;
use std::io::{self, BufReader, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use steery_driver_common::lines::{BATCH, Line, RECORD, bytes, capped, consumed, first_whole};
use steery_sdk::wire::harness::{
    self, Count, Event, Events, Failure, FailureKind, Folded, TurnState, Voice,
};

/// The voice whose model is the session's.
const ANSWERS: &str = "plotter";
/// Who serves every model the canvas runs.
pub const PROVIDER: &str = "anthropic";
/// How many bytes before a cursor it keeps, to see the file rewritten.
const SEAL: u64 = 32;

/// One record, as the canvas writes it.
#[derive(Deserialize)]
struct Record {
    id: String,
    at: String,
    #[serde(flatten)]
    body: Body,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Body {
    Session {
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        title: Option<String>,
    },
    TurnBegan {
        turn: String,
    },
    Words {
        text: String,
    },
    /// A stroke line of the sketcher or the plotter: both are the agent's words.
    Line {
        message: String,
        text: String,
    },
    Tokens {
        voice: String,
        model: String,
        #[serde(rename = "in")]
        input: u64,
        cached: u64,
        out: u64,
    },
    TurnEnded {
        state: TurnState,
        #[serde(default)]
        failure: Option<Fault>,
    },
    /// A record of a kind this driver does not read.
    #[serde(other)]
    Other,
}

/// Why a turn failed, as the canvas says it in its transcript and its `ended` line.
#[derive(Debug, Clone, Deserialize)]
pub struct Fault {
    pub kind: FailureKind,
    pub line: String,
}

/// What the records before a cursor left.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct State {
    /// The turn that is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn: Option<String>,
    /// The plotter's last model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// The id of the last record read or folded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    v: u32,
    /// Just past the last whole record read; absent in a fold's cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    n: Option<u64>,
    /// The bytes before `n`, in hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    s: Option<String>,
    #[serde(flatten)]
    st: State,
}

impl Cursor {
    fn text(&self) -> io::Result<String> {
        serde_json::to_string(self).map_err(io::Error::other)
    }
}

/// What the session record says, for the listing.
#[derive(Debug, Default)]
pub struct Summary {
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
    pub created_at: Option<u64>,
}

/// `sessions/read` of the transcript at `path`: the events after `cursor` (a read's or a
/// fold's), or of its last `tail` bytes, at most about [`BATCH`] bytes of records.
pub fn read(path: &Path, cursor: Option<&str>, tail: Option<u64>) -> io::Result<Events> {
    let open = File::open(path)?;
    let length = open.metadata()?.len();
    let fallback = modified(path)?;
    let kept = cursor.and_then(|cursor| serde_json::from_str::<Cursor>(cursor).ok());
    let (from, mut state, reset) = match kept {
        Some(kept) if kept.v == 1 => match (kept.n, &kept.st.last) {
            (Some(n), _) if n <= length && Some(seal(&open, n)?) == kept.s => (n, kept.st, false),
            (Some(_), _) => (0, State::default(), true),
            (None, None) => (0, kept.st, false),
            (None, Some(last)) => match end_of(&open, length, last)? {
                Some(n) => (n, kept.st, false),
                // The fold's last record is not in the transcript yet.
                None => {
                    return Ok(Events {
                        cursor: cursor.unwrap_or_default().to_owned(),
                        events: Vec::new(),
                        more: false,
                        reset: false,
                        whole: false,
                        after: None,
                    });
                }
            },
        },
        Some(_) => (0, State::default(), true),
        None => (0, State::default(), cursor.is_some()),
    };
    let from = match (cursor, tail) {
        (None, Some(tail)) => first_whole(&open, length.saturating_sub(tail), length)?,
        _ => from,
    };
    let mut events = Vec::new();
    let (through, more) = scan(&open, from, length, BATCH, |record, _| {
        fold_one(record, fallback, &mut state, &mut events);
        true
    })?;
    let whole = from == 0;
    let after = match whole || cursor.is_some() {
        true => state.last.clone(),
        // A tail begins past records it did not read.
        false => None,
    };
    Ok(Events {
        cursor: Cursor {
            v: 1,
            n: Some(through),
            s: Some(seal(&open, through)?),
            st: state,
        }
        .text()?,
        events,
        more,
        reset,
        whole,
        after,
    })
}

/// `sessions/fold` of the records the door sent, after `cursor` (a fold's or a read's),
/// folded as [`read`] folds the same lines. A record sent as its id alone is read from the
/// transcript at `path`; one it does not hold yet is left, with those after it, for the
/// next fold.
pub fn fold(path: &Path, cursor: Option<&str>, records: Vec<harness::Record>) -> io::Result<Folded> {
    let mut state = cursor
        .and_then(|cursor| serde_json::from_str::<Cursor>(cursor).ok())
        .filter(|kept| kept.v == 1)
        .map(|kept| kept.st)
        .unwrap_or_default();
    let now = now();
    let mut events = Vec::new();
    let mut consumed = 0u32;
    for sent in records {
        let record = match sent.value {
            Some(value) => serde_json::from_value::<Record>(value).ok(),
            None => match kept(path, &sent.id)? {
                Some(record) => Some(record),
                None => break,
            },
        };
        match record {
            Some(record) => fold_one(record, now, &mut state, &mut events),
            // As a read passes over a record that does not parse.
            None => state.last = Some(sent.id),
        }
        consumed += 1;
    }
    let cursor = match consumed {
        0 => cursor.map(str::to_owned),
        _ => None,
    };
    Ok(Folded {
        cursor: match cursor {
            Some(cursor) => cursor,
            None => Cursor {
                v: 1,
                n: None,
                s: None,
                st: state,
            }
            .text()?,
        },
        events,
        consumed,
    })
}

/// The transcript's own copy of the record `id`, where it holds it yet.
fn kept(path: &Path, id: &str) -> io::Result<Option<Record>> {
    let open = match File::open(path) {
        Ok(open) => open,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let length = open.metadata()?.len();
    let mut found = None;
    scan(&open, 0, length, u64::MAX, |record, _| {
        if record.id == id {
            found = Some(record);
            return false;
        }
        true
    })?;
    Ok(found)
}

/// Just past the record `id`, where the transcript holds it.
fn end_of(open: &File, length: u64, id: &str) -> io::Result<Option<u64>> {
    let mut found = None;
    scan(open, 0, length, u64::MAX, |record, end| {
        if record.id == id {
            found = Some(end);
            return false;
        }
        true
    })?;
    Ok(found)
}

/// Every event of the transcript at `path`, from its start.
pub fn events(path: &Path) -> io::Result<Vec<Event>> {
    let open = File::open(path)?;
    let length = open.metadata()?.len();
    let fallback = modified(path)?;
    let (mut state, mut events) = (State::default(), Vec::new());
    scan(&open, 0, length, u64::MAX, |record, _| {
        fold_one(record, fallback, &mut state, &mut events);
        true
    })?;
    Ok(events)
}

/// What the transcript's first record, the session's, says.
pub fn summary(path: &Path) -> io::Result<Summary> {
    let open = File::open(path)?;
    let length = open.metadata()?.len();
    let mut summary = Summary::default();
    scan(&open, 0, length, u64::MAX, |record, _| {
        if let Body::Session { cwd, title } = record.body {
            summary = Summary {
                cwd,
                title,
                created_at: stamp(&record.at),
            };
        }
        false
    })?;
    Ok(summary)
}

/// Each whole record of `open` from `from`, with where it ends, until about `budget` bytes
/// are read or `each` answers false: where the read stopped, and whether whole records remain
/// past it. A record that does not parse, or is over [`RECORD`] bytes, is passed over.
fn scan(
    mut open: &File,
    from: u64,
    length: u64,
    budget: u64,
    mut each: impl FnMut(Record, u64) -> bool,
) -> io::Result<(u64, bool)> {
    open.seek(SeekFrom::Start(from))?;
    let total = length - from;
    let mut reader = BufReader::new(open.take(total));
    let mut line = Vec::new();
    let mut through = from;
    loop {
        if through - from > budget {
            return Ok((through, through < length));
        }
        let read = capped(&mut reader, &mut line, RECORD)?;
        if read == Line::Partial {
            return Ok((through, false));
        }
        through = from + consumed(&reader, total);
        if read == Line::Whole
            && let Ok(record) = serde_json::from_slice::<Record>(&line)
            && !each(record, through)
        {
            return Ok((through, through < length));
        }
    }
}

/// The events of one record, after those before it left `state`.
fn fold_one(record: Record, fallback: u64, state: &mut State, out: &mut Vec<Event>) {
    let at = stamp(&record.at).unwrap_or(fallback);
    let id = record.id;
    state.last = Some(id.clone());
    match record.body {
        Body::Session { .. } | Body::Other => {}
        Body::TurnBegan { turn } => {
            if let Some(open) = state.turn.take() {
                out.push(Event::TurnEnded {
                    at,
                    turn: Some(open),
                    state: TurnState::Interrupted,
                    failure: None,
                });
            }
            state.turn = Some(turn.clone());
            out.push(Event::TurnBegan {
                at,
                turn: Some(turn),
            });
        }
        Body::Words { text } => out.push(Event::Words {
            at,
            id,
            voice: Voice::Person,
            text,
            message: None,
            call: None,
            clips: Vec::new(),
        }),
        Body::Line { message, text } => out.push(line(at, id, message, text)),
        Body::Tokens {
            voice,
            model,
            input,
            cached,
            out: output,
        } => {
            if voice == ANSWERS && state.model.as_deref() != Some(model.as_str()) {
                state.model = Some(model.clone());
                out.push(Event::Model {
                    at,
                    model: model.clone(),
                    provider: Some(PROVIDER.into()),
                    thinking: None,
                });
            }
            out.push(tokens(at, id, model, input, cached, output));
        }
        Body::TurnEnded {
            state: ended,
            failure,
        } => out.push(Event::TurnEnded {
            at,
            turn: state.turn.take(),
            state: ended,
            failure: failure
                .filter(|_| ended == TurnState::Failed)
                .map(|fault| self::failure(fault, state.model.clone())),
        }),
    }
}

/// A stroke line: the agent's words, of the model call `message` names.
pub fn line(at: u64, id: String, message: String, text: String) -> Event {
    Event::Words {
        at,
        id,
        voice: Voice::Agent,
        text,
        message: Some(message),
        call: None,
        clips: Vec::new(),
    }
}

/// One model call's count, its record's id as its response. `in` counts the cached tokens.
pub fn tokens(at: u64, id: String, model: String, input: u64, cached: u64, output: u64) -> Event {
    Event::Tokens {
        at,
        count: Count::Response,
        response: Some(id),
        model: Some(model),
        input,
        cached,
        cache_write: None,
        output,
        reasoning: 0,
        context_left: None,
    }
}

/// A turn's failure. Every kind but `other` comes from Claude: a refused sign-in, a limit, a
/// prompt over the model's limit.
pub fn failure(fault: Fault, model: Option<String>) -> Failure {
    Failure {
        service: fault.kind != FailureKind::Other,
        kind: fault.kind,
        line: fault.line,
        http_status: None,
        code: None,
        retry_at: None,
        provider: Some(PROVIDER.into()),
        model,
        thinking: None,
    }
}

/// An RFC 3339 time in milliseconds since 1970.
fn stamp(at: &str) -> Option<u64> {
    let time = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    u64::try_from(time.timestamp_millis()).ok()
}

/// The bytes just before `n`, in hex.
fn seal(open: &File, n: u64) -> io::Result<String> {
    Ok(bytes(open, n.saturating_sub(SEAL), n)?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// When the file last changed, in milliseconds since 1970.
pub fn modified(path: &Path) -> io::Result<u64> {
    let modified = std::fs::metadata(path)?.modified()?;
    Ok(modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)))
}

/// Now, in milliseconds since 1970.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Whether a record is the person's words carrying `marker`, for a receipt.
pub fn carries(record: &serde_json::Value, marker: &str) -> bool {
    record["kind"] == "words"
        && record["text"]
            .as_str()
            .is_some_and(|text| steery_driver_common::receipt::marked(text, marker))
}
