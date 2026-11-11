//! A turn of an ACP harness (Kilo, OpenCode, Cline), run by its driver's own binary in its
//! turn mode: the words on standard input, the harness's own ACP server ([`Host::serves`])
//! spoken to — `session/new`, `session/resume` or `session/load` ([`Opens`]), or
//! `session/fork`, the model and the effort set as
//! the session's config options, then `session/prompt` with the words — every message the
//! harness writes and every request this turn sends it written verbatim to standard output,
//! one a line, and out once the harness has answered each prompt. What runs is the
//! conversation the harness's own clients run: the person's configuration, MCP servers,
//! rules and skills.
//!
//! While the turn runs, more words told on its inbox (a unix socket in the turn's folder) go
//! in as the harness takes words mid-turn ([`Told`]); the caller reads back `heard`. A
//! permission the harness asks for goes to Steery's hook (`--hook`, setup's command line),
//! run as `<hook> session/request_permission` with the request's `params` on standard input:
//! its standard output is the harness's answer ([`hooks`]). Without a hook, or where the hook
//! fails or says nothing, the call is rejected, as no person is at the harness to ask. Once
//! the harness runs nothing more of the session, the turn runs `<hook> session/prompt`:
//! words on its standard output go in as one more `session/prompt`, and the turn goes on.
//! SIGTERM or SIGINT cancels the turn (`session/cancel`), so the harness ends its message as
//! aborted instead of leaving it unfinished, and every permission still asked is answered
//! cancelled; a second signal ends the turn at once. A request the harness refuses before
//! the words go in is the one line `<refusal word>: <the harness's line>` on standard error,
//! and the turn fails.

pub mod drive;
pub mod hooks;

use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use steery_driver_common::kernel;
use steery_sdk::refusal;
use steery_sdk::wire::harness::HEARD;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinSet;

use crate::Harness;
use hooks::{ASKS_WAITS, ENDED, ENDED_WAITS, PERMISSION};

/// How long a caller at the inbox has to finish writing its words.
const WORDS_WITHIN: Duration = Duration::from_secs(10);

/// How long the harness has to answer a cancel before the turn ends without its answer.
const CANCELLED_WITHIN: Duration = Duration::from_secs(15);

/// How long the harness has to end once its standard input closes.
const CLOSED_WITHIN: Duration = Duration::from_secs(5);

/// How often the turn looks again whether the harness still runs a turn of the session,
/// once every prompt it sent has its answer.
const LOOKS_EVERY: Duration = Duration::from_millis(100);

/// OpenCode's extension that puts words into the running turn of a session.
const STEERING: &str = "_session/steering";

/// How words told on the inbox go into the harness mid-turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Told {
    /// As one more `session/prompt`, which the harness queues behind the one running.
    Prompt,
    /// Into the running turn, through [`STEERING`]; where no turn runs any more, as one
    /// more `session/prompt`. The harness refuses a second prompt while one runs.
    Steer,
    /// As one prompt once the harness has answered every prompt: it refuses a second prompt
    /// while one runs, and has no stable way to steer words into the running turn.
    After,
}

/// How a continue opens the conversation it goes on with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opens {
    /// `session/resume`: the harness answers without the history.
    Resume,
    /// `session/load`: the harness replays the whole history as `session/update`
    /// notifications before it answers. The replay stays off standard output, where it would
    /// read as the turn answering.
    Load,
}

/// What one harness of the family adds to the turn.
pub trait Host {
    /// The harness.
    fn harness(&self) -> Harness;

    /// How words told on the inbox go in.
    fn told(&self) -> Told;

    /// The arguments that start the harness's ACP server.
    fn serves(&self) -> &'static [&'static str] {
        &["acp"]
    }

    /// The environment the harness's ACP server starts with, beside the turn's own.
    fn env(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// How a continue opens its conversation.
    fn opens(&self) -> Opens {
        Opens::Resume
    }

    /// The config option that sets the session's thinking level.
    fn thinking(&self) -> &'static str {
        "effort"
    }

    /// The request that sets `option` of the session to `value`: by default, as one of its
    /// config options.
    fn pick(&self, session: &str, option: &str, value: &str) -> (&'static str, Value) {
        (
            "session/set_config_option",
            json!({"sessionId": session, "configId": option, "value": value}),
        )
    }

    /// The harness opened `session` for the turn, before any words go in: a harness whose
    /// store does not tell a driven conversation from a headless one marks it here.
    fn opened(&self, _session: &str) -> std::io::Result<()> {
        Ok(())
    }

    /// Whether the turn asks Steery's hook for words to go on once the harness runs
    /// nothing more of the session: not where the harness runs Steery's own hook at a
    /// turn's end, which makes it go on.
    fn goes_on(&self) -> bool {
        true
    }

    /// Whether the harness still runs a turn of `session` once every prompt the turn sent
    /// has its answer, as words another client queued into it can make it; `acp` is the pid
    /// of the harness's ACP server, `queued` the words whose prompt the harness answered
    /// at once, queued behind a turn still running, which it runs later with no prompt
    /// open, and `unmarked` the turn's own memory between looks.
    fn runs(
        &self,
        acp: Option<u32>,
        session: &str,
        queued: &[String],
        unmarked: &mut Option<tokio::time::Instant>,
    ) -> bool;
}

/// What a turn is asked to do, as its command line says it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Asked {
    /// The harness's executable, whose `acp` the turn speaks to.
    pub binary: PathBuf,
    pub act: String,
    pub id: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub inbox: Option<PathBuf>,
    /// Steery's hook command line, which answers the permissions the harness asks for.
    pub hook: Option<String>,
}

impl Asked {
    /// The arguments after `turn`.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--binary".to_owned(),
            self.binary.to_string_lossy().into_owned(),
            self.act.clone(),
        ];
        args.extend(self.id.clone());
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
        if let Some(hook) = &self.hook {
            args.push("--hook".into());
            args.push(hook.clone());
        }
        args
    }

    pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut asked = Self::default();
        while let Some(arg) = args.next() {
            let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
            match arg.as_str() {
                "--binary" => asked.binary = PathBuf::from(value("--binary")?),
                "--model" => asked.model = Some(value("--model")?),
                "--thinking" => asked.thinking = Some(value("--thinking")?),
                "--inbox" => asked.inbox = Some(PathBuf::from(value("--inbox")?)),
                "--hook" => asked.hook = Some(value("--hook")?),
                _ if asked.act.is_empty() => asked.act = arg,
                _ if asked.id.is_none() => asked.id = Some(arg),
                other => return Err(format!("unexpected argument {other}")),
            }
        }
        let named = asked.id.is_some();
        let fits = match asked.act.as_str() {
            "new" => !named,
            "continue" | "fork" => named,
            _ => false,
        };
        if asked.binary.as_os_str().is_empty() || !fits {
            return Err(
                "usage: turn --binary <binary> <new | continue <id> | fork <id>> \
                        [--model m] [--thinking t] [--inbox path] [--hook command]"
                    .into(),
            );
        }
        Ok(asked)
    }
}

/// The turn mode's entry: run the turn its arguments ask for, and say on standard error why
/// it failed.
pub async fn main(host: impl Host, args: impl Iterator<Item = String>) -> std::io::Result<()> {
    let outcome = match Asked::parse(args) {
        Ok(asked) => run(&host, asked).await,
        Err(usage) => Err(format!("{}: {usage}", refusal::START_FAILED)),
    };
    if let Err(line) = outcome {
        eprintln!("{line}");
        std::process::exit(1);
    }
    Ok(())
}

/// Run one turn; the error is the line for standard error.
async fn run(host: &impl Host, asked: Asked) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("{}: {e}", refusal::START_FAILED);
    let mut words = String::new();
    tokio::io::stdin()
        .read_to_string(&mut words)
        .await
        .map_err(failed)?;
    let cwd = std::env::current_dir().map_err(failed)?;
    let mut terms = signal(SignalKind::terminate()).map_err(failed)?;
    let mut interrupts = signal(SignalKind::interrupt()).map_err(failed)?;
    let label = host.harness().label();
    let mut acp = Acp::open(host, &asked.binary).await.map_err(failed)?;
    let outcome = async {
        acp.call(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await?;
        let cwd = cwd.to_string_lossy();
        let session = match (asked.act.as_str(), asked.id.as_deref()) {
            ("continue", Some(id)) => {
                let params = json!({"sessionId": id, "cwd": cwd, "mcpServers": []});
                match host.opens() {
                    Opens::Resume => {
                        acp.call("session/resume", params).await?;
                    }
                    Opens::Load => {
                        acp.replaying = true;
                        let loaded = acp.call("session/load", params).await;
                        acp.replaying = false;
                        loaded?;
                    }
                }
                id.to_owned()
            }
            (act, id) => {
                let (method, params) = match id {
                    Some(id) => (
                        "session/fork",
                        json!({"sessionId": id, "cwd": cwd, "mcpServers": []}),
                    ),
                    None => ("session/new", json!({"cwd": cwd, "mcpServers": []})),
                };
                let opened = acp.call(method, params).await?;
                opened
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{}: {label}'s {act} named no session", refusal::REFUSED))?
            }
        };
        host.opened(&session).map_err(failed)?;
        for (option, value) in [("model", &asked.model), (host.thinking(), &asked.thinking)] {
            if let Some(value) = value {
                let (method, params) = host.pick(&session, option, value);
                acp.call(method, params).await?;
            }
        }
        // The prompts that wait for the harness's answer, with their words, by their ids.
        let mut prompts = HashMap::from([(acp.prompt(&session, &words).await?, words)]);
        // Bound once the session is named: words heard before that would have no turn to
        // go into. A turn that cannot bind runs with no inbox.
        let heard = asked
            .inbox
            .as_ref()
            .and_then(|path| UnixListener::bind(path).ok());
        // Words told on the inbox that wait for the harness to say whether it steered them
        // in, with the caller who told them, by the steering request's id.
        let mut steering: HashMap<u64, (String, UnixStream)> = HashMap::new();
        // Words that wait to go in as one prompt once the harness answered every prompt, as
        // it takes no second prompt while one runs: words it would not steer in, as no turn
        // ran, and every word told to a harness that takes them only after ([`Told::After`]).
        let mut later: Vec<(String, UnixStream)> = Vec::new();
        let mut cancelled: Option<tokio::time::Instant> = None;
        // The permissions that wait on the hook, by their request ids.
        let mut asking = JoinSet::new();
        let mut waiting: Vec<Value> = Vec::new();
        let mut unmarked: Option<tokio::time::Instant> = None;
        // Words whose prompt the harness answered while an older prompt still ran: it queued
        // them, to run once that turn ends.
        let mut queued: Vec<String> = Vec::new();
        // How the harness's answer to the newest prompt says the turn ended, and whether
        // Steery's hook already made the turn go on.
        let mut stopped: Option<String> = None;
        let mut continued = false;
        loop {
        while !prompts.is_empty()
            || !steering.is_empty()
            || !later.is_empty()
            || (cancelled.is_none() && host.runs(acp.child.id(), &session, &queued, &mut unmarked))
        {
            if prompts.is_empty() && !later.is_empty() {
                let told = std::mem::take(&mut later);
                let words = told.iter().map(|(words, _)| words.as_str()).collect::<Vec<_>>().join("\n\n");
                let said = match (cancelled, acp.prompt(&session, &words).await) {
                    (Some(_), _) => "the turn is ending".to_owned(),
                    (None, Ok(id)) => {
                        prompts.insert(id, words);
                        HEARD.to_owned()
                    }
                    (None, Err(line)) => line,
                };
                for (_, stream) in told {
                    reply(stream, &said).await;
                }
            }
            for request in std::mem::take(&mut acp.asks) {
                let Some(id) = request.get("id").cloned() else {
                    continue;
                };
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                match (&asked.hook, cancelled) {
                    (Some(hook), None) => {
                        waiting.push(id.clone());
                        asking.spawn(ask(hook.clone(), id, params));
                    }
                    (None, None) => {
                        let rejection = json!({"outcome": rejected(params.get("options"))});
                        acp.answer(&id, rejection).await?;
                    }
                    (_, Some(_)) => acp.answer(&id, CANCELLED.clone()).await?,
                }
            }
            let deadline = async {
                match cancelled {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                message = acp.next() => {
                    let answered = message?;
                    let id = answered.as_ref().and_then(|message| message.get("id")?.as_u64());
                    if let (Some(message), Some(id)) = (answered, id) {
                        if let Some(words) = prompts.remove(&id) {
                            if message.get("result").is_some() && prompts.keys().any(|open| *open < id) {
                                queued.push(words);
                            } else {
                                let reason = message.pointer("/result/stopReason").and_then(Value::as_str);
                                stopped = Some(reason.unwrap_or("error").to_owned());
                            }
                        } else if let Some((words, stream)) = steering.remove(&id) {
                            match steered(acp.label, &message) {
                                Ok(true) => reply(stream, HEARD).await,
                                Ok(false) => later.push((words, stream)),
                                Err(line) => reply(stream, &line).await,
                            }
                        }
                    }
                }
                Some(Ok((id, answer))) = asking.join_next() => {
                    waiting.retain(|waits| *waits != id);
                    acp.answer(&id, answer).await?;
                }
                Ok((stream, _)) = accept(heard.as_ref()) => {
                    match hear(&mut acp, host.told(), &session, stream).await {
                        Heard::Prompted(id, words) => {
                            prompts.insert(id, words);
                        }
                        Heard::Steering(id, words, stream) => {
                            steering.insert(id, (words, stream));
                        }
                        Heard::Later(words, stream) => later.push((words, stream)),
                        Heard::Not => {}
                    }
                }
                _ = terms.recv() => match cancelled {
                    Some(_) => break,
                    None => {
                        cancelled = Some(acp.cancel(&session).await?);
                        abandon(&mut acp, &mut asking, &mut waiting).await?;
                    }
                },
                _ = interrupts.recv() => match cancelled {
                    Some(_) => break,
                    None => {
                        cancelled = Some(acp.cancel(&session).await?);
                        abandon(&mut acp, &mut asking, &mut waiting).await?;
                    }
                },
                () = deadline => break,
                () = tokio::time::sleep(LOOKS_EVERY), if prompts.is_empty() && steering.is_empty() => {}
            }
        }
        // The harness runs nothing more of the session: Steery's hook may give it words to
        // go on.
        let (Some(hook), None, Some(reason), true) =
            (&asked.hook, cancelled, stopped.take(), host.goes_on())
        else {
            break;
        };
        let Some(words) = go_on(hook, &session, &reason, continued).await else {
            break;
        };
        continued = true;
        unmarked = None;
        prompts.insert(acp.prompt(&session, &words).await?, words);
        }
        // The inbox closes before the harness does: words told from here on are told to a
        // turn that is ending.
        drop(heard);
        Ok(())
    }
    .await;
    if let Some(inbox) = &asked.inbox {
        let _ = std::fs::remove_file(inbox);
    }
    acp.close().await;
    outcome
}

/// The answer to a permission asked in a turn that is cancelled.
static CANCELLED: std::sync::LazyLock<Value> =
    std::sync::LazyLock::new(|| json!({"outcome": {"outcome": "cancelled"}}));

/// End the hooks still running, and answer their permissions cancelled, as ACP asks of a
/// client once it cancels.
async fn abandon(
    acp: &mut Acp,
    asking: &mut JoinSet<(Value, Value)>,
    waiting: &mut Vec<Value>,
) -> Result<(), String> {
    asking.abort_all();
    for id in waiting.drain(..) {
        acp.answer(&id, CANCELLED.clone()).await?;
    }
    Ok(())
}

/// Run Steery's hook at a permission the harness asks for, the request's `params` on its
/// standard input. The harness's answer is the hook's standard output where it is an
/// answer with an `outcome`, else a rejection: a hook that fails, says nothing, or runs
/// past [`ASKS_WAITS`] lets no call run.
async fn ask(hook: String, id: Value, params: Value) -> (Value, Value) {
    let waits = Duration::from_secs(ASKS_WAITS.into());
    let answer = hooked(&hook, PERMISSION, &params, waits)
        .await
        .and_then(|stdout| serde_json::from_slice::<Value>(&stdout).ok())
        .filter(|answer| answer.get("outcome").is_some_and(Value::is_object))
        .unwrap_or_else(|| json!({"outcome": rejected(params.get("options"))}));
    (id, answer)
}

/// Run Steery's hook at the end of the turn; the words it gives the session to go on with.
async fn go_on(hook: &str, session: &str, reason: &str, continued: bool) -> Option<String> {
    let document = json!({"sessionId": session, "stopReason": reason, "continued": continued});
    let stdout = hooked(hook, ENDED, &document, ENDED_WAITS).await?;
    let words = String::from_utf8(stdout).ok()?;
    let words = words.trim();
    (!words.is_empty()).then(|| words.to_owned())
}

/// Run `<hook> <event>` with the document on its standard input; its standard output, where
/// it ends well within `waits`.
async fn hooked(hook: &str, event: &str, document: &Value, waits: Duration) -> Option<Vec<u8>> {
    let mut child = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{hook} {event}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    stdin
        .write_all(document.to_string().as_bytes())
        .await
        .ok()?;
    drop(stdin);
    let output = tokio::time::timeout(waits, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    output.status.success().then_some(output.stdout)
}

async fn accept(
    heard: Option<&UnixListener>,
) -> std::io::Result<(UnixStream, tokio::net::unix::SocketAddr)> {
    match heard {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

/// What became of words told on the inbox.
enum Heard {
    /// They went in as a prompt of this id.
    Prompted(u64, String),
    /// The steering request of this id carries them; the caller waits for its answer.
    Steering(u64, String, UnixStream),
    /// They wait to go in once the harness answered every prompt; the caller hears then.
    Later(String, UnixStream),
    /// They did not go in, and the caller heard why.
    Not,
}

/// Words told on the inbox, sent the way the harness takes words mid-turn. The caller hears
/// `heard`, or why they did not go in, once the harness took them.
async fn hear(acp: &mut Acp, told: Told, session: &str, mut stream: UnixStream) -> Heard {
    let mut words = String::new();
    let read = tokio::time::timeout(WORDS_WITHIN, stream.read_to_string(&mut words)).await;
    let failed = match read {
        Ok(Ok(_)) if !words.is_empty() => None,
        Ok(Ok(_)) => Some("no words came".to_owned()),
        Ok(Err(e)) => Some(format!("the words did not read: {e}")),
        Err(_) => Some("the words did not come in time".to_owned()),
    };
    if let Some(said) = failed {
        reply(stream, &said).await;
        return Heard::Not;
    }
    let sent = match told {
        Told::Prompt => acp.prompt(session, &words).await,
        Told::Steer => acp.steer(session, &words).await,
        Told::After => return Heard::Later(words, stream),
    };
    match sent {
        Ok(id) if told == Told::Prompt => {
            reply(stream, HEARD).await;
            Heard::Prompted(id, words)
        }
        Ok(id) => Heard::Steering(id, words, stream),
        Err(line) => {
            reply(stream, &line).await;
            Heard::Not
        }
    }
}

/// The harness's answer to words steered in: true where they went into the running turn,
/// false where no turn ran to take them, or the harness's refusal.
fn steered(label: &str, answer: &Value) -> Result<bool, String> {
    if let Some(error) = answer.get("error") {
        let said = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("refused");
        return Err(format!("{label} did not take the words: {said}"));
    }
    match answer.pointer("/result/outcome").and_then(Value::as_str) {
        Some("injected") => Ok(true),
        Some("promptRequired") => Ok(false),
        other => Err(format!(
            "{label} answered the words with {}",
            other.unwrap_or("no outcome")
        )),
    }
}

/// Tell the caller at the inbox one line, and close.
async fn reply(mut stream: UnixStream, said: &str) {
    let _ = stream.write_all(format!("{said}\n").as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// A running harness's ACP server: JSON-RPC 2.0, one message a line, on its standard input
/// and output.
struct Acp {
    /// The harness's name, as its lines say it.
    label: &'static str,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Lines<BufReader<ChildStdout>>,
    next: u64,
    /// The harness's permission requests that the turn has yet to answer, whole.
    asks: Vec<Value>,
    /// The steering requests whose answers stay off standard output: the runtime reads an
    /// answer there that refuses as the end of the turn.
    quiet: HashSet<u64>,
    /// Whether the harness replays a conversation's history ([`Opens::Load`]): its
    /// notifications stay off standard output until the load is answered.
    replaying: bool,
}

impl Acp {
    /// Start the host's ACP server in a process group of its own, so a signal meant for the
    /// turn reaches the harness only as the cancel this turn sends; its standard error is
    /// the harness's log, dropped.
    async fn open(host: &impl Host, binary: &std::path::Path) -> std::io::Result<Self> {
        let label = host.harness().label();
        let mut command = std::process::Command::new(binary);
        command
            .args(host.serves())
            .envs(host.env().iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        // Steery's plugin trusts `STEERY_STARTED` only from its own parent, which is this
        // turn: it passes on what the runtime started it as, under its own pid.
        match started() {
            Some(what) => command.env("STEERY_STARTED", format!("{}:{what}", std::process::id())),
            None => command.env_remove("STEERY_STARTED"),
        };
        let mut child = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::other(format!("{label}'s acp has no standard output"))
        })?;
        Ok(Self {
            label,
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next: 0,
            asks: Vec::new(),
            quiet: HashSet::new(),
            replaying: false,
        })
    }

    /// Write one message to the harness, and the same line to standard output.
    async fn send(&mut self, message: &Value) -> Result<(), String> {
        let line = self.write(message).await?;
        out(&line).await
    }

    /// Write one message to the harness alone; the line written.
    async fn write(&mut self, message: &Value) -> Result<String, String> {
        let label = self.label;
        let line = format!("{message}\n");
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| format!("{}: {label}'s input is closed", refusal::REFUSED))?;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("{}: {label} did not take a message: {e}", refusal::REFUSED))?;
        Ok(line)
    }

    /// The next request's id.
    fn id(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    /// Send a request; its id.
    async fn request(&mut self, method: &str, params: Value) -> Result<u64, String> {
        let id = self.id();
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        Ok(id)
    }

    /// The next message from the harness, written to standard output first; none where it
    /// is a permission the harness asks for, which then waits in `asks` for the turn to
    /// answer. Any other request of the harness's is answered here as a method this client
    /// does not have. No answer of this client's is written out, so every answer on standard
    /// output is the harness's; nor is the harness's answer to a steering request, nor a
    /// notification of a history it replays.
    async fn next(&mut self) -> Result<Option<Value>, String> {
        let label = self.label;
        loop {
            let line = self
                .lines
                .next_line()
                .await
                .map_err(|e| format!("{}: {label}'s output did not read: {e}", refusal::REFUSED))?
                .ok_or_else(|| format!("{}: {label} ended before it answered", refusal::REFUSED))?;
            if line.trim().is_empty() {
                continue;
            }
            let message = serde_json::from_str::<Value>(&line).ok();
            let steering = message
                .as_ref()
                .filter(|message| message.get("method").is_none())
                .and_then(|message| message.get("id")?.as_u64())
                .is_some_and(|id| self.quiet.remove(&id));
            let replayed = self.replaying
                && message.as_ref().is_some_and(|message| {
                    message.get("method").is_some() && message.get("id").is_none()
                });
            if !steering && !replayed {
                out(&format!("{line}\n")).await?;
            }
            let Some(message) = message else {
                continue;
            };
            if let (Some(method), Some(id)) = (
                message.get("method").and_then(Value::as_str),
                message.get("id"),
            ) {
                if method == "session/request_permission" {
                    self.asks.push(message);
                    return Ok(None);
                }
                let reply = json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": format!("no {method} here")}});
                self.write(&reply).await?;
                continue;
            }
            return Ok(Some(message));
        }
    }

    /// Call `method` and wait for its answer; the harness's refusal is the line for
    /// standard error, with the refusal word it means.
    async fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.request(method, params).await?;
        loop {
            let Some(message) = self.next().await? else {
                continue;
            };
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let said = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("refused");
                return Err(format!(
                    "{}: {}'s {method}: {said}",
                    word(error),
                    self.label
                ));
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Prompt the session with the words; the prompt's id. The harness answers it when the
    /// turn those words began ends.
    async fn prompt(&mut self, session: &str, words: &str) -> Result<u64, String> {
        let params = json!({"sessionId": session, "prompt": [{"type": "text", "text": words}]});
        self.request("session/prompt", params).await
    }

    /// Steer the words into the session's running turn ([`STEERING`]); the request's id.
    /// Neither the request nor its answer goes to standard output.
    async fn steer(&mut self, session: &str, words: &str) -> Result<u64, String> {
        let id = self.id();
        let params = json!({"sessionId": session, "prompt": [{"type": "text", "text": words}]});
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": STEERING, "params": params}))
            .await?;
        self.quiet.insert(id);
        Ok(id)
    }

    /// Ask the harness to cancel the session's turn; when the turn ends without its answer.
    async fn cancel(&mut self, session: &str) -> Result<tokio::time::Instant, String> {
        self.send(&json!({"jsonrpc": "2.0", "method": "session/cancel",
            "params": {"sessionId": session}}))
            .await?;
        Ok(tokio::time::Instant::now() + CANCELLED_WITHIN)
    }

    /// Answer one of the harness's requests.
    async fn answer(&mut self, id: &Value, result: Value) -> Result<(), String> {
        self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await
            .map(drop)
    }

    /// Close the harness's input, which ends it, and end whatever of it still runs after
    /// [`CLOSED_WITHIN`].
    async fn close(mut self) {
        drop(self.stdin.take());
        let running = self.child.id().map(|pid| {
            let mut running = kernel::descendants(pid);
            running.extend(kernel::started(pid).map(|start| (pid, start)));
            running
        });
        if tokio::time::timeout(CLOSED_WITHIN, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.start_kill();
        }
        if let Some(running) = running {
            kernel::signal(&running, libc::SIGKILL);
        }
    }
}

/// What the runtime started this turn as (`STEERY_STARTED=<pid>:<what>`), where the pid is
/// this process's parent, as PROTOCOL.md §3.3 trusts it.
fn started() -> Option<String> {
    let value = std::env::var("STEERY_STARTED").ok()?;
    let (pid, what) = value.split_once(':')?;
    let pid: u32 = pid.parse().ok()?;
    (pid > 1 && pid == std::os::unix::process::parent_id()).then(|| what.to_owned())
}

/// One line on standard output, whole.
async fn out(line: &str) -> Result<(), String> {
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(line.as_bytes())
        .await
        .and(stdout.flush().await)
        .map_err(|e| format!("{}: the turn's output is closed: {e}", refusal::REFUSED))
}

/// The answer that rejects a permission once: the harness's `reject_once` option, else
/// cancelled.
fn rejected(options: Option<&Value>) -> Value {
    options
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|option| option.get("kind").and_then(Value::as_str) == Some("reject_once"))
        .and_then(|option| option.get("optionId"))
        .map_or_else(
            || json!({"outcome": "cancelled"}),
            |id| json!({"outcome": "selected", "optionId": id}),
        )
}

/// The refusal word an error of the family's ACP means, by what its data names (Kilo's and
/// OpenCode's `acp/error.ts`): a session it does not have, a model, an effort or a config
/// option it does not know.
pub(super) fn word(error: &Value) -> &'static str {
    let data = error.get("data");
    let names = |key: &str| data.and_then(|data| data.get(key)).is_some();
    if names("sessionId") {
        refusal::NOT_FOUND
    } else if names("modelId") {
        refusal::MODEL_MISSING
    } else if names("effort") || names("configId") {
        refusal::SELECTION_UNSUPPORTED
    } else {
        refusal::REFUSED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_s_command_line_reads_back_as_it_was_asked() {
        let asked = Asked {
            binary: "/opt/homebrew/bin/kilo".into(),
            act: "fork".into(),
            id: Some("ses_1".into()),
            model: Some("kilo/anthropic/claude-opus-4.5".into()),
            thinking: Some("high".into()),
            inbox: Some("/tmp/t/inbox".into()),
            hook: Some("'/opt/steery/bin/steery' hook kilo".into()),
        };
        assert_eq!(Asked::parse(asked.args().into_iter()), Ok(asked));
        let new = |args: &[&str]| Asked::parse(args.iter().map(|arg| (*arg).to_owned()));
        assert!(new(&["--binary", "kilo", "new"]).is_ok());
        assert!(new(&["--binary", "kilo", "new", "ses_1"]).is_err());
        assert!(new(&["--binary", "kilo", "continue"]).is_err());
        assert!(new(&["--binary", "kilo", "hand", "ses_1"]).is_err());
        assert!(new(&["new"]).is_err());
    }

    #[test]
    fn acp_errors_mean_their_refusal_words() {
        let error = |data: Value| json!({"code": -32602, "message": "x", "data": data});
        assert_eq!(
            word(&error(json!({"sessionId": "ses_x"}))),
            refusal::NOT_FOUND
        );
        assert_eq!(
            word(&error(json!({"providerId": "kilo", "modelId": "nope"}))),
            refusal::MODEL_MISSING
        );
        assert_eq!(
            word(&error(json!({"effort": "max"}))),
            refusal::SELECTION_UNSUPPORTED
        );
        assert_eq!(
            word(&json!({"code": -32000, "message": "auth"})),
            refusal::REFUSED
        );
    }

    #[test]
    fn a_permission_is_rejected_once() {
        let options = json!([
            {"optionId": "once", "kind": "allow_once"},
            {"optionId": "reject", "kind": "reject_once"},
        ]);
        assert_eq!(
            rejected(Some(&options)),
            json!({"outcome": "selected", "optionId": "reject"})
        );
        assert_eq!(rejected(None), json!({"outcome": "cancelled"}));
    }
}
