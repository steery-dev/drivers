//! One of Aider's chat history files, read as one conversation.
//!
//! Aider appends every run to a Markdown file, `<git root>/.aider.chat.history.md` unless
//! `--chat-history-file` names another (`io.py`, `append_chat_history`). The file has no ids
//! and one stamp per run:
//!
//! - `# aider chat started at 2026-11-14 16:15:40`, in the machine's local time, opens a run.
//! - `#### ` lines are the person's words, one line each (`user_input`).
//! - `> ` lines are Aider's own output (`tool_output`, `tool_warning`, `tool_error`): the
//!   run's command line, its model, its prompts and their answers, applied edits, commits,
//!   errors and `Tokens: 721 sent, 331 received.` after each model call.
//! - Every other line is the model's answer, written whole once the answer is done
//!   (`ai_output`), its reasoning between `<thinking-content-…>` tags (`reasoning_tags.py`).
//!
//! `--restore-chat-history` loads the messages of the whole file (`utils.py`,
//! `split_chat_history_markdown`), so the file is what Aider continues: one conversation.
//! A turn runs from the person's words to the next words or the next run. Its end is read
//! from what follows it, so the last turn stays open until the next turn begins, Aider
//! exits (the turn mode reads the file then) or `idle` reads it unheld. Every event of a
//! run carries the run's stamp, the only one Aider writes.

use std::fs::File;
use std::io;
use std::path::Path;

use chrono::{Local, NaiveDateTime, TimeZone as _};
use serde::{Deserialize, Serialize};
use steery_driver_common::lines::{BATCH, RECORD, bytes};
use steery_sdk::wire::harness::{Count, Event, Events, Failure, FailureKind, TurnState, Voice};

/// What opens a run.
const STARTED: &str = "# aider chat started at ";
/// What opens the person's words, and Aider's own output.
const PERSON: &str = "####";
const NOTICE: &str = ">";
/// The tag that wraps a reasoning model's thinking (`reasoning_tags.py`, `REASONING_TAG`).
const THINKING: &str = "thinking-content-";
/// What Aider writes as Ctrl-C ends a model call or the run (`base_coder.py`,
/// `keyboard_interrupt`).
const INTERRUPTED: [&str; 2] = ["^C again to exit", "^C KeyboardInterrupt"];
/// The arguments that give Aider its words on the command line: a run with nobody at it.
const MESSAGES: [&str; 5] = ["-m", "--message", "--msg", "-f", "--message-file"];
/// The bytes before a cursor whose hash it keeps, to see the file rewritten.
const KEPT: u64 = 256;

/// One record of the file: a block of lines of one kind.
#[derive(Debug, Clone, PartialEq)]
enum Record {
    /// A run begins, at this time (ms), where its stamp reads.
    Started(Option<u64>),
    /// The person's words.
    Person(String),
    /// Aider's own output lines, each without its `> `.
    Notices(Vec<String>),
    /// The model's answer.
    Answer(String),
}

/// A record and where it starts.
#[derive(Debug, Clone, PartialEq)]
struct Placed {
    start: u64,
    record: Record,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Blank,
    Started,
    Person,
    Notice,
    Answer,
}

fn kind(line: &str) -> Kind {
    if line.trim().is_empty() {
        Kind::Blank
    } else if line.starts_with(STARTED) {
        Kind::Started
    } else if line == PERSON || line.starts_with("#### ") {
        Kind::Person
    } else if line == NOTICE || line.starts_with("> ") {
        Kind::Notice
    } else {
        Kind::Answer
    }
}

/// A line without its mark and the two spaces Aider ends it with.
fn body<'a>(line: &'a str, mark: &str) -> &'a str {
    let line = line.strip_prefix(mark).unwrap_or(line);
    line.strip_prefix(' ').unwrap_or(line).trim_end()
}

/// When a run's stamp says it began, read in this machine's time zone.
fn stamp(line: &str) -> Option<u64> {
    let text = line.strip_prefix(STARTED)?.trim();
    let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").ok()?;
    let at = Local.from_local_datetime(&naive).earliest()?;
    u64::try_from(at.timestamp_millis()).ok()
}

/// The records of `text`, which starts at byte `base` of the file, and how many of its
/// bytes they cover. Only whole lines count. A record still being written stays out:
/// one at the end of `text` that its next line could still carry on, unless `whole` (the
/// file ends here and nothing writes it). The person's words and Aider's lines are each
/// written in one go, so they end with the file; an answer can hold blank lines, so it
/// ends only where another record begins.
fn records(text: &[u8], base: u64, ends: bool, whole: bool) -> (Vec<Placed>, usize) {
    let mut placed = Vec::new();
    // The open block: its kind, where it starts, its lines and the blank lines after it.
    let mut open: Option<(Kind, usize, Vec<String>, usize)> = None;
    let mut done = 0;
    let mut at = 0;
    let close = |open: &mut Option<(Kind, usize, Vec<String>, usize)>, placed: &mut Vec<Placed>| {
        if let Some((kind, start, lines, _)) = open.take() {
            let record = match kind {
                Kind::Person => Record::Person(lines.join("\n")),
                Kind::Notice => Record::Notices(lines),
                _ => Record::Answer(lines.join("\n").trim_end().to_owned()),
            };
            placed.push(Placed {
                start: base + start as u64,
                record,
            });
        }
    };
    while let Some(newline) = text[at..].iter().position(|&b| b == b'\n') {
        let end = at + newline + 1;
        let line = String::from_utf8_lossy(&text[at..at + newline]);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        match kind(line) {
            Kind::Blank => match &mut open {
                Some((Kind::Answer, _, _, blanks)) => *blanks += 1,
                Some(_) => {
                    close(&mut open, &mut placed);
                    done = end;
                }
                None => done = end,
            },
            Kind::Started => {
                close(&mut open, &mut placed);
                placed.push(Placed {
                    start: base + at as u64,
                    record: Record::Started(stamp(line)),
                });
                done = end;
            }
            kind => {
                let text = match kind {
                    Kind::Person => body(line, PERSON),
                    Kind::Notice => body(line, NOTICE),
                    _ => line.trim_end(),
                };
                match &mut open {
                    Some((open_kind, _, lines, blanks)) if *open_kind == kind => {
                        lines.extend(std::iter::repeat_n(String::new(), *blanks));
                        *blanks = 0;
                        lines.push(text.to_owned());
                    }
                    _ => {
                        close(&mut open, &mut placed);
                        open = Some((kind, at, vec![text.to_owned()], 0));
                    }
                }
            }
        }
        at = end;
    }
    match &open {
        Some((Kind::Answer, start, ..)) if !(ends && whole) => done = *start,
        Some((_, start, ..)) if !ends => done = *start,
        Some(_) => {
            close(&mut open, &mut placed);
            done = at;
        }
        None => {}
    }
    (placed, done)
}

/// Where the reading stands between records: what the records before it said.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// When the run began, in milliseconds: the stamp of every event of the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    at: Option<u64>,
    /// The model the run names, and its thinking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking: Option<String>,
    /// The open turn: the start of the person's words that began it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn: Option<u64>,
    /// The turn's last model call failed, and no answer came after it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure: Option<Failure>,
    /// Ctrl-C interrupted the turn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    interrupted: bool,
}

/// What a whole file says of its conversation, for its listing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    /// When its first run began.
    pub created_at: Option<u64>,
    /// The first line of the person's first words.
    pub title: Option<String>,
    /// The model of its last run, and that run's thinking.
    pub model: Option<String>,
    pub thinking: Option<String>,
    /// A run that a person was at: one whose command line gave Aider no words, or one whose
    /// command line Aider did not write.
    pub person: bool,
    /// How many runs it has.
    pub runs: usize,
}

/// The model a run announces: `Model: ollama_chat/qwen3:4b with whole edit format` (or
/// `Main model:` beside a weak model), and its thinking from `, reasoning high` or
/// `, 8k think tokens` (`base_coder.py`, `get_announcements`).
fn announced(line: &str) -> Option<(String, Option<String>)> {
    let rest = line
        .strip_prefix("Model: ")
        .or_else(|| line.strip_prefix("Main model: "))?;
    let (model, rest) = rest.split_once(" with ")?;
    let thinking = rest.split(", ").find_map(|part| {
        part.strip_prefix("reasoning ")
            .or_else(|| part.strip_suffix(" think tokens"))
            .map(str::to_owned)
    });
    Some((model.to_owned(), thinking))
}

/// Whether a line is the command line Aider logs at its start, and whether that run gave
/// Aider its words on the command line.
fn command(line: &str) -> Option<bool> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let named = words.iter().take(2).any(|word| {
        Path::new(word)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "aider" || name == "aider.exe")
    });
    named.then(|| {
        words.iter().any(|word| {
            MESSAGES.contains(word)
                || MESSAGES
                    .iter()
                    .any(|flag| flag.starts_with("--") && word.starts_with(&format!("{flag}=")))
        })
    })
}

/// A failed model call, as Aider prints litellm's error: `litellm.AuthenticationError:
/// AuthenticationError: OpenAIException - …`.
fn failure(line: &str, state: &State) -> Option<Failure> {
    let (name, _) = line.split_once(':')?;
    if name.contains(' ') || !(name.ends_with("Error") || name.ends_with("Exception")) {
        return None;
    }
    let kind = if name.contains("Authentication") || name.contains("PermissionDenied") {
        FailureKind::SignIn
    } else if name.contains("RateLimit") {
        FailureKind::RateLimit
    } else if name.contains("ContextWindowExceeded") {
        FailureKind::ContextFull
    } else {
        FailureKind::Other
    };
    Some(Failure {
        kind,
        line: line.to_owned(),
        service: name.starts_with("litellm."),
        http_status: None,
        code: Some(name.rsplit('.').next().unwrap_or(name).to_owned()),
        retry_at: None,
        provider: state.model.as_deref().and_then(provider).map(str::to_owned),
        model: state.model.clone(),
        thinking: state.thinking.clone(),
    })
}

/// The provider of a model as litellm spells it: what comes before its `/`.
pub fn provider(model: &str) -> Option<&str> {
    model.split_once('/').map(|(provider, _)| provider)
}

/// A count as Aider prints it (`utils.py`, `format_tokens`): `831`, `1.1k`, `12k`.
fn count(text: &str) -> Option<u64> {
    match text.strip_suffix('k') {
        Some(thousands) => {
            let value: f64 = thousands.parse().ok()?;
            Some((value * 1000.0).round() as u64)
        }
        None => text.parse().ok(),
    }
}

/// `Tokens: 2.3k sent, 1.1k cache write, 4.5k cache hit, 275 received.`, one model call's
/// count (`base_coder.py`, `calculate_and_show_tokens_and_cost`).
fn tokens(line: &str) -> Option<(u64, u64, Option<u64>, u64)> {
    let rest = line.strip_prefix("Tokens: ")?;
    let rest = rest.split(". Cost:").next()?.trim_end_matches('.');
    let (mut sent, mut cached, mut written, mut received) = (None, 0, None, None);
    for part in rest.split(", ") {
        let (number, what) = part.split_once(' ')?;
        let number = count(number)?;
        match what {
            "sent" => sent = Some(number),
            "cache hit" => cached = number,
            "cache write" => written = Some(number),
            "received" => received = Some(number),
            _ => return None,
        }
    }
    Some((sent?, cached, written, received?))
}

/// Thinking and answer of the model's words, each where it is there.
fn thought(answer: &str) -> (Option<String>, Option<String>) {
    let open = format!("<{THINKING}");
    let Some(begin) = answer.find(&open) else {
        return (None, Some(answer.to_owned()).filter(|a| !a.is_empty()));
    };
    let after_tag = answer[begin..].find('>').map(|at| begin + at + 1);
    let Some(inner) = after_tag else {
        return (None, Some(answer.to_owned()));
    };
    let close = format!("</{THINKING}");
    let (thinking, rest) = match answer[inner..].find(&close) {
        Some(at) => {
            let end = inner + at;
            let after = answer[end..]
                .find('>')
                .map_or(answer.len(), |at| end + at + 1);
            (
                &answer[inner..end],
                format!("{}{}", &answer[..begin], &answer[after..]),
            )
        }
        None => (&answer[inner..], answer[..begin].to_owned()),
    };
    let nonempty = |text: &str| Some(text.trim().to_owned()).filter(|t| !t.is_empty());
    (nonempty(thinking), nonempty(&rest))
}

/// The events of `records`, read on from `state`, kept from byte `from` on.
fn events(records: &[Placed], state: &mut State, from: u64, fallback: u64, out: &mut Vec<Event>) {
    for placed in records {
        let keep = placed.start >= from;
        let at = state.at.unwrap_or(fallback);
        let id = placed.start.to_string();
        match &placed.record {
            Record::Started(stamp) => {
                end(state, at, keep, out);
                *state = State {
                    at: stamp.or(state.at),
                    ..State::default()
                };
            }
            Record::Person(text) => {
                end(state, at, keep, out);
                state.turn = Some(placed.start);
                if keep {
                    out.push(Event::TurnBegan {
                        at,
                        turn: Some(id.clone()),
                    });
                    out.push(Event::Words {
                        at,
                        id,
                        voice: Voice::Person,
                        text: text.clone(),
                        message: None,
                        call: None,
                        clips: Vec::new(),
                    });
                }
            }
            Record::Answer(text) => {
                state.failure = None;
                let (thinking, answer) = thought(text);
                if keep {
                    if let Some(text) = thinking {
                        out.push(words(at, format!("{id}t"), Voice::Thought, text));
                    }
                    if let Some(text) = answer {
                        out.push(words(at, id, Voice::Agent, text));
                    }
                }
            }
            Record::Notices(lines) => {
                let mut said = Vec::new();
                for line in lines {
                    if state.turn.is_none() {
                        // Before the run's first words: what the run is, never a turn's.
                        if let Some((model, thinking)) = announced(line) {
                            state.model = Some(model.clone());
                            state.thinking = thinking.clone();
                            if keep {
                                out.push(Event::Model {
                                    at,
                                    provider: provider(&model).map(str::to_owned),
                                    model,
                                    thinking,
                                });
                            }
                        }
                        continue;
                    }
                    if let Some((input, cached, cache_write, output)) = tokens(line) {
                        if keep {
                            out.push(Event::Tokens {
                                at,
                                count: Count::Response,
                                response: None,
                                model: state.model.clone(),
                                input,
                                cached,
                                cache_write,
                                output,
                                reasoning: 0,
                                context_left: None,
                            });
                        }
                        continue;
                    }
                    if line.starts_with("Cost: $") {
                        continue;
                    }
                    if INTERRUPTED.iter().any(|said| line.contains(said)) {
                        state.interrupted = true;
                    } else if let Some(failed) = failure(line, state) {
                        state.failure = Some(failed);
                    }
                    if !line.is_empty() {
                        said.push(line.as_str());
                    }
                }
                if keep && !said.is_empty() {
                    out.push(words(at, id, Voice::Thought, said.join("\n")));
                }
            }
        }
    }
}

fn words(at: u64, id: String, voice: Voice, text: String) -> Event {
    Event::Words {
        at,
        id,
        voice,
        text,
        message: None,
        call: None,
        clips: Vec::new(),
    }
}

/// End the open turn, as what follows it shows.
fn end(state: &mut State, at: u64, keep: bool, out: &mut Vec<Event>) {
    let Some(turn) = state.turn.take() else {
        return;
    };
    let failure = state.failure.take();
    let interrupted = std::mem::take(&mut state.interrupted);
    if keep {
        out.push(Event::TurnEnded {
            at,
            turn: Some(turn.to_string()),
            state: if interrupted {
                TurnState::Interrupted
            } else if failure.is_some() {
                TurnState::Failed
            } else {
                TurnState::Finished
            },
            failure: failure.filter(|_| !interrupted),
        });
    }
}

/// A cursor: the byte the next read starts at, a hash of the bytes before it, and the
/// state of the reading there.
#[derive(Debug, Serialize, Deserialize)]
struct Cursor {
    v: u32,
    n: u64,
    h: String,
    #[serde(default)]
    st: State,
}

/// FNV-1a of the bytes before `n`, at most [`KEPT`] of them.
fn hash(open: &File, n: u64) -> io::Result<String> {
    let bytes = bytes(open, n.saturating_sub(KEPT), n)?;
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    Ok(format!("{hash:016x}"))
}

/// The file's last change, in milliseconds since 1970.
pub fn modified(path: &Path) -> io::Result<u64> {
    let modified = std::fs::metadata(path)?.modified()?;
    Ok(modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)))
}

/// `sessions/read` of one file. `whole` says that nothing writes the file now, so its last
/// turn ends with it (the turn mode once Aider exited, `idle`).
pub fn read(
    path: &Path,
    cursor: Option<&str>,
    tail: Option<u64>,
    whole: bool,
) -> io::Result<Events> {
    let open = File::open(path)?;
    let length = open.metadata()?.len();
    let fallback = modified(path)?;
    let kept = cursor.and_then(|cursor| serde_json::from_str::<Cursor>(cursor).ok());
    let (mut from, mut state, reset) = match kept {
        Some(cursor)
            if cursor.v == 1 && cursor.n <= length && hash(&open, cursor.n)? == cursor.h =>
        {
            (cursor.n, cursor.st, false)
        }
        Some(_) => (0, State::default(), true),
        None => (0, State::default(), cursor.is_some()),
    };
    // A tail reads the file from its start, for the state of where it begins, and keeps
    // only the events of its last bytes.
    let keep_from = match (cursor, tail) {
        (None, Some(tail)) => length.saturating_sub(tail),
        _ => 0,
    };
    let mut out = Vec::new();
    loop {
        let mut end = length.min(from + BATCH);
        let (placed, done) = loop {
            let text = bytes(&open, from, end)?;
            let ends = end == length;
            let (placed, done) = records(&text, from, ends, whole);
            if done > 0 || ends {
                break (placed, done);
            }
            if end - from >= RECORD as u64 {
                return Err(io::Error::other(format!(
                    "a record of {} is over {} bytes",
                    path.display(),
                    RECORD
                )));
            }
            end = length.min(from + RECORD as u64);
        };
        events(&placed, &mut state, keep_from, fallback, &mut out);
        from += done as u64;
        let more = from < length && end < length;
        if whole && from == length {
            let at = state.at.unwrap_or(fallback);
            end_last(&mut state, at, &mut out);
        }
        if !more || keep_from == 0 {
            return Ok(Events {
                cursor: serde_json::to_string(&Cursor {
                    v: 1,
                    n: from,
                    h: hash(&open, from)?,
                    st: state,
                })
                .map_err(io::Error::other)?,
                events: out,
                more,
                reset,
                whole: keep_from == 0 && (cursor.is_none() || reset),
                after: None,
            });
        }
    }
}

/// The last turn of a file nothing writes ends with it.
fn end_last(state: &mut State, at: u64, out: &mut Vec<Event>) {
    end(state, at, true, out);
}

/// What the whole of `path` says for its listing.
pub fn summary(path: &Path) -> io::Result<Summary> {
    let open = File::open(path)?;
    let length = open.metadata()?.len();
    let mut summary = Summary::default();
    let mut from = 0;
    // The current run gave Aider its words on the command line.
    let mut headless: Option<bool> = None;
    let mut model_seen = false;
    let close_run = |headless: &mut Option<bool>, summary: &mut Summary| {
        if summary.runs > 0 && *headless != Some(true) {
            summary.person = true;
        }
        *headless = None;
    };
    while from < length {
        let end = length.min(from + BATCH);
        let text = bytes(&open, from, end)?;
        let (placed, done) = records(&text, from, end == length, true);
        for placed in &placed {
            match &placed.record {
                Record::Started(stamp) => {
                    close_run(&mut headless, &mut summary);
                    summary.runs += 1;
                    model_seen = false;
                    if summary.created_at.is_none() {
                        summary.created_at = *stamp;
                    }
                }
                Record::Person(text) => {
                    if summary.title.is_none() {
                        summary.title = text
                            .lines()
                            .map(str::trim)
                            .find(|line| !line.is_empty())
                            .map(|line| line.chars().take(80).collect());
                    }
                }
                Record::Notices(lines) => {
                    for line in lines {
                        if headless.is_none()
                            && let Some(said) = command(line)
                        {
                            headless = Some(said);
                        }
                        if !model_seen && let Some((model, thinking)) = announced(line) {
                            model_seen = true;
                            summary.model = Some(model);
                            summary.thinking = thinking;
                        }
                    }
                }
                Record::Answer(_) => {}
            }
        }
        if done == 0 {
            break;
        }
        from += done as u64;
    }
    if summary.runs == 0 {
        summary.runs = 1;
    }
    close_run(&mut headless, &mut summary);
    Ok(summary)
}

/// The person's words after byte `after`, each with where it starts.
pub fn said(path: &Path, after: u64) -> io::Result<Vec<String>> {
    let open = File::open(path)?;
    let length = open.metadata()?.len();
    let text = bytes(&open, after.min(length), length)?;
    let (placed, _) = records(&text, after, true, true);
    Ok(placed
        .into_iter()
        .filter_map(|placed| match placed.record {
            Record::Person(text) => Some(text),
            _ => None,
        })
        .collect())
}

/// The file's length: a point after everything it holds now.
pub fn length(path: &Path) -> io::Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}
