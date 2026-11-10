//! What drive's methods read alike across the family: a turn's launch plan, its output's
//! lines, the receipt of words, a stop that cannot reach outside the turn, and the terminal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use steery_driver_common::receipt::{DELIVERED, marked};
use steery_sdk::wire::harness::{
    Act, Ended, Launch, Model, Receipt, Start, Stdin, Stop, Stopped, Stream, TerminalCommand,
    TurnLine, TurnRead, TurnState,
};
use steery_sdk::{Error, refusal};

use super::Asked;

/// The updates of a session that only a prompt makes: the harness answering words.
const ANSWERING: [&str; 5] = [
    "agent_message_chunk",
    "agent_thought_chunk",
    "tool_call",
    "tool_call_update",
    "plan",
];

/// A model and thinking level that the harness's last list has; an empty list checks
/// nothing.
pub fn offered(
    label: &str,
    listed: &[Model],
    model: Option<&str>,
    thinking: Option<&str>,
) -> Result<(), Error> {
    let (Some(model), false) = (model, listed.is_empty()) else {
        return Ok(());
    };
    let Some(entry) = listed.iter().find(|entry| entry.model == model) else {
        return Err(Error::refused(
            refusal::MODEL_MISSING,
            format!("{label} lists no model {model}"),
        ));
    };
    match thinking {
        Some(level) if !entry.thinking.iter().any(|known| known == level) => Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("{label}'s {model} has no thinking level {level}"),
        )),
        _ => Ok(()),
    }
}

/// Steery's hook command line, as setup wrote it in `file`.
pub fn hook(file: &Path) -> Option<String> {
    std::fs::read_to_string(file)
        .ok()
        .map(|hook| hook.trim().to_owned())
        .filter(|hook| !hook.is_empty())
}

/// `start`: the launch plan of a turn, which runs this driver's binary in its turn mode
/// speaking to `<binary> acp`. The harness resumes a conversation in the folder it ran in
/// (`directory`), so a continue whose folder is gone is refused. Without a `hook`, a
/// permission the harness asks for is rejected.
pub fn launch(
    label: &str,
    binary: Option<&Path>,
    start: Start,
    directory: impl FnOnce(&str) -> Result<Option<PathBuf>, Error>,
    hook: Option<String>,
    env: BTreeMap<String, String>,
) -> Result<Launch, Error> {
    let binary = binary
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, format!("{label} is not installed")))?;
    let act = match start.act {
        Act::Continue => "continue",
        Act::Fork => "fork",
        Act::New => "new",
        Act::Hand => {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                format!("{label} has no hand-over"),
            ));
        }
    };
    if !start.cwd.is_absolute() || !start.folder.is_absolute() {
        return Err(Error::invalid("the folders must be absolute paths"));
    }
    let id = match start.act {
        Act::New => None,
        _ => {
            let id = start.external_id.clone().ok_or_else(|| {
                Error::invalid(format!("a {act} must name the conversation it acts on"))
            })?;
            let directory = directory(&id)?;
            if start.act == Act::Continue
                && let Some(directory) = directory
                && !directory.is_dir()
            {
                return Err(Error::refused(
                    refusal::NO_PLACE,
                    format!(
                        "{} is gone, and {label} resumes the conversation only there",
                        directory.display()
                    ),
                ));
            }
            Some(id)
        }
    };
    let program = std::env::current_exe().map_err(|e| {
        Error::refused(
            refusal::START_FAILED,
            format!("cannot find this driver's own path: {e}"),
        )
    })?;
    let asked = Asked {
        binary: binary.to_path_buf(),
        act: act.to_owned(),
        id,
        model: start.model,
        thinking: start.thinking,
        inbox: Some(start.folder.join("inbox")),
        hook,
    };
    let mut argv = vec![program.to_string_lossy().into_owned(), "turn".to_owned()];
    argv.extend(asked.args());
    Ok(Launch {
        argv,
        env,
        cwd: start.cwd,
        stdin: Stdin::Words,
        inbox: true,
    })
}

/// `stop`: the harness has no way to end a turn from outside the process that runs it. A
/// turn of this driver's ends on the runtime's signal, which it passes on as a cancel.
pub fn stop(label: &str, stop: &Stop) -> Result<Stopped, Error> {
    if stop.say.is_some() {
        return Err(Error::refused(
            refusal::OTHER_TURN,
            format!("{label}'s driver cannot tell which words began the running turn"),
        ));
    }
    Ok(Stopped {
        stopped: false,
        proof: Some(format!(
            "{label} has no way to end a turn from outside its process"
        )),
    })
}

/// `receipt/find`: whether exactly one of the person's words after the mark (`said`)
/// carries the delivery's marker.
pub fn find(said: &[String], delivery: &str) -> Result<Receipt, Error> {
    let marker = format!("{DELIVERED}{delivery}");
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

/// `terminal`: the harness's own window (`command`) on the conversation, in its folder.
pub fn terminal(command: &str, cwd: &Path, external_id: &str) -> TerminalCommand {
    TerminalCommand {
        command: format!(
            "cd {} && {command} --session {}",
            quoted(&cwd.to_string_lossy()),
            quoted(external_id)
        ),
        app: None,
        line: None,
    }
}

/// `turn/read`: a line of a turn's output. Standard output is ACP: the session is named
/// by the harness's answer that opens it or by the prompt sent into it, and taken once the
/// harness answers the prompt; its answer to a prompt ends the turn. Standard error is the
/// turn mode's one line, `<refusal word>: <why>`.
pub fn turn_line(label: &str, read: TurnRead) -> TurnLine {
    match read.stream {
        Stream::Stderr => TurnLine {
            refused: read
                .line
                .split_once(':')
                .map(|(word, _)| word.trim())
                .filter(|word| refusal::ALL.contains(word))
                .map(str::to_owned)
                .or_else(|| (!read.line.trim().is_empty()).then(|| refusal::REFUSED.to_owned())),
            ..TurnLine::default()
        },
        Stream::Stdout => {
            let Ok(line) = serde_json::from_str::<Value>(&read.line) else {
                return TurnLine::default();
            };
            let method = line.get("method").and_then(Value::as_str);
            let answer = method.is_none() && line.get("id").is_some();
            let named = if answer {
                line.pointer("/result/sessionId")
            } else if method == Some("session/prompt") {
                line.pointer("/params/sessionId")
            } else {
                None
            }
            .and_then(Value::as_str)
            .map(str::to_owned);
            let update = (method == Some("session/update"))
                .then(|| line.pointer("/params/update"))
                .flatten();
            let kind = update
                .and_then(|update| update.get("sessionUpdate"))
                .and_then(Value::as_str);
            let ended = answer.then(|| ended(label, &line)).flatten();
            let took = read.delivery.as_ref().and_then(|_| {
                if kind.is_some_and(|kind| ANSWERING.contains(&kind))
                    || ended
                        .as_ref()
                        .is_some_and(|ended| ended.state != TurnState::Failed)
                {
                    Some(true)
                } else {
                    named.is_some().then_some(false)
                }
            });
            TurnLine {
                named,
                took,
                ended,
                refused: answer
                    .then(|| line.get("error"))
                    .flatten()
                    .map(|error| super::word(error).to_owned()),
                events: Vec::new(),
                title: (kind == Some("session_info_update"))
                    .then(|| update?.get("title")?.as_str().map(str::to_owned))
                    .flatten(),
            }
        }
    }
}

/// How the harness's answer ends the turn: a prompt's `stopReason`, or any refusal.
fn ended(label: &str, answer: &Value) -> Option<Ended> {
    if let Some(error) = answer.get("error") {
        return Some(Ended {
            state: TurnState::Failed,
            error: error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    let reason = answer.pointer("/result/stopReason")?.as_str()?;
    Some(match reason {
        "end_turn" => Ended {
            state: TurnState::Finished,
            error: None,
        },
        "cancelled" => Ended {
            state: TurnState::Interrupted,
            error: None,
        },
        other => Ended {
            state: TurnState::Failed,
            error: Some(format!("{label} stopped the turn: {other}")),
        },
    })
}

/// Now, in milliseconds since 1970.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}
