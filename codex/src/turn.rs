//! A Codex turn, run by this driver's own binary in its turn mode: the words on standard
//! input, an app server of Codex's own (`codex app-server`) spoken to — `thread/start`,
//! `thread/resume` or `thread/fork`, then `turn/start` with the words — every message it
//! sends written verbatim to standard output, and out once `turn/completed` says the
//! turn's last turn ended. What runs is the conversation the Codex app would have run: the
//! person's configuration, MCP servers, hooks and skills; it is in the app's sidebar.
//!
//! While the turn runs, more words told on its inbox (a unix socket in the turn's folder)
//! go in the way the Codex app puts them: `turn/steer` into the turn running, or
//! `turn/start` for the next once it has ended; the caller reads back `heard`, or Codex's
//! words. Where the app server refuses, its line goes to standard error and the turn
//! fails. Codex makes a worktree in its app, not its app server, so one asked for is made
//! where Codex makes its own: `<CODEX_HOME>/worktrees/<4 hex>/<repository>`, detached at
//! `HEAD`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::wire::harness::HEARD;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::appserver::{self, AppServer};

/// How long a caller at the inbox has to finish writing its words.
const WORDS_WITHIN: Duration = Duration::from_secs(10);

/// What a turn is asked to do, as its command line says it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Asked {
    pub codex: PathBuf,
    pub act: String,
    pub id: Option<String>,
    pub worktree: bool,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub inbox: Option<PathBuf>,
}

impl Asked {
    /// The arguments after `turn`.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--codex".to_owned(),
            self.codex.to_string_lossy().into_owned(),
            self.act.clone(),
        ];
        args.extend(self.id.clone());
        if self.worktree {
            args.push("--worktree".into());
        }
        for (flag, value) in [("--model", &self.model), ("--thinking", &self.thinking)] {
            if let Some(value) = value {
                args.push(flag.into());
                args.push(value.clone());
            }
        }
        if let Some(inbox) = &self.inbox {
            args.push("--inbox".into());
            args.push(inbox.to_string_lossy().into_owned());
        }
        args
    }

    pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut asked = Self::default();
        while let Some(arg) = args.next() {
            let mut value = |flag: &str| args.next().ok_or(format!("{flag} takes a value"));
            match arg.as_str() {
                "--codex" => asked.codex = PathBuf::from(value("--codex")?),
                "--worktree" => asked.worktree = true,
                "--model" => asked.model = Some(value("--model")?),
                "--thinking" => asked.thinking = Some(value("--thinking")?),
                "--inbox" => asked.inbox = Some(PathBuf::from(value("--inbox")?)),
                _ if asked.act.is_empty() => asked.act = arg,
                _ if asked.id.is_none() => asked.id = Some(arg),
                other => return Err(format!("unexpected argument {other}")),
            }
        }
        if asked.codex.as_os_str().is_empty() || asked.act.is_empty() {
            return Err("usage: turn --codex <binary> <continue|fork|new> [id] [--worktree] [--model m] [--thinking t] [--inbox path]".into());
        }
        Ok(asked)
    }
}

/// Run one turn; the error is the line for standard error.
pub async fn run(asked: Asked) -> Result<(), String> {
    let mut words = String::new();
    tokio::io::stdin()
        .read_to_string(&mut words)
        .await
        .map_err(|e| format!("reading the words: {e}"))?;
    let cwd = std::env::current_dir().map_err(|e| format!("reading where the turn runs: {e}"))?;
    let place = match asked.worktree && asked.act != "continue" {
        true => made(&cwd)?,
        false => None,
    };
    let place = place.as_deref().unwrap_or(&cwd);
    let (method, params) = opening(&asked.act, asked.id.as_deref(), place)?;
    let mut command = tokio::process::Command::new(&asked.codex);
    command.stderr(Stdio::null());
    let mut server = AppServer::open(command, true)
        .await
        .map_err(|e| e.to_string())?;
    let opened = server
        .call(method, params)
        .await
        .map_err(|e| e.to_string())?;
    let thread = opened
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .ok_or("the app server named no thread")?
        .to_owned();
    let mut current = start(
        &mut server,
        &thread,
        &words,
        asked.model.as_deref(),
        asked.thinking.as_deref(),
    )
    .await
    .map_err(|e| e.to_string())?;
    // Bound once the thread is named: words heard before that would have no turn to go
    // into. A turn that cannot bind runs with no inbox.
    let heard = asked
        .inbox
        .as_ref()
        .and_then(|path| UnixListener::bind(path).ok());
    loop {
        let message = tokio::select! {
            message = server.recv() => message.map_err(|e| e.to_string())?,
            Ok((stream, _)) = accept(heard.as_ref()) => {
                hear(&mut server, &thread, &mut current, stream).await;
                continue;
            }
        };
        if ends(&message, &thread, &current) {
            break;
        }
    }
    // The inbox closes before the app server does: words told from here on are told to a
    // turn that is ending.
    drop(heard);
    if let Some(inbox) = &asked.inbox {
        let _ = std::fs::remove_file(inbox);
    }
    server.close().await;
    Ok(())
}

/// Start a turn in the thread with the words, and name it.
async fn start(
    server: &mut AppServer,
    thread: &str,
    words: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<String, appserver::Failed> {
    let started = server
        .call("turn/start", turn_start(thread, words, model, effort))
        .await?;
    started
        .pointer("/turn/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(appserver::Failed::Ended)
}

/// `turn/start` of the words in the thread, on the `model` and `effort` picked, and
/// nothing of either where nothing was.
fn turn_start(thread: &str, words: &str, model: Option<&str>, effort: Option<&str>) -> Value {
    let mut request = json!({"threadId": thread, "input": appserver::said(words)});
    if let Some(model) = model {
        request["model"] = json!(model);
    }
    if let Some(effort) = effort {
        request["effort"] = json!(effort);
    }
    request
}

async fn accept(
    heard: Option<&UnixListener>,
) -> std::io::Result<(UnixStream, tokio::net::unix::SocketAddr)> {
    match heard {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

/// Words told on the inbox: steered into the turn running, or a turn of their own when
/// that one has ended. The caller hears `heard`, or what the app server said.
async fn hear(server: &mut AppServer, thread: &str, current: &mut String, mut stream: UnixStream) {
    let mut words = String::new();
    let read = tokio::time::timeout(WORDS_WITHIN, stream.read_to_string(&mut words)).await;
    if !matches!(read, Ok(Ok(_))) || words.trim().is_empty() {
        return;
    }
    let steered = server
        .call(
            "turn/steer",
            json!({"threadId": thread, "expectedTurnId": current, "input": appserver::said(&words)}),
        )
        .await;
    let answer = match steered {
        Ok(_) => HEARD.to_owned(),
        Err(_) => match start(server, thread, &words, None, None).await {
            Ok(next) => {
                *current = next;
                HEARD.to_owned()
            }
            Err(error) => error.to_string(),
        },
    };
    let _ = stream.write_all(answer.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Whether a message is `turn/completed` for the turn the driver waits on.
fn ends(message: &Value, thread: &str, turn: &str) -> bool {
    message.get("method").and_then(Value::as_str) == Some("turn/completed")
        && message.pointer("/params/threadId").and_then(Value::as_str) == Some(thread)
        && message.pointer("/params/turn/id").and_then(Value::as_str) == Some(turn)
}

/// The request that opens the thread for an act, in `place`.
fn opening(act: &str, id: Option<&str>, place: &Path) -> Result<(&'static str, Value), String> {
    let cwd = place.to_string_lossy();
    Ok(match (act, id) {
        ("new", _) => ("thread/start", json!({"cwd": cwd})),
        ("fork", Some(id)) => (
            "thread/fork",
            json!({"threadId": id, "cwd": cwd, "excludeTurns": true}),
        ),
        ("continue", Some(id)) => (
            "thread/resume",
            json!({"threadId": id, "excludeTurns": true}),
        ),
        (act, None) => return Err(format!("a {act} names the conversation it acts on")),
        (act, Some(_)) => return Err(format!("Codex runs no {act}")),
    })
}

/// A new worktree of the repository `cwd` is in, where Codex makes its own, and the folder
/// in it that `cwd` is (the worktree itself where that folder holds nothing the repository
/// tracks); none where `cwd` is in no repository.
fn made(cwd: &Path) -> Result<Option<PathBuf>, String> {
    let Some(top) = git(cwd, &["rev-parse", "--show-toplevel"]).map(PathBuf::from) else {
        return Ok(None);
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or("HOME is not set")?;
    let root = crate::home::root(&home)?;
    let name = top.file_name().ok_or("a repository has a name")?;
    let slot = uuid::Uuid::now_v7().simple().to_string();
    let dir = root
        .join("worktrees")
        .join(&slot[slot.len() - 4..])
        .join(name);
    let dir_arg = dir.to_string_lossy().into_owned();
    git(&top, &["worktree", "add", "--detach", &dir_arg, "HEAD"])
        .ok_or_else(|| format!("making a worktree at {dir_arg}"))?;
    let within = cwd
        .canonicalize()
        .ok()
        .and_then(|cwd| {
            let top = top.canonicalize().ok()?;
            cwd.strip_prefix(top).ok().map(Path::to_path_buf)
        })
        .unwrap_or_default();
    let inside = dir.join(within);
    Ok(Some(if inside.is_dir() { inside } else { dir }))
}

/// What `git` said on standard output, where it succeeded.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The act opens the thread it names: a fork or a new one in the place given, a
    /// continue where the thread ran.
    #[test]
    fn each_act_opens_its_thread() {
        let place = Path::new("/w");
        assert_eq!(opening("new", None, place).unwrap().0, "thread/start");
        let (method, params) = opening("fork", Some("t"), place).unwrap();
        assert_eq!(
            (method, params["cwd"].as_str()),
            ("thread/fork", Some("/w"))
        );
        let (method, params) = opening("continue", Some("t"), place).unwrap();
        assert_eq!(method, "thread/resume");
        assert!(params.get("cwd").is_none());
        assert!(opening("continue", None, place).is_err());
        assert!(opening("hand", Some("t"), place).is_err());
    }

    #[test]
    fn a_turn_s_command_line_reads_back() {
        let asked = Asked {
            codex: "/bin/codex".into(),
            act: "fork".into(),
            id: Some("t".into()),
            worktree: true,
            model: Some("gpt-6".into()),
            thinking: Some("high".into()),
            inbox: Some("/f/inbox".into()),
        };
        assert_eq!(Asked::parse(asked.args().into_iter()).unwrap(), asked);
        assert!(Asked::parse(["new".to_owned()].into_iter()).is_err());
    }

    #[test]
    fn a_turn_ends_on_its_own_completion() {
        let done = json!({"method": "turn/completed", "params": {"threadId": "t", "turn": {"id": "u", "status": "completed"}}});
        assert!(ends(&done, "t", "u"));
        assert!(!ends(&done, "t", "v"));
    }

    #[test]
    fn a_turn_starts_on_what_its_person_picked() {
        let asked = turn_start("th", "go", None, None);
        assert!(asked.get("model").is_none() && asked.get("effort").is_none());
        let started = turn_start("th", "go", Some("gpt-6-sol"), Some("xhigh"));
        assert_eq!(
            (&started["threadId"], &started["model"], &started["effort"]),
            (&json!("th"), &json!("gpt-6-sol"), &json!("xhigh"))
        );
    }
}
