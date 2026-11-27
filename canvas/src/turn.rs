//! The canvas's headless harness: one turn, run by this driver's own binary in its turn mode
//! (API.md §8.2 to §8.4), `steery-driver-canvas turn [--session <id>] [--cwd <dir>]`.
//!
//! The message on standard input goes to the sketcher, then to the plotter with the sketch's
//! lines ([`crate::claude`]). Every non-empty line of a drawer's text that is not a `#`
//! comment is one `line`, verbatim: Steery's server parses the strokes and refuses those it
//! does not take. Each record goes to the session's transcript ([`crate::transcript`]), to
//! standard output as a JSON line, and to Steery's plugin door ([`crate::door`]) where
//! `steery.json` names one.
//!
//! SIGINT, SIGTERM or the door's `stop` end the turn as `interrupted`.

use std::cell::RefCell;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use steery_driver_common::kernel;
use steery_sdk::wire::door::LiveStop;
use steery_sdk::wire::harness::{FailureKind, LiveSettled, LiveState, LiveTurn, TurnState};
use tokio::io::AsyncReadExt as _;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Notify;

use crate::claude::{Failure, Line, Source};
use crate::door::{self, Door};
use crate::models::{PLOTTER, SKETCHER};
use crate::{home, transcript};

/// The word that starts the turn mode, as `start`'s plan runs this binary.
pub const MODE: &str = "turn";

const SKETCHER_PROMPT: &str = include_str!("../prompts/SKETCHER.md");
const PLOTTER_PROMPT: &str = include_str!("../prompts/PLOTTER.md");

/// The head of a composed message's outline part (API.md §4.1).
const OUTLINE_HEAD: &str = "Canvas outline:\n";
/// The longest title a session record keeps, in characters.
const TITLE: usize = 60;
/// The longest error a settled state keeps, in characters.
const SETTLED_ERROR: usize = 600;

/// A fresh UUIDv7.
fn fresh() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Now in RFC 3339, as a record's `at`.
fn now_at() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// One JSON line on standard output, at once.
fn say(line: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The turn mode, `args` after its own word.
pub async fn run(args: impl Iterator<Item = String>) -> ExitCode {
    let (mut session, mut cwd) = (None, None);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--session", Some(id)) => session = Some(id),
            ("--cwd", Some(dir)) => cwd = Some(PathBuf::from(dir)),
            _ => {
                eprintln!("usage: steery-driver-canvas turn [--session <id>] [--cwd <dir>]");
                return ExitCode::from(2);
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let root = home::root(&home);
    let id = session.unwrap_or_else(fresh);
    say(&json!({"named": id}));
    let cwd = match cwd.map_or_else(std::env::current_dir, std::path::absolute) {
        Ok(cwd) => cwd,
        Err(e) => return refused(&format!("no folder to run in: {e}")),
    };
    let Ok(transcript) = home::transcript_to_be(&root, &id) else {
        return refused(&format!("{id} cannot name a canvas session"));
    };
    if let Err(e) = std::fs::create_dir_all(home::sessions(&root)) {
        return refused(&format!("cannot make the sessions' folder: {e}"));
    }
    let holder = home::holder(&root, &id);
    if let Some(pid) = std::fs::read_to_string(&holder)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .filter(|pid| *pid != std::process::id() && kernel::started(*pid).is_some())
    {
        return refused(&format!("the session {id} has a turn running (pid {pid})"));
    }
    if let Err(e) = std::fs::write(&holder, std::process::id().to_string()) {
        return refused(&format!("cannot write {}: {e}", holder.display()));
    }
    let code = Turn::new(id, cwd, &root, transcript).run().await;
    let _ = std::fs::remove_file(&holder);
    ExitCode::from(code)
}

/// A turn that fails at once, with a fresh `id` and the time now, writing nothing.
fn refused(line: &str) -> ExitCode {
    say(&json!({"ended": {
        "id": fresh(),
        "at": now_at(),
        "state": TurnState::Failed,
        "failure": {"kind": FailureKind::Other, "line": line, "service": Source::read().service()},
    }}));
    ExitCode::FAILURE
}

/// The model call in hand: its key and its lines so far, joined with `"\n"`.
struct Open {
    key: String,
    text: String,
}

struct Turn {
    turn: String,
    cwd: PathBuf,
    transcript: PathBuf,
    began_at: u64,
    source: Source,
    door: Option<Door>,
    stop: Arc<Notify>,
    open: RefCell<Option<Open>>,
}

impl Turn {
    fn new(id: String, cwd: PathBuf, root: &Path, transcript: PathBuf) -> Self {
        let began_at = transcript::now();
        let stop = Arc::new(Notify::new());
        let door = door::url(root).map(|url| {
            let records = std::fs::read_to_string(&transcript)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|record| record["id"].is_string())
                .collect();
            Door::open(
                url,
                id,
                cwd.clone(),
                records,
                running(began_at, SKETCHER.model()),
                Arc::clone(&stop),
            )
        });
        Self {
            turn: fresh(),
            cwd,
            transcript,
            began_at,
            source: Source::read(),
            door,
            stop,
            open: RefCell::new(None),
        }
    }

    /// Appends a record of `kind` to the transcript and hands it to the door.
    fn record(&self, kind: &str, fields: Value) -> Map<String, Value> {
        let mut record = Map::new();
        record.insert("id".into(), fresh().into());
        record.insert("at".into(), now_at().into());
        record.insert("kind".into(), kind.into());
        if let Value::Object(fields) = fields {
            record.extend(fields);
        }
        let line = Value::Object(record);
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.transcript)
            .and_then(|mut file| writeln!(file, "{line}"));
        if let Err(e) = written {
            eprintln!("cannot write {}: {e}", self.transcript.display());
        }
        if let Some(door) = &self.door {
            door.append(line.clone());
        }
        let Value::Object(record) = line else {
            unreachable!("a record is an object")
        };
        record
    }

    /// The record's `id` and `at`, with `fields`, as the JSON line `name` says it.
    fn said(name: &str, record: &Map<String, Value>, fields: Value) -> Value {
        let mut body = Map::new();
        body.insert("id".into(), record["id"].clone());
        body.insert("at".into(), record["at"].clone());
        if let Value::Object(fields) = fields {
            body.extend(fields);
        }
        json!({ name: body })
    }

    async fn run(mut self) -> u8 {
        let ended = {
            let work = self.work();
            tokio::pin!(work);
            let mut interrupt = signal(SignalKind::interrupt()).ok();
            let mut terminate = signal(SignalKind::terminate()).ok();
            let signalled = async {
                match (&mut interrupt, &mut terminate) {
                    (Some(interrupt), Some(terminate)) => tokio::select! {
                        _ = interrupt.recv() => {},
                        _ = terminate.recv() => {},
                    },
                    _ => std::future::pending().await,
                }
            };
            tokio::select! {
                ended = &mut work => ended,
                () = signalled => Err(None),
                () = self.stop.notified() => Err(None),
            }
        };
        let (state, failure) = match ended {
            Ok(()) => (TurnState::Finished, None),
            Err(None) => {
                if let Some(open) = self.open.take() {
                    self.live(&open, Some(LiveStop::Aborted));
                }
                (TurnState::Interrupted, None)
            }
            Err(Some(failure)) => (TurnState::Failed, Some(failure)),
        };
        self.end(state, failure)
    }

    /// The turn's work: its records, then the sketch, then the refine. `Err(None)`: it was
    /// interrupted.
    async fn work(&self) -> Result<(), Option<Failure>> {
        let mut message = String::new();
        if let Err(e) = tokio::io::stdin().read_to_string(&mut message).await {
            return Err(Some(Failure::other(
                self.source.service(),
                format!("cannot read the message on stdin: {e}"),
            )));
        }
        if !self.transcript.exists() {
            let title: String = message
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(TITLE)
                .collect();
            self.record("session", json!({"cwd": self.cwd, "title": title}));
        }
        self.record("turn_began", json!({"turn": self.turn}));
        self.record("words", json!({"text": message}));
        if message.trim().is_empty() {
            return Err(Some(Failure::other(
                self.source.service(),
                "no message on stdin".into(),
            )));
        }
        let (sketcher, plotter) = (SKETCHER.model(), PLOTTER.model());
        // Both lines open at once: on a plan, the plotter's `claude` starts while the
        // sketcher draws.
        let mut sketching =
            Line::open(&self.source, sketcher.clone(), SKETCHER_PROMPT, &self.cwd).map_err(Some)?;
        let mut refining =
            Line::open(&self.source, plotter.clone(), PLOTTER_PROMPT, &self.cwd).map_err(Some)?;
        let sketch = self
            .draw(&mut sketching, SKETCHER.role, &sketcher, &sketch_message(&message))
            .await
            .map_err(Some)?;
        drop(sketching);
        self.draw(
            &mut refining,
            PLOTTER.role,
            &plotter,
            &refine_message(&message, &sketch),
        )
        .await
        .map_err(Some)?;
        Ok(())
    }

    /// One model call of `voice`: each of its lines out as it ends; the lines it drew.
    async fn draw(
        &self,
        line: &mut Line,
        voice: &str,
        model: &str,
        message: &str,
    ) -> Result<Vec<String>, Failure> {
        let key = format!("{}/{voice}", self.turn);
        *self.open.borrow_mut() = Some(Open {
            key: key.clone(),
            text: String::new(),
        });
        if let Some(door) = &self.door {
            door.state(running(self.began_at, model.to_owned()));
        }
        let mut drawn = Vec::new();
        let mut each = |text: &str| {
            let record = self.record("line", json!({"message": key, "voice": voice, "text": text}));
            say(&Self::said(
                "line",
                &record,
                json!({"message": key, "voice": voice, "text": text}),
            ));
            if let Some(open) = self.open.borrow_mut().as_mut() {
                if !open.text.is_empty() {
                    open.text.push('\n');
                }
                open.text.push_str(text);
                self.live(open, None);
            }
            drawn.push(text.to_owned());
        };
        let mut lines = Lines::default();
        let asked = line
            .ask(message, &mut |text| lines.feed(text, &mut each))
            .await;
        if asked.is_ok() {
            // A whole reply's last line is complete; a failed one drops its open line.
            lines.end(&mut each);
        }
        let open = self.open.take();
        if let Some(open) = &open {
            let stop = match asked {
                Ok(_) => LiveStop::Stop,
                Err(_) => LiveStop::Error,
            };
            self.live(open, Some(stop));
        }
        if let Some(usage) = asked? {
            let tokens = json!({
                "voice": voice,
                "model": model,
                "in": usage.input,
                "cached": usage.cached,
                "out": usage.output,
            });
            let record = self.record("tokens", tokens.clone());
            say(&Self::said("tokens", &record, tokens));
        }
        Ok(drawn)
    }

    fn live(&self, open: &Open, end: Option<LiveStop>) {
        if let Some(door) = &self.door {
            door.live(&open.key, &open.text, end);
        }
    }

    /// The turn's end: its record and line, the door's settled state and its settle; the
    /// process's exit code.
    fn end(&mut self, state: TurnState, failure: Option<Failure>) -> u8 {
        let failure = failure.map(|failure| {
            json!({"kind": failure.kind, "line": failure.line, "service": failure.service})
        });
        let mut fields = json!({"state": state});
        if let Some(failure) = &failure {
            fields["failure"] = failure.clone();
        }
        let record = self.record("turn_ended", fields.clone());
        say(&Self::said("ended", &record, fields));
        if let Some(door) = self.door.take() {
            let error = failure.as_ref().map(|failure| {
                failure["line"]
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(SETTLED_ERROR)
                    .collect()
            });
            door.state(LiveState {
                idle: Some(true),
                settled: vec![LiveSettled {
                    began_at: self.began_at,
                    at: transcript::now(),
                    says: Vec::new(),
                    failed: state == TurnState::Failed,
                    error,
                }],
                ..LiveState::default()
            });
            door.settle(state);
        }
        u8::from(state == TurnState::Failed)
    }
}

/// The door's state while a model call of `model` runs.
fn running(began_at: u64, model: String) -> LiveState {
    LiveState {
        turn: Some(LiveTurn {
            began_at,
            says: Vec::new(),
        }),
        model: Some(model),
        ..LiveState::default()
    }
}

/// A drawer's text as it streams, cut into its lines: each that ends goes to `each`, but a
/// blank one and a `#` comment.
#[derive(Default)]
struct Lines {
    open: String,
}

impl Lines {
    fn feed(&mut self, text: &str, each: &mut impl FnMut(&str)) {
        self.open.push_str(text);
        while let Some(at) = self.open.find('\n') {
            let line: String = self.open.drain(..=at).collect();
            kept(&line, each);
        }
    }

    fn end(&mut self, each: &mut impl FnMut(&str)) {
        kept(&std::mem::take(&mut self.open), each);
    }
}

fn kept(line: &str, each: &mut impl FnMut(&str)) {
    let line = line.trim_end();
    if !line.trim_start().is_empty() && !line.trim_start().starts_with('#') {
        each(line);
    }
}

/// The person's words as one typed input line.
fn typed(words: &str) -> String {
    format!(
        "type \"{}\"",
        words
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}

/// The sketcher's message: a composed one as it is; bare words as one final typed
/// utterance on an empty canvas.
fn sketch_message(message: &str) -> String {
    match message.contains(OUTLINE_HEAD) {
        true => message.to_owned(),
        false => format!(
            "{OUTLINE_HEAD}(empty)\n\nNew words:\n{}",
            typed(message.trim())
        ),
    }
}

/// The plotter's message: the same context, the sketch's lines, and the words.
fn refine_message(message: &str, sketch: &[String]) -> String {
    let sketch = match sketch.is_empty() {
        true => "(none)".to_owned(),
        false => sketch.join("\n"),
    };
    let part = format!("Sketch to refine:\n{sketch}");
    match message.contains(OUTLINE_HEAD) {
        true => format!("{}\n\n{part}", message.trim_end()),
        false => format!(
            "{OUTLINE_HEAD}(empty)\n\n{part}\n\nWords (the person stopped):\n{}",
            typed(message.trim())
        ),
    }
}
