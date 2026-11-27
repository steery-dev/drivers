//! Drive: Steery starts the canvas's turns as `canvas turn [--session <id>] --cwd <cwd>`, the
//! words on standard input, through this driver's own binary in its turn mode
//! ([`crate::turn`]), which passes the canvas's JSON lines on. `turn/read` reads them.
//!
//! A turn holds its session: `<id>.pid` beside the transcript holds the turn's pid while it
//! runs, and is removed at its end. That file is the canvas's own registry, so `held`, `idle`
//! and `stop` read it, and the canvas ends a turn on SIGINT.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use steery_driver_common::kernel;
use steery_sdk::wire::harness::{
    Act, Ended, Event, Execution, Held, HeldBy, Holder, HolderStop, Launch, Look, Looked, Sample,
    Sampled, Start, Stdin, Stop, Stopped, Stream, Terminal, TerminalCommand, TurnLine, TurnRead,
    TurnState,
};
use steery_sdk::{Error, refusal};

use crate::sessions::unreadable;
use crate::{home, transcript, turn};

/// How long `stop` waits for the turn to end on SIGINT, inside its 2.8 s.
const STOP_WITHIN: Duration = Duration::from_millis(2500);
/// The canvas's own command that runs one turn.
const TURN: &str = "turn";

/// `start`: the plan of this driver's turn mode, which runs `canvas turn` in `cwd`. The
/// canvas picks each model by its role and takes no model or thinking level.
pub fn start(root: &Path, start: Start) -> Result<Launch, Error> {
    let binary = home::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "canvas is not installed here"))?;
    if !start.cwd.is_absolute() {
        return Err(Error::invalid("the folder is not an absolute path"));
    }
    if start.worktree {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Steery does not start the canvas in a worktree of its own",
        ));
    }
    if let Some(picked) = start.model.as_deref().or(start.thinking.as_deref()) {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("`canvas turn` picks its own models by role and takes no {picked}"),
        ));
    }
    if !start.cwd.is_dir() {
        return Err(Error::refused(
            refusal::NO_PLACE,
            format!("{} is not a folder here", start.cwd.display()),
        ));
    }
    let mut run = vec![TURN.to_owned()];
    match start.act {
        Act::New => {}
        Act::Continue => {
            let id = start
                .external_id
                .ok_or_else(|| Error::invalid("a continue must name its session"))?;
            home::transcript(root, &id)?;
            run.extend(["--session".into(), id]);
        }
        Act::Fork | Act::Hand => {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Steery starts only new and continued canvas sessions",
            ));
        }
    }
    run.extend(["--cwd".into(), text_of(start.cwd.clone())?]);
    let me = std::env::current_exe().map_err(|e| {
        Error::refused(
            refusal::START_FAILED,
            format!("cannot find this driver: {e}"),
        )
    })?;
    let mut argv = vec![text_of(me)?, turn::MODE.into(), text_of(binary)?];
    argv.extend(run);
    Ok(Launch {
        argv,
        env: BTreeMap::new(),
        cwd: start.cwd,
        stdin: Stdin::Words,
        inbox: false,
    })
}

fn text_of(path: PathBuf) -> Result<String, Error> {
    path.into_os_string()
        .into_string()
        .map_err(|path| Error::invalid(format!("{} is not text", path.display())))
}

/// One JSON line of `canvas turn`: the session it names, a finished stroke line, a model
/// call's count, or how the turn ended. Each `line` and `tokens` names its transcript
/// record, so its event is the one `sessions/read` gives under the same id.
#[derive(Debug, Default, Deserialize)]
pub struct Said {
    #[serde(default)]
    pub named: Option<String>,
    #[serde(default)]
    pub line: Option<Stroke>,
    #[serde(default)]
    pub tokens: Option<Spent>,
    #[serde(default)]
    pub ended: Option<Ending>,
}

#[derive(Debug, Deserialize)]
pub struct Stroke {
    pub id: String,
    pub message: String,
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct Spent {
    pub id: String,
    pub model: String,
    #[serde(rename = "in")]
    pub input: u64,
    pub cached: u64,
    pub out: u64,
}

#[derive(Debug, Deserialize)]
pub struct Ending {
    pub state: TurnState,
    #[serde(default)]
    pub failure: Option<transcript::Fault>,
}

impl Ending {
    pub fn ended(self) -> Ended {
        Ended {
            state: self.state,
            error: self.failure.map(|failure| failure.line),
        }
    }
}

/// `turn/read`: one line of the turn mode, which are the canvas's own. Its lines carry no
/// time, so their events are stamped as they are read.
pub fn read(read: &TurnRead) -> TurnLine {
    let said = match read.stream {
        Stream::Stdout => serde_json::from_str::<Said>(&read.line).unwrap_or_default(),
        Stream::Stderr => Said::default(),
    };
    let mut events = Vec::new();
    if let Some(stroke) = said.line {
        events.push(transcript::line(transcript::now(), stroke.id, stroke.message, stroke.text));
    }
    if let Some(spent) = said.tokens {
        events.push(transcript::tokens(
            transcript::now(),
            spent.id,
            spent.model,
            spent.input,
            spent.cached,
            spent.out,
        ));
    }
    TurnLine {
        named: said.named,
        ended: said.ended.map(Ending::ended),
        events,
        ..TurnLine::default()
    }
}

/// Who holds a session, by its `.pid` file.
enum Hold {
    /// No turn: the proof.
    Free(String),
    /// The turn's process, and whether it started before the file was written.
    Turn { pid: u32, start: u64, certain: bool },
}

fn hold(root: &Path, id: &str) -> std::io::Result<Hold> {
    let file = home::holder(root, id);
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Hold::Free(format!(
                "no canvas turn holds it: {} is absent",
                file.display()
            )));
        }
        Err(e) => return Err(e),
    };
    let pid: u32 = text.trim().parse().map_err(|_| {
        std::io::Error::other(format!("{} holds no pid: {text:?}", file.display()))
    })?;
    let Some(start) = kernel::started(pid) else {
        return Ok(Hold::Free(format!(
            "pid {pid} of {} is gone",
            file.display()
        )));
    };
    // The turn writes its pid once it runs: a process started after the file is another.
    let written = transcript::modified(&file)?;
    Ok(Hold::Turn {
        pid,
        start,
        certain: start <= written + 1000,
    })
}

/// `held`: the turn whose pid the session's `.pid` file holds, else nobody. A session that
/// is not here, or a `.pid` file that does not read, leaves only its transcript to look at.
pub fn held(root: &Path, held: Held) -> Result<HeldBy, Error> {
    let Ok(path) = home::transcript(root, &held.external_id) else {
        return Ok(HeldBy::Look(Looked {
            look: Look {
                transcripts: Vec::new(),
                strays: false,
                beyond: Vec::new(),
            },
        }));
    };
    Ok(match hold(root, &held.external_id) {
        Ok(Hold::Free(proof)) => HeldBy::Holder(Holder::Nobody { proof }),
        Ok(Hold::Turn {
            pid,
            start,
            certain,
        }) => HeldBy::Holder(Holder::Person {
            door: None,
            stop: HolderStop::Process {
                pid,
                process_start: start,
                certain,
                how: "in a canvas turn".into(),
            },
        }),
        Err(_) => HeldBy::Look(Looked {
            look: Look {
                transcripts: vec![path],
                strays: false,
                beyond: Vec::new(),
            },
        }),
    })
}

/// `idle`: a turn that holds the session runs; else its last turn ended as the transcript
/// says, or `interrupted` where it wrote no end. The `.pid` file proves both, so the sample
/// stands whatever process the runtime finds.
pub fn idle(root: &Path, held: Held) -> Result<Sampled, Error> {
    let Ok(path) = home::transcript(root, &held.external_id) else {
        return Ok(Sampled { sample: None });
    };
    let (Ok(hold), Ok(events)) = (hold(root, &held.external_id), transcript::events(&path)) else {
        return Ok(Sampled { sample: None });
    };
    let last = last(&events);
    let Some(state) = (match (&hold, &last.ended) {
        (Hold::Turn { .. }, _) => Some(Execution::Running),
        (Hold::Free(_), _) if last.open => Some(Execution::Interrupted),
        (Hold::Free(_), Some((_, state, _))) => Some(match state {
            TurnState::Finished => Execution::Finished,
            TurnState::Failed => Execution::Failed,
            TurnState::Interrupted => Execution::Interrupted,
        }),
        (Hold::Free(_), None) => None,
    }) else {
        return Ok(Sampled { sample: None });
    };
    let closed = state != Execution::Running && !last.open;
    Ok(Sampled {
        sample: Some(Sample {
            state,
            observed_at: transcript::now(),
            started_at: last.started_at,
            ended_at: last.ended.as_ref().filter(|_| closed).map(|(at, ..)| *at),
            model: last.model,
            thinking: None,
            error: last
                .ended
                .filter(|_| closed)
                .and_then(|(_, _, error)| error),
            vacant: false,
            if_unheld: false,
        }),
    })
}

/// What the events say of the last turn.
#[derive(Default)]
struct Last {
    started_at: Option<u64>,
    /// The last end: when, how, and the failure's line.
    ended: Option<(u64, TurnState, Option<String>)>,
    /// A turn began after the last end.
    open: bool,
    model: Option<String>,
}

fn last(events: &[Event]) -> Last {
    let mut last = Last::default();
    for event in events {
        match event {
            Event::TurnBegan { at, .. } => {
                last.started_at = Some(*at);
                last.open = true;
            }
            Event::TurnEnded {
                at, state, failure, ..
            } => {
                last.ended = Some((*at, *state, failure.as_ref().map(|f| f.line.clone())));
                last.open = false;
            }
            Event::Model { model, .. } => last.model = Some(model.clone()),
            _ => {}
        }
    }
    last
}

/// `stop`: SIGINT to the turn's process, the canvas's own way to end a turn as
/// `interrupted`, and a wait for it to end.
pub async fn stop(stop: &Stop) -> Result<Stopped, Error> {
    if stop.say.is_some() {
        return Err(Error::refused(
            refusal::OTHER_TURN,
            "the canvas's driver cannot tell which words began the running turn",
        ));
    }
    let Holder::Person {
        stop: HolderStop::Process {
            pid, process_start, ..
        },
        ..
    } = &stop.holder
    else {
        return Ok(Stopped {
            stopped: false,
            proof: Some("no canvas turn's process holds this session".into()),
        });
    };
    let (pid, start) = (*pid, *process_start);
    if kernel::signal(&[(pid, start)], libc::SIGINT) == 0 {
        return Ok(Stopped {
            stopped: false,
            proof: Some(format!("pid {pid} had ended already")),
        });
    }
    let began = tokio::time::Instant::now();
    while began.elapsed() < STOP_WITHIN {
        if kernel::started(pid) != Some(start) {
            return Ok(Stopped {
                stopped: true,
                proof: Some(format!("the canvas turn of pid {pid} ended on SIGINT")),
            });
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(Stopped {
        stopped: false,
        proof: Some(format!(
            "pid {pid} still ran {} ms after SIGINT",
            STOP_WITHIN.as_millis()
        )),
    })
}

/// `terminal`: one more turn on the session, its words typed at the terminal. The canvas
/// has no window of its own that opens a session.
pub fn terminal(root: &Path, terminal: Terminal) -> Result<TerminalCommand, Error> {
    let path = home::transcript(root, &terminal.external_id)?;
    let summary = transcript::summary(&path).map_err(|e| unreadable(&path, &e))?;
    let cwd = summary.cwd.unwrap_or(terminal.cwd);
    let cwd = quoted(&cwd.to_string_lossy());
    let command = format!(
        "cd {cwd} && canvas turn --session {} --cwd {cwd}",
        quoted(&terminal.external_id)
    );
    let mut line = "Type your words, then Ctrl-D: the canvas runs one turn on them.".to_owned();
    if let Some(Holder::Person {
        stop: HolderStop::Process { pid, .. },
        ..
    }) = &terminal.holder
    {
        line.push_str(&format!(" A canvas turn (pid {pid}) holds this session now."));
    }
    Ok(TerminalCommand {
        command,
        app: None,
        line: Some(line),
    })
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}
