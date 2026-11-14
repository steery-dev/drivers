//! Drive: Steery starts Aider's turns as `aider --message-file /dev/stdin`, the words on
//! standard input, through this driver's own binary in its turn mode ([`crate::turn`]).
//! A continue passes `--restore-chat-history`, which loads the whole history file
//! (`utils.py`, `split_chat_history_markdown`), so the file is the conversation.
//!
//! Nobody answers Aider's questions in a turn, so the plan answers them yes
//! (`--yes-always`), and turns off what a yes would let reach past the repository: shell
//! commands the model suggests (`--no-suggest-shell-commands`) and the pages of URLs in the
//! words (`--no-detect-urls`). Two of Aider's questions open the person's browser, so the
//! plan avoids both: a model's warnings ask to open its documentation
//! (`--no-show-model-warnings`), and a run with no model and no key opens OpenRouter's
//! sign-in and waits 5 minutes for it (`onboarding.py`), so the plan always names a model.
//!
//! The other methods read the history files: where to look for who holds a conversation,
//! how its last turn ended, the receipt of words, and the command that opens it in a
//! terminal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use steery_driver_common::program::{ask, helper};
use steery_driver_common::receipt::{DELIVERED, marked};
use steery_sdk::wire::harness::{
    Act, Event, Execution, Held, HeldBy, Holder, HolderStop, Launch, Look, Looked, Marked, Model,
    Models, Receipt, ReceiptFind, ReceiptMark, Sample, Sampled, Start, Stdin, Stop, Stopped,
    Stream, Terminal, TerminalCommand, TurnLine, TurnRead, TurnState, Unlisted,
};
use steery_sdk::{Error, refusal};

use crate::history;
use crate::home;
use crate::store::{Store, unreadable};
use crate::turn;

/// The reasoning efforts Aider passes to a model (`--reasoning-effort`).
const THINKING: [&str; 3] = ["low", "medium", "high"];

/// What every turn passes Aider: no questions, no screen, no network but the model's.
const QUIET: [&str; 9] = [
    "--yes-always",
    "--no-pretty",
    "--no-fancy-input",
    "--no-check-update",
    "--no-show-release-notes",
    "--no-analytics",
    "--no-show-model-warnings",
    "--no-suggest-shell-commands",
    "--no-detect-urls",
];

/// `models`: the models Ollama has here, as Aider names them (`ollama_chat/<name>`). Aider
/// runs every model litellm knows, with the key of its provider in its environment or a
/// `.env`, so any other model is unlisted.
pub async fn models(home: &Path) -> Result<Models, Error> {
    if home::binary(home).is_none() {
        return Err(Error::refused(
            refusal::NO_BINARY,
            "aider is not installed here",
        ));
    }
    let mut models = Vec::new();
    if let Some(ollama) = home::ollama() {
        let mut command = helper(&ollama);
        command.arg("list");
        if let Ok(answer) = ask(command).await
            && answer.code == Some(0)
        {
            models = String::from_utf8_lossy(&answer.stdout)
                .lines()
                .skip(1)
                .filter_map(|line| line.split_whitespace().next())
                .map(|name| format!("ollama_chat/{name}"))
                .map(|model| Model {
                    provider: history::provider(&model).map(str::to_owned),
                    label: model.split_once('/').map(|(_, name)| name.to_owned()),
                    model,
                    thinking: Vec::new(),
                    cost: Some(0.0),
                })
                .collect();
        }
    }
    Ok(Models {
        models,
        unlisted: Some(Unlisted {
            thinking: THINKING.iter().map(|&level| level.to_owned()).collect(),
        }),
        watch: vec![home::manifests(home)],
        signed_in: None,
    })
}

/// `start`: the plan of this driver's turn mode, which runs Aider in the history file's
/// folder. A new conversation takes Aider's own file in the repository of `cwd` where there
/// is none yet, else a file of its own beside it.
pub fn start(home: &Path, store: &Store, start: Start) -> Result<Launch, Error> {
    let binary = home::binary(home)
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "aider is not installed here"))?;
    if !start.cwd.is_absolute() {
        return Err(Error::invalid("the folder is not an absolute path"));
    }
    if start.worktree {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Steery does not start Aider in a worktree of its own",
        ));
    }
    let mut run: Vec<String> = QUIET.iter().map(|&flag| flag.to_owned()).collect();
    let (file, last_model) = match start.act {
        Act::New => {
            if !start.cwd.is_dir() {
                return Err(Error::refused(
                    refusal::NO_PLACE,
                    format!("{} is not a folder here", start.cwd.display()),
                ));
            }
            let root = home::root(&start.cwd);
            let mut file = root.join(home::NAME);
            if file.exists() {
                file = root.join(format!(".aider.chat.history.{}.md", now()));
            }
            (file, None)
        }
        Act::Continue => {
            let id = start
                .external_id
                .as_deref()
                .ok_or_else(|| Error::invalid("a continue must name its conversation"))?;
            let file = store.path(home, id)?;
            let place = file.parent().unwrap_or(&file).to_owned();
            if !start.cwd.is_dir() {
                return Err(Error::refused(
                    refusal::NO_PLACE,
                    format!(
                        "{} is gone, and Aider continues the conversation there",
                        start.cwd.display()
                    ),
                ));
            }
            if place != start.cwd {
                return Err(Error::refused(
                    refusal::NO_PLACE,
                    format!(
                        "Aider keeps conversation {id} in {}, and continues it only there",
                        place.display()
                    ),
                ));
            }
            let (summary, _) = store.summary(&file).map_err(|e| unreadable(&file, &e))?;
            run.push("--restore-chat-history".into());
            (file, summary.model)
        }
        Act::Fork | Act::Hand => {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Steery starts only new and continued Aider conversations",
            ));
        }
    };
    let model = start.model.or(last_model).ok_or_else(|| {
        Error::refused(
            refusal::MODEL_MISSING,
            "Aider needs a model picked: without one it opens OpenRouter's sign-in in a browser",
        )
    })?;
    run.extend(["--model".into(), model]);
    if let Some(thinking) = start.thinking {
        run.extend(["--reasoning-effort".into(), thinking]);
    }
    run.extend([
        "--chat-history-file".into(),
        text_of(file.clone())?,
        "--message-file".into(),
        "/dev/stdin".into(),
    ]);
    let me = std::env::current_exe().map_err(|e| {
        Error::refused(
            refusal::START_FAILED,
            format!("cannot find this driver: {e}"),
        )
    })?;
    let mut argv = vec![text_of(me)?, turn::MODE.into(), text_of(binary)?];
    argv.extend(run);
    store.planned(&file);
    Ok(Launch {
        argv,
        env: BTreeMap::new(),
        cwd: file.parent().unwrap_or(&start.cwd).to_owned(),
        stdin: Stdin::Words,
        inbox: false,
    })
}

fn text_of(path: PathBuf) -> Result<String, Error> {
    path.into_os_string()
        .into_string()
        .map_err(|path| Error::invalid(format!("{} is not text", path.display())))
}

/// `turn/read`: one line of the turn mode ([`turn::Line`]). It names the conversation at
/// once, and says how the turn ended once Aider exited; a refusal comes on standard error.
pub fn read(read: &TurnRead) -> TurnLine {
    let mut line = TurnLine::default();
    let Ok(said) = serde_json::from_str::<turn::Line>(&read.line) else {
        return line;
    };
    line.named = said.named;
    line.ended = said.ended;
    line.refused = said.refused.filter(|_| read.stream == Stream::Stderr);
    line
}

/// The refusal that one of Aider's failure lines means.
pub fn refusal_of(line: &str) -> Option<&'static str> {
    let lower = line.to_lowercase();
    (lower.contains("model") && lower.contains("not found")
        || lower.contains("notfounderror")
        || lower.contains("llm provider not provided"))
    .then_some(refusal::MODEL_MISSING)
}

/// `held`: Aider keeps no mark of the run that has a conversation open, and holds its file
/// open only while it appends. A run names the file on its command line only where it was
/// given `--chat-history-file`, so any Aider can hold the conversation. A conversation that
/// is not here leaves nowhere to look.
pub fn held(home: &Path, store: &Store, held: Held) -> Result<HeldBy, Error> {
    let look = match store.path(home, &held.external_id) {
        Ok(path) => Look {
            transcripts: vec![path],
            strays: true,
            beyond: Vec::new(),
        },
        Err(_) => Look {
            transcripts: Vec::new(),
            strays: false,
            beyond: Vec::new(),
        },
    };
    Ok(HeldBy::Look(Looked { look }))
}

/// `idle`: how the conversation's last turn ended, read as if nothing wrote the file, so
/// true only where nothing holds it. With no file to read, the driver cannot say.
pub fn idle(home: &Path, store: &Store, held: Held) -> Result<Sampled, Error> {
    let Ok(path) = store.path(home, &held.external_id) else {
        return Ok(Sampled { sample: None });
    };
    let (Ok(read), Ok((summary, _))) =
        (history::read(&path, None, None, true), store.summary(&path))
    else {
        return Ok(Sampled { sample: None });
    };
    Ok(Sampled {
        sample: last(&read.events).map(|(started_at, ended_at, state, error)| Sample {
            state: match state {
                TurnState::Finished => Execution::Finished,
                TurnState::Failed => Execution::Failed,
                TurnState::Interrupted => Execution::Interrupted,
            },
            observed_at: now(),
            started_at,
            ended_at: Some(ended_at),
            model: summary.model,
            thinking: summary.thinking,
            error,
            vacant: false,
            if_unheld: true,
        }),
    })
}

/// The last turn's start, end, state and error.
pub fn last(events: &[Event]) -> Option<(Option<u64>, u64, TurnState, Option<String>)> {
    let mut started_at = None;
    let mut ended = None;
    for event in events {
        match event {
            Event::TurnBegan { at, .. } => {
                started_at = Some(*at);
                ended = None;
            }
            Event::TurnEnded {
                at, state, failure, ..
            } => ended = Some((*at, *state, failure.as_ref().map(|f| f.line.clone()))),
            _ => {}
        }
    }
    let (at, state, error) = ended?;
    Some((started_at, at, state, error))
}

/// `stop`: Aider ends a turn only on Ctrl-C in its own process, which the runtime signals
/// for a turn it started (the turn mode passes it on). A person's Aider is theirs.
pub fn stop(stop: &Stop) -> Result<Stopped, Error> {
    if stop.say.is_some() {
        return Err(Error::refused(
            refusal::OTHER_TURN,
            "Aider's driver cannot tell which words began the running turn",
        ));
    }
    Ok(Stopped {
        stopped: false,
        proof: Some("Aider has no way to end a turn from outside its process".into()),
    })
}

/// `receipt/mark`: the file's length, after every record it holds now.
pub fn mark(home: &Path, store: &Store, mark: ReceiptMark) -> Result<Marked, Error> {
    let path = store.path(home, &mark.external_id)?;
    let length = history::length(&path).map_err(|e| unreadable(&path, &e))?;
    Ok(Marked {
        mark: length.to_string(),
    })
}

/// `receipt/find`: whether the person's words after the mark carry the delivery marker.
pub fn find(home: &Path, store: &Store, find: ReceiptFind) -> Result<Receipt, Error> {
    let after: u64 = find
        .mark
        .parse()
        .map_err(|_| Error::invalid(format!("mark {} is not a mark of Aider's", find.mark)))?;
    let path = store.path(home, &find.external_id)?;
    let said = history::said(&path, after).map_err(|e| unreadable(&path, &e))?;
    let marker = format!("{DELIVERED}{}", find.delivery);
    let found = said.iter().filter(|words| marked(words, &marker)).count();
    if found > 1 {
        return Err(Error::refused(
            refusal::REFUSED,
            "two inputs carry the delivery marker",
        ));
    }
    Ok(Receipt {
        found: found == 1,
        owner: None,
    })
}

/// `terminal`: Aider in the conversation's folder, its history loaded, on its last model.
pub fn terminal(home: &Path, store: &Store, terminal: Terminal) -> Result<TerminalCommand, Error> {
    let path = store.path(home, &terminal.external_id)?;
    let place = path.parent().unwrap_or(&path);
    let (summary, _) = store.summary(&path).map_err(|e| unreadable(&path, &e))?;
    let mut command = format!(
        "cd {} && aider --restore-chat-history --chat-history-file {}",
        quoted(&place.to_string_lossy()),
        quoted(&path.to_string_lossy())
    );
    if let Some(model) = &summary.model {
        command.push_str(&format!(" --model {}", quoted(model)));
    }
    let line = match &terminal.holder {
        Some(Holder::Person {
            stop: HolderStop::Process { pid, .. },
            ..
        }) => Some(format!(
            "Aider has this conversation open already (pid {pid}). Two Aiders append to the \
             same file and each loads only what it read at its start."
        )),
        Some(Holder::Person {
            stop: HolderStop::Beyond(how),
            ..
        }) => Some(how.clone()),
        _ => None,
    };
    Ok(TerminalCommand {
        command,
        app: None,
        line,
    })
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Now, in milliseconds since 1970.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
