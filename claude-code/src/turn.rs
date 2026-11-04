//! One turn of Claude Code, run by this driver's own binary in its turn mode, and its
//! lines read back.
//!
//! `steery-driver-claude-code turn <claude> <act> [<id>] [--worktree] [--model <model>]
//! [--thinking <effort>] [--reads <folder>]…` takes the words on standard input, starts a
//! background session with `claude --bg … -- <words>` (the words follow `--`, which Claude
//! reads as the words and never as a flag or a file), and follows it through `claude agents
//! --json` to where its turn stopped. Every change in how the session is listed goes to
//! standard output as `{"listed": <entry>}`, and the entry that settles the turn as
//! `{"settled": <entry>}`, each with `"took"`: whether the session's transcript holds the
//! person's input carrying the words' delivery marker yet (null for words carrying none);
//! what Claude Code refused, in its own words, goes to standard error, and the mode exits 1.
//!
//! Where the listing leaves a question — whether a stop it shows is this turn's, and what
//! went wrong — the session's own `Stop` and `StopFailure` hooks answer it: the launch
//! gives that one session two more hooks (`--settings`), which run this binary's `heard`
//! mode and hand their documents to the follower ([`Heard`]). A row whose process started
//! after the launch is this turn's without them, and an error no `StopFailure` named is
//! read from the transcript's end.
//!
//! Steery's own variables (`STEERY_STARTED`) reach the session through the same
//! `--settings`, as its `env`, and never through the launch's environment: Claude Code's
//! supervisor keeps the environment of the launch that started it for every session after
//! ([`follow`]).
//!
//! A `continue` of a conversation Claude Code keeps a background job for wakes that job
//! with `--bg --resume <id>` and nothing else ([`Kept`]): Claude Code 2.1.317 wakes it with
//! the options it saved and starts a copy for any flag beside it, so the job goes on with
//! the settings, hooks and variables its first launch gave it, and a pick it would not run
//! is refused first. A `continue` never starts beside a session that is in the
//! conversation: Claude Code would start a copy. One already there is refused before
//! anything starts, and a copy Claude Code started anyway, for any reason it has for one
//! ([`agents::COPIED`]), is stopped and removed at once, and the turn refused in Claude
//! Code's own words. Claude refuses a background session in a folder the person has not
//! trusted in Claude, as it would at their own terminal.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use steery_driver_common::{anthropic, receipt};
use steery_sdk::wire::harness::{Act, Ended, Stream, TurnLine, TurnRead, TurnState};

use crate::agents::{self, Follow, RUNNING};

/// The levels `claude --effort` takes, least first (`claude --help`).
pub const EFFORT: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The effort levels Claude Code takes for `model`, least first
/// (https://code.claude.com/docs/en/model-config, "Adjust effort level"): none for Haiku 4.5;
/// every level but `xhigh` for the 4.6 models; every level for the rest, the `haiku` alias
/// too, which runs Haiku 5.5 on the Anthropic API. Where a provider serves an older Haiku
/// under it, Claude Code runs a level the model does not take at the highest it does (same
/// section).
pub fn efforts(model: &str) -> &'static [&'static str] {
    let model = model.strip_suffix("[1m]").unwrap_or(model);
    if model.contains("claude-haiku-4-5") {
        &[]
    } else if model.contains("-4-6") {
        &["low", "medium", "high", "max"]
    } else {
        &EFFORT
    }
}

/// The word that starts the turn mode, as `start`'s plan runs this binary.
pub const MODE: &str = "turn";

/// How often a background session is read back while its turn runs. Reading it costs
/// Claude Code about a second.
const FOLLOW_EVERY: Duration = Duration::from_secs(1);

/// How long a session `claude --bg` started has to be listed.
const LISTED_WITHIN: Duration = Duration::from_secs(90);

/// How long a turn is followed while its sessions cannot be listed. Claude Code replaces
/// itself as it updates, and its session goes on meanwhile: an npm global update from
/// 2.1.314 to 2.1.316 left no `claude` at its path for 35 s, and every turn followed then
/// read as never started while its session worked on.
const UNLISTED_FOR: Duration = Duration::from_secs(300);

/// How long Claude Code gives the `heard` hook: it appends one line to a file.
const HEARD_WAITS: u32 = 5;

/// The word that starts the hook mode a turn's own session runs as its turn ends.
pub const HEARD: &str = "heard";

/// The hooks of the session's own turn end ([`Heard`]).
const ENDS: [&str; 2] = ["Stop", "StopFailure"];

/// What the name of each of Steery's own variables begins with: the ones the runtime gives a
/// turn's process, which only the session it starts carries on ([`follow`]).
const STEERY: &str = "STEERY_";

/// A turn, as `start` plans it and the turn mode reads it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub binary: PathBuf,
    pub act: Act,
    pub id: Option<String>,
    pub worktree: bool,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub reads: Vec<PathBuf>,
}

impl Turn {
    /// The turn mode's arguments, after the mode's own word.
    pub fn args(&self) -> Result<Vec<String>, String> {
        let mut args = vec![
            text(&self.binary)?.to_owned(),
            act_word(self.act).to_owned(),
        ];
        args.extend(self.id.clone());
        if self.worktree && self.act != Act::Continue {
            args.push("--worktree".into());
        }
        if let Some(model) = &self.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(thinking) = &self.thinking {
            args.extend(["--thinking".into(), thinking.clone()]);
        }
        for folder in &self.reads {
            args.extend(["--reads".into(), text(folder)?.to_owned()]);
        }
        Ok(args)
    }

    /// The turn [`Turn::args`] says.
    pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let binary = PathBuf::from(args.next().ok_or("the turn names no claude")?);
        let act = match args.next().as_deref() {
            Some("continue") => Act::Continue,
            Some("fork") => Act::Fork,
            Some("new") => Act::New,
            other => return Err(format!("no act a turn runs: {other:?}")),
        };
        let mut turn = Self {
            binary,
            act,
            id: None,
            worktree: false,
            model: None,
            thinking: None,
            reads: Vec::new(),
        };
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or(format!("{arg} takes a value"));
            match arg.as_str() {
                "--worktree" => turn.worktree = true,
                "--model" => turn.model = Some(value()?),
                "--thinking" => turn.thinking = Some(value()?),
                "--reads" => turn.reads.push(PathBuf::from(value()?)),
                id if turn.id.is_none() && !id.starts_with('-') => turn.id = Some(id.to_owned()),
                other => return Err(format!("no argument a turn takes: {other}")),
            }
        }
        Ok(turn)
    }

    /// The command line that starts a background session for the act, before `--` and
    /// the words. A folder outside its own the session reads is granted it (`--add-dir`):
    /// Claude Code asks its person before a read anywhere else. Where its person accepted
    /// Claude Code's bypass disclaimer (`bypass`, [`bypass_accepted`]), bypass mode is made
    /// available, never turned on, so a person's "everything, this session" in Steery, a
    /// `setMode` to `bypassPermissions`, takes effect: Claude Code ignores that update in a
    /// session launched without it (https://code.claude.com/docs/en/hooks, "Permission
    /// update entries"). Before that acceptance Claude Code refuses to start a background
    /// session that names the flag at all. A `continue` of a session Claude Code keeps a
    /// background job for (`kept`, [`Kept`]) is `--bg --resume <id>` alone: Claude Code
    /// wakes it with the options the job saved, and starts a copy for any flag passed.
    fn launching(&self, bypass: bool, kept: bool) -> Result<Vec<String>, String> {
        let mut args: Vec<String> = vec!["--bg".into()];
        if let (Act::Continue, Some(id), true) = (self.act, self.id.as_deref(), kept) {
            args.extend(["--resume".into(), id.into()]);
            return Ok(args);
        }
        if bypass {
            args.push("--allow-dangerously-skip-permissions".into());
        }
        match (self.act, self.id.as_deref()) {
            (Act::New, _) => {}
            (Act::Continue, Some(id)) => args.extend(["--resume".into(), id.into()]),
            (Act::Fork, Some(id)) => {
                args.extend(["--resume".into(), id.into(), "--fork-session".into()]);
            }
            (Act::Continue | Act::Fork, None) => {
                return Err("a continue or a fork names the conversation it acts on".into());
            }
            (Act::Hand, _) => return Err("a hand over is no turn".into()),
        }
        if self.worktree && self.act != Act::Continue {
            args.push("-w".into());
        }
        for folder in &self.reads {
            args.extend(["--add-dir".into(), text(folder)?.to_owned()]);
        }
        if let Some(model) = &self.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(thinking) = &self.thinking
            && self
                .model
                .as_deref()
                .is_none_or(|model| !efforts(model).is_empty())
        {
            args.extend(["--effort".into(), thinking.clone()]);
        }
        Ok(args)
    }
}

fn act_word(act: Act) -> &'static str {
    match act {
        Act::Continue => "continue",
        Act::Fork => "fork",
        Act::New => "new",
        Act::Hand => "hand",
    }
}

fn text(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("{} is not text", path.display()))
}

/// Whether the person accepted Claude Code's bypass-permissions disclaimer, as Claude Code
/// 2.1.310 reads it before a `--bg` launch may name bypass: `skipDangerousModePermissionPrompt`
/// in their user, local or managed settings, or `bypassPermissionsModeAccepted` in its
/// state file. `place` is the folder the session starts in, whose local settings count.
fn bypass_accepted(home: &Path, root: &Path, place: &Path) -> bool {
    let set = |file: &Path, key: &str| {
        std::fs::read(file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|value| value.get(key) == Some(&Value::Bool(true)))
    };
    let mut settings = vec![root.join("settings.json"), place.join(".claude/settings.local.json")];
    settings.extend(crate::drive::managed_files());
    settings
        .iter()
        .any(|file| set(file, "skipDangerousModePermissionPrompt"))
        || set(&crate::state_file(home, root), "bypassPermissionsModeAccepted")
}

/// The turn mode: run the turn its arguments say, the words on standard input. Exits 0
/// once the turn settled, 1 with Claude Code's words on standard error where it did not.
/// A runtime stops a turn by signalling its process group, so a SIGTERM, a SIGINT or a
/// SIGHUP ends the follow there, which drops everything it made ([`Heard`]), and ends the
/// session the launch started through Claude Code's own `claude stop`, which keeps its
/// conversation ([`halt`]): that session runs in a process of Claude Code's supervisor,
/// outside the group and carrying nothing of the turn's, so no signal of the runtime's
/// reaches it.
pub fn run(args: impl Iterator<Item = String>) -> std::process::ExitCode {
    let ran = (|| {
        let turn = Turn::parse(args)?;
        let words = std::io::read_to_string(std::io::stdin())
            .map_err(|e| format!("reading the words: {e}"))?;
        let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
        let root = crate::root(Path::new(&home))?;
        let place = std::env::current_dir().map_err(|e| format!("finding the place: {e}"))?;
        let bypass = bypass_accepted(Path::new(&home), &root, &place);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?
            .block_on(async {
                use tokio::signal::unix::{SignalKind, signal};
                let hear = |kind| signal(kind).map_err(|e| format!("hearing signals: {e}"));
                let mut term = hear(SignalKind::terminate())?;
                let mut int = hear(SignalKind::interrupt())?;
                let mut hup = hear(SignalKind::hangup())?;
                let mut out = std::io::stdout();
                let job = std::cell::OnceCell::new();
                let signalled = tokio::select! {
                    followed = follow(&turn, &root, bypass, &words, &job, &mut out) => {
                        return followed;
                    }
                    _ = term.recv() => "SIGTERM",
                    _ = int.recv() => "SIGINT",
                    _ = hup.recv() => "SIGHUP",
                };
                Err(halt(&turn.binary, &root, job.get(), signalled).await)
            })
    })();
    match ran {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(said) => {
            eprintln!("{}", said.replace('\n', " "));
            std::process::ExitCode::FAILURE
        }
    }
}

/// [`run`], the words in hand: start the session, then follow it to where its turn
/// stopped, writing each change to `out`. `bypass`: whether the launch may make bypass
/// mode available ([`Turn::launching`]). `job` is given the background session's job once
/// the launch names it, for [`halt`].
async fn follow(
    turn: &Turn,
    root: &Path,
    bypass: bool,
    words: &str,
    job: &std::cell::OnceCell<String>,
    out: &mut impl Write,
) -> Result<(), String> {
    let registry = crate::registry::dir(root);
    let kept = match (turn.act, turn.id.as_deref()) {
        (Act::Continue, Some(id)) => Kept::of(&turn.binary, root, &registry, id).await?,
        _ => None,
    };
    if let Some(kept) = &kept {
        kept.takes(turn)?;
    }
    let goes_on = turn.id.as_deref().filter(|_| turn.act == Act::Continue);
    let mut taking = Taking::before(words, root, goes_on);
    let launched = now();
    // A woken job keeps the hooks its first launch gave it, which find no file to write
    // to; every other launch gives its session its own.
    let heard = match &kept {
        Some(_) => None,
        None => Some(Heard::new(launched)?),
    };
    // Steery's own variables (`STEERY_STARTED`, PROTOCOL.md §3.3) reach the session this
    // launch starts through its own settings, never through the launch's environment:
    // Claude Code runs every background session in a process of its supervisor's, which
    // passes on none of a launch's variables and keeps the environment of whichever launch
    // started it. Passed down, the first turn's `STEERY_STARTED` named every session after
    // it (2.1.317: a session started at 11:38 carried the `turn@` of 11:30), so their agents'
    // `steery` calls were filed under that turn's session and its stop stopped them all.
    // The settings' `env` outranks the supervisor's (2.1.317). A session Claude Code keeps
    // a background job for is given nothing ([`Kept`]): it goes on with the settings its
    // own launch gave it, which Claude Code gives it again whenever it wakes it.
    let steery: serde_json::Map<String, Value> = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(name, _)| name.starts_with(STEERY))
        .map(|(name, value)| (name, Value::String(value)))
        .collect();
    let mut command = tokio::process::Command::new(&turn.binary);
    for name in steery.keys() {
        command.env_remove(name);
    }
    command.stdin(std::process::Stdio::null());
    command.args(turn.launching(bypass, kept.is_some())?);
    // The key is read through the helper only where Claude Code has no sign-in of its own:
    // a helper outranks a plan's sign-in, and a person signed in with their plan who also
    // exports a key they never approved keeps their plan, as Claude Code itself would.
    let keyed = std::env::var_os(anthropic::VARIABLE).is_some_and(|key| !key.is_empty())
        && crate::drive::signed_in(false).await == Some(false);
    if kept.is_none()
        && let Some(settings) = settings(heard.as_ref(), Some(&steery), keyed)?
    {
        command.args(["--settings".to_owned(), settings]);
    }
    let launch = command
        .arg("--")
        .arg(words)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("starting {}: {e}", turn.binary.display()))?;
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&launch.stdout),
        String::from_utf8_lossy(&launch.stderr)
    );
    if turn.act == Act::Continue && agents::copied(&said) {
        if let Some(copy) = agents::backgrounded(&said) {
            discard(&turn.binary, &copy).await;
        }
        let note = said.lines().find(|line| agents::copied(line)).unwrap_or_default();
        return Err(format!(
            "{}: the copy Claude Code started instead of going on in session {} was stopped \
             and removed",
            note.trim(),
            turn.id.as_deref().unwrap_or_default()
        ));
    }
    let short = match (launch.status.success(), agents::backgrounded(&said)) {
        (true, Some(short)) => short,
        _ => return Err(said.trim().to_owned()),
    };
    let _ = job.set(short.clone());
    let listed_by = tokio::time::Instant::now() + LISTED_WITHIN;
    let mut last: Option<Value> = None;
    let mut follow = Follow::default();
    let mut unlisted_since: Option<tokio::time::Instant> = None;
    loop {
        let listed = match agents::agents(&turn.binary).await {
            Ok(listed) => {
                unlisted_since = None;
                listed
            }
            Err(error) => {
                let since = *unlisted_since.get_or_insert_with(tokio::time::Instant::now);
                if since.elapsed() >= UNLISTED_FOR {
                    return Err(format!(
                        "{error}, and for {} s no listing came",
                        since.elapsed().as_secs()
                    ));
                }
                tokio::time::sleep(FOLLOW_EVERY).await;
                continue;
            }
        };
        match listed
            .into_iter()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(&short))
        {
            Some(mut entry) => {
                let short_of_done = agents::end(&entry).is_some()
                    && entry.get("state").and_then(Value::as_str) != Some("done");
                let end = heard.as_ref().and_then(Heard::end);
                // What went wrong as the turn's own StopFailure said it, else as Claude Code
                // recorded its own error in the transcript: StopFailure names API errors, and
                // a person's settings may run no hooks.
                if short_of_done
                    && let Some(error) = end.as_ref().and_then(failed_on).or_else(|| {
                        let session = entry.get("sessionId").and_then(Value::as_str)?;
                        let transcript = crate::transcripts::Store::new(root.to_owned())
                            .session(session)
                            .ok()?
                            .transcripts
                            .pop()?;
                        stopped_on(&transcript, 0)
                    })
                    && let Some(object) = entry.as_object_mut()
                {
                    object.insert("error".into(), Value::from(error));
                }
                // The stop is this turn's where its own hook said so, or where the row is
                // of the process this launch started: hooks may not run at all.
                let ended = || end.is_some() || agents::started_since(&entry, launched);
                // Each line says whether the transcript showed the words taken yet, where
                // they carry a delivery to know them by, and a look that first shows them
                // says the entry again.
                let newly = taking.look(root, &entry);
                let took = taking.delivery.is_some().then_some(taking.took);
                if follow.settled_by(&entry, ended) {
                    return say(out, json!({ "settled": entry, "took": took }));
                }
                if newly || last.as_ref() != Some(&entry) {
                    say(out, json!({ "listed": entry, "took": took }))?;
                }
                last = Some(entry);
            }
            None if last.is_some() => {
                return Err(format!(
                    "background session {short} ended before its turn did"
                ));
            }
            None if tokio::time::Instant::now() >= listed_by => {
                return Err(format!("background session {short} was never listed"));
            }
            None => {}
        }
        tokio::time::sleep(FOLLOW_EVERY).await;
    }
}

fn say(out: &mut impl Write, line: Value) -> Result<(), String> {
    writeln!(out, "{line}")
        .and_then(|()| out.flush())
        .map_err(|e| format!("writing the turn's line: {e}"))
}

/// Whether the session took the words, as its transcript shows them: the person's input
/// carrying the words' delivery marker (PROTOCOL.md §3.3), past where the conversation's
/// transcript ended before the launch.
struct Taking {
    /// The delivery the words carry; none where they carry no marker.
    delivery: Option<String>,
    /// The transcript, once known, and where it was last read to.
    read: Option<(PathBuf, receipt::End)>,
    took: bool,
}

impl Taking {
    /// Before the launch: the delivery `words` carry, and where the transcript of the
    /// conversation `id` the launch goes on in ends now.
    fn before(words: &str, root: &Path, id: Option<&str>) -> Self {
        let read = id.and_then(|id| {
            let path = crate::transcripts::Store::new(root.to_owned())
                .session(id)
                .ok()?
                .transcripts
                .pop()?;
            let end = receipt::end(&path).ok()?;
            Some((path, end))
        });
        Self {
            delivery: delivery(words),
            read,
            took: false,
        }
    }

    /// Whether the transcript of the session `entry` lists newly shows the words taken.
    /// Each look reads on from where the last one ended.
    fn look(&mut self, root: &Path, entry: &Value) -> bool {
        let Some(delivery) = self.delivery.as_deref().filter(|_| !self.took) else {
            return false;
        };
        let Some(session) = entry.get("sessionId").and_then(Value::as_str) else {
            return false;
        };
        let known = self
            .read
            .as_ref()
            .filter(|(path, _)| path.file_stem().and_then(|stem| stem.to_str()) == Some(session))
            .map(|(path, end)| (path.clone(), *end));
        let Some((path, from)) = known.or_else(|| {
            let path = crate::transcripts::Store::new(root.to_owned())
                .session(session)
                .ok()?
                .transcripts
                .pop()?;
            // A transcript new to this look is read whole.
            Some((
                path,
                receipt::End {
                    dev: 0,
                    ino: 0,
                    bytes: 0,
                },
            ))
        }) else {
            return false;
        };
        let to = receipt::end(&path).ok();
        self.took =
            receipt::found(&path, &from, delivery, crate::drive::input_carries).unwrap_or(false);
        self.read = Some((path, to.unwrap_or(from)));
        self.took
    }
}

/// The delivery the words carry: the name after their first `steery-delivery:`, up to the
/// first character no name holds (PROTOCOL.md §3.3).
fn delivery(words: &str) -> Option<String> {
    let (_, rest) = words.split_once(receipt::DELIVERED)?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Now, in milliseconds since 1970.
fn now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or_default()
}

/// The background job Claude Code keeps for the conversation a `continue` goes on in:
/// `jobs/<the first 8 of its id>/state.json` under its configuration root, naming that
/// conversation, with the flags it wakes it with (`respawnFlags`) and the provider's
/// variables it saved (`providerEnv`). Claude Code wakes such a job with the options it
/// saved, and starts a copy for any flag passed beside `--resume` (2.1.317, `own-options`),
/// so the continue passes none ([`Turn::launching`]), and a pick the job would not run is
/// refused before anything starts ([`Kept::takes`]).
struct Kept {
    job: String,
    model: Option<String>,
    effort: Option<String>,
    provider: serde_json::Map<String, Value>,
}

impl Kept {
    /// The job kept for conversation `id`, none where Claude Code keeps none for it. A
    /// process in the conversation now holds it, and is refused: the registry names a live
    /// one, or `claude agents` lists a session in it with a process behind it (one stopped
    /// keeps its row with no process, and holds nothing), and Claude Code would start a
    /// copy beside it.
    async fn of(
        binary: &Path,
        root: &Path,
        registry: &Path,
        id: &str,
    ) -> Result<Option<Self>, String> {
        let running = crate::registry::live(registry, id).is_some()
            || agents::agents(binary).await?.iter().any(|entry| {
                entry.get("sessionId").and_then(Value::as_str) == Some(id)
                    && entry.get("pid").and_then(Value::as_u64).is_some()
            });
        if running {
            return Err(format!("session {id} {}", RUNNING[0]));
        }
        let job = id.get(..8).unwrap_or(id);
        let Some(state) = std::fs::read(root.join("jobs").join(job).join("state.json"))
            .ok()
            .and_then(|text| serde_json::from_slice::<Value>(&text).ok())
            .filter(|state| state.get("sessionId").and_then(Value::as_str) == Some(id))
        else {
            return Ok(None);
        };
        let flags: Vec<&str> = state
            .get("respawnFlags")
            .and_then(Value::as_array)
            .map(|flags| flags.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let after = |flag: &str| {
            let at = flags.iter().position(|given| *given == flag)?;
            flags.get(at + 1).map(|value| (*value).to_owned())
        };
        Ok(Some(Self {
            job: job.to_owned(),
            model: after("--model"),
            effort: after("--effort"),
            provider: state
                .get("providerEnv")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        }))
    }

    /// Whether the job runs the turn's pick anyway: the model its flags name, else its
    /// provider's `ANTHROPIC_MODEL`, and the effort its flags name. Any other pick is
    /// refused in Claude Code's own words for it ([`agents::OWN_OPTIONS`]), which read as
    /// `selection-unsupported`: woken, the job would run its own, and a fork takes another.
    fn takes(&self, turn: &Turn) -> Result<(), String> {
        let runs = self
            .model
            .as_deref()
            .or_else(|| self.provider.get("ANTHROPIC_MODEL").and_then(Value::as_str));
        if let Some(picked) = turn.model.as_deref()
            && runs.is_none_or(|runs| self.resolved(runs) != self.resolved(picked))
        {
            return Err(format!(
                "background session {} {}: it runs {}, not {picked}",
                self.job,
                agents::OWN_OPTIONS,
                runs.unwrap_or("Claude Code's default model")
            ));
        }
        if let Some(picked) = turn.thinking.as_deref()
            && turn
                .model
                .as_deref()
                .or(runs)
                .is_none_or(|model| !efforts(model).is_empty())
            && self.effort.as_deref() != Some(picked)
        {
            return Err(format!(
                "background session {} {}: it runs at {}, not {picked}",
                self.job,
                agents::OWN_OPTIONS,
                self.effort.as_deref().unwrap_or("Claude Code's default effort")
            ));
        }
        Ok(())
    }

    /// A model as the job's provider runs it: a family's alias (`opus`, `sonnet`, `haiku`)
    /// as its `ANTHROPIC_DEFAULT_<FAMILY>_MODEL` names it, where the provider names one.
    fn resolved<'a>(&'a self, model: &'a str) -> &'a str {
        ["opus", "sonnet", "haiku"]
            .contains(&model)
            .then(|| {
                self.provider
                    .get(&format!("ANTHROPIC_DEFAULT_{}_MODEL", model.to_uppercase()))
                    .and_then(Value::as_str)
            })
            .flatten()
            .unwrap_or(model)
    }
}

/// Stop and remove a background session nobody asked for: `claude stop`, then `claude
/// rm`. What it could not do goes to standard error.
async fn discard(binary: &Path, job: &str) {
    for verb in ["stop", "rm"] {
        match agents::said(binary, &[verb, job], agents::ASKED_WITHIN).await {
            Ok(out) if out.status.success() => {}
            Ok(out) => eprintln!(
                "`claude {verb} {job}` did not discard the copy: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(error) => eprintln!("`claude {verb} {job}` did not discard the copy: {error}"),
        }
    }
}

/// How long `claude stop` has as a turn is stopped: inside the 3 s the runtime gives every
/// graceful stop before it signals what is left of the turn (PROTOCOL.md §4.2, `stop`).
const HALT_WITHIN: Duration = Duration::from_secs(2);

/// The turn stopped by `signal`: the background session the launch started, where it named
/// one (`job`), is ended through `claude stop`, which keeps its conversation, so the turn
/// stops with the follower that stood for it, and so is every command the session's
/// process had started that outlived it ([`crate::drive::end_left`]): they run outside the
/// turn's group too. The line says what came of it.
async fn halt(binary: &Path, root: &Path, job: Option<&String>, signal: &str) -> String {
    let Some(job) = job else {
        return format!("stopped by {signal} before the launch named its background session");
    };
    let deadline = tokio::time::Instant::now() + HALT_WITHIN + crate::drive::COMMANDS_END_WITHIN;
    let running = crate::registry::living(&crate::registry::dir(root))
        .find(|session| session.job_id.as_deref() == Some(job.as_str()))
        .and_then(|session| u32::try_from(session.pid).ok())
        .map(crate::registry::kernel::descendants)
        .unwrap_or_default();
    match agents::said(binary, &["stop", job], HALT_WITHIN).await {
        Ok(out) if out.status.success() => {
            let ended = crate::drive::end_left(&running, deadline).await;
            format!("stopped by {signal}; `claude stop {job}` ended its background session{ended}")
        }
        Ok(out) => format!(
            "stopped by {signal}; `claude stop {job}` did not end its background session: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(error) => format!(
            "stopped by {signal}; `claude stop {job}` did not end its background session: {error}"
        ),
    }
}

/// What a background session reads its key from where the turn holds `ANTHROPIC_API_KEY`
/// and Claude Code has no sign-in of its own: that variable, printed by a shell. Claude
/// Code 2.1.310 uses the variable itself only in `-p`; an interactive session, `--bg`
/// among them, uses it only once its person approved the key's last 20 characters in
/// `customApiKeyResponses`, and otherwise answered "Not
/// logged in · Please run /login" (a fake key, a fresh home). With this helper in the
/// launch's flag settings the same launch sent the key ("401 invalid x-api-key"), with no
/// prompt, also under a supervisor started without the variable. The key stays out of
/// every argument and file.
const KEY_HELPER: &str = r#"printf %s "$ANTHROPIC_API_KEY""#;

/// The launch's `--settings`, none where it has nothing to say: the hooks that hand the
/// turn's end back ([`Heard`]), Steery's variables the session's processes carry (`env`,
/// [`STEERY`]), and [`KEY_HELPER`] where the turn is `keyed`.
fn settings(
    heard: Option<&Heard>,
    env: Option<&serde_json::Map<String, Value>>,
    keyed: bool,
) -> Result<Option<String>, String> {
    let mut settings = serde_json::Map::new();
    if let Some(heard) = heard {
        settings.insert("hooks".into(), heard.hooks()?);
    }
    if let Some(env) = env.filter(|env| !env.is_empty()) {
        settings.insert("env".into(), Value::Object(env.clone()));
    }
    if keyed {
        settings.insert("apiKeyHelper".into(), json!(KEY_HELPER));
    }
    Ok((!settings.is_empty()).then(|| Value::Object(settings).to_string()))
}

/// Where the session a turn launches hands over its own turn ends: its `Stop` and
/// `StopFailure` documents, one per line, written by this binary's [`HEARD`] mode. Made
/// empty before the launch, so every document in it is of a turn this launch began, and
/// removed once the follower is done; the hooks stay on the session for its later turns,
/// and find no file to write to.
struct Heard(PathBuf);

impl Heard {
    fn new(launched: u64) -> Result<Self, String> {
        // Two turns this process launches in one second each get a file of their own.
        static MADE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let made = MADE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "steery-claude-code-turn-{}-{launched}-{made}.jsonl",
            std::process::id()
        ));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("making {}: {e}", path.display()))?;
        Ok(Self(path))
    }

    /// The hooks that run this binary's [`HEARD`] mode on the session's turn ends
    /// ([`ENDS`]), beside every hook its person's own settings name.
    fn hooks(&self) -> Result<Value, String> {
        let binary = std::env::current_exe().map_err(|e| format!("finding this driver: {e}"))?;
        let command = format!(
            "{} {HEARD} {}",
            crate::drive::quoted(text(&binary)?),
            crate::drive::quoted(text(&self.0)?)
        );
        let hook = json!([{"hooks": [{"type": "command", "command": command, "timeout": HEARD_WAITS}]}]);
        Ok(Value::Object(
            ENDS.iter()
                .map(|end| ((*end).to_owned(), hook.clone()))
                .collect(),
        ))
    }

    /// The newest turn end the session handed over since the launch, if any.
    fn end(&self) -> Option<Value> {
        std::fs::read(&self.0)
            .ok()?
            .split(|byte| *byte == b'\n')
            .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
            .next_back()
    }
}

impl Drop for Heard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// What Claude Code said went wrong, where a turn end it handed over is a `StopFailure`.
fn failed_on(end: &Value) -> Option<String> {
    (end.get("hook_event_name").and_then(Value::as_str) == Some("StopFailure"))
        .then(|| crate::hooks::failure(end).line)
        .filter(|line| !line.is_empty())
}

/// What Claude Code said went wrong, when the last assistant record of a transcript is
/// its own error (`isApiErrorMessage`) rather than the model's words, written at or after
/// `since` (milliseconds since 1970) where it says when.
pub(crate) fn stopped_on(transcript: &Path, since: u64) -> Option<String> {
    let last = crate::context::newest(transcript, |record| {
        record.get("type").and_then(Value::as_str) == Some("assistant")
    })?;
    if last.get("isApiErrorMessage").and_then(Value::as_bool) != Some(true)
        || last
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(crate::events::millis)
            .is_some_and(|at| at < since)
    {
        return None;
    }
    let text = last
        .pointer("/message/content")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    (!text.trim().is_empty()).then_some(text)
}

/// The [`HEARD`] mode, the hook a turn's own session runs as a turn ends: its document, off
/// standard input, appended as one line to the follower's file its one argument names,
/// while that file is there. It prints nothing, so Claude Code goes on as its person's own
/// hooks say, and always exits 0.
pub fn heard(mut args: impl Iterator<Item = String>) -> std::process::ExitCode {
    if let Some(file) = args.next()
        && let Ok(document) = std::io::read_to_string(std::io::stdin())
        && let Ok(document) = serde_json::from_str::<Value>(&document)
        && let Ok(mut out) = std::fs::OpenOptions::new().append(true).open(file)
    {
        let _ = out.write_all(format!("{document}\n").as_bytes());
    }
    std::process::ExitCode::SUCCESS
}

/// `turn/read`: what one line of a turn says. Standard output is the turn mode's own
/// lines: a session listed names its conversation, and the settling one says how the turn
/// ended — `stopped` after it worked is a turn interrupted. Each says whether the session's
/// transcript showed the words taken by then (`took`). Standard error is Claude Code's own
/// words for what it would not start.
pub fn read(read: TurnRead) -> TurnLine {
    let line = match read.stream {
        Stream::Stderr => {
            return TurnLine {
                refused: Some(agents::refusal_of(&read.line).to_owned()),
                ..TurnLine::default()
            };
        }
        Stream::Stdout => match serde_json::from_str::<Value>(&read.line) {
            Ok(line) => line,
            Err(_) => return TurnLine::default(),
        },
    };
    let took = line.get("took").and_then(Value::as_bool);
    if let Some(entry) = line.get("listed") {
        return TurnLine {
            named: agents::named(entry),
            took,
            ..TurnLine::default()
        };
    }
    let Some(entry) = line.get("settled") else {
        return TurnLine::default();
    };
    TurnLine {
        named: agents::named(entry),
        took,
        ended: Some(agents::end(entry).unwrap_or(Ended {
            state: TurnState::Interrupted,
            error: None,
        })),
        ..TurnLine::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::tests::Scratch;

    fn turn(act: Act, id: Option<&str>, worktree: bool) -> Turn {
        Turn {
            binary: PathBuf::from("/bin/claude"),
            act,
            id: id.map(str::to_owned),
            worktree,
            model: None,
            thinking: None,
            reads: Vec::new(),
        }
    }

    /// The words follow `--`, after every flag; what the person picked reaches Claude as
    /// its own flags.
    #[test]
    fn a_turn_says_its_act_and_claude_its_own_flags() {
        assert_eq!(
            turn(Act::Fork, Some("s-1"), true).args().unwrap()[1..],
            ["fork", "s-1", "--worktree"]
        );
        assert_eq!(
            turn(Act::Continue, Some("s-1"), true).args().unwrap()[1..],
            ["continue", "s-1"]
        );
        let picked = Turn {
            model: Some("claude-opus-5".into()),
            thinking: Some("max".into()),
            reads: vec![PathBuf::from("/prepared/7")],
            ..turn(Act::Continue, Some("s-1"), false)
        };
        let args = picked.args().unwrap();
        assert_eq!(
            args[1..],
            [
                "continue",
                "s-1",
                "--model",
                "claude-opus-5",
                "--thinking",
                "max",
                "--reads",
                "/prepared/7"
            ]
        );
        assert_eq!(Turn::parse(args.into_iter()).unwrap(), picked);
        assert_eq!(
            picked.launching(true, false).unwrap(),
            [
                "--bg",
                "--allow-dangerously-skip-permissions",
                "--resume",
                "s-1",
                "--add-dir",
                "/prepared/7",
                "--model",
                "claude-opus-5",
                "--effort",
                "max"
            ]
        );
        // A conversation Claude Code keeps a background job for is woken with its own
        // saved options: any flag beside `--resume` would start a copy.
        assert_eq!(
            picked.launching(true, true).unwrap(),
            ["--bg", "--resume", "s-1"]
        );
        assert_eq!(
            turn(Act::Fork, Some("s-1"), true)
                .launching(false, true)
                .unwrap(),
            ["--bg", "--resume", "s-1", "--fork-session", "-w"]
        );
        let new = Turn {
            reads: vec![PathBuf::from("/a/2026/09"), PathBuf::from("/a/2026/10")],
            ..turn(Act::New, None, false)
        };
        assert_eq!(
            new.launching(false, false).unwrap(),
            ["--bg", "--add-dir", "/a/2026/09", "--add-dir", "/a/2026/10"]
        );
        assert!(
            turn(Act::Continue, None, false)
                .launching(false, false)
                .is_err()
        );
        // Haiku 4.5 takes no effort: the level picked is not passed.
        let haiku = Turn {
            model: Some("claude-haiku-4-5-20251001".into()),
            thinking: Some("high".into()),
            ..turn(Act::New, None, false)
        };
        assert_eq!(
            haiku.launching(false, false).unwrap(),
            ["--bg", "--model", "claude-haiku-4-5-20251001"]
        );
    }

    /// The session's own turn ends reach the follower through the hooks its launch gives
    /// it: each document a line of a file made empty before the launch, the newest saying
    /// how the turn ended, a `StopFailure` with Claude Code's words for what went wrong.
    #[test]
    fn a_turns_own_hooks_say_its_end_and_what_went_wrong() {
        let heard = Heard::new(now()).unwrap();
        let hooks = heard.hooks().unwrap();
        let plain: Value =
            serde_json::from_str(&settings(Some(&heard), None, false).unwrap().unwrap()).unwrap();
        assert_eq!(plain, json!({ "hooks": hooks }));
        // Steery's variables go to the one session the launch starts, in its own settings.
        let given = json!({"STEERY_STARTED": "7:turn@t-1"});
        let carried: Value = serde_json::from_str(
            &settings(Some(&heard), given.as_object(), false)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(carried, json!({ "hooks": hooks, "env": given }));
        assert_eq!(settings(None, Some(&serde_json::Map::new()), false).unwrap(), None);
        for end in ENDS {
            let command = hooks[end][0]["hooks"][0]["command"].as_str().unwrap();
            assert!(
                command.ends_with(&format!(" {HEARD} '{}'", heard.0.display())),
                "{command}"
            );
        }
        assert_eq!(heard.end(), None, "nothing ended since the launch");
        let stop = json!({"session_id": "s-1", "hook_event_name": "Stop", "last_assistant_message": "done"});
        std::fs::write(&heard.0, format!("{stop}\n")).unwrap();
        assert_eq!(heard.end().as_ref().and_then(failed_on), None);
        // As Claude Code 2.1.304 handed it, the model unknown to its service.
        let failed = json!({"session_id": "s-1", "effort": {"level": "high"},
            "hook_event_name": "StopFailure", "error": "unknown",
            "last_assistant_message": "API Error: 400 unknown provider for model claude-nonexistent-9"});
        std::fs::write(&heard.0, format!("{stop}\n{failed}\n")).unwrap();
        assert_eq!(
            heard.end().as_ref().and_then(failed_on).as_deref(),
            Some("API Error: 400 unknown provider for model claude-nonexistent-9")
        );
        let path = heard.0.clone();
        drop(heard);
        assert!(!path.exists(), "the follower leaves no file behind");
    }

    #[test]
    fn a_turns_lines_name_it_and_say_where_it_ended() {
        let line = |text: Value| TurnRead {
            turn: "t".into(),
            stream: Stream::Stdout,
            line: text.to_string(),
            delivery: None,
        };
        let entry = json!({"id": "6920e989", "kind": "background", "sessionId": "s-1",
            "pid": 7, "state": "working"});
        let listed = read(line(json!({ "listed": entry })));
        assert_eq!(listed.named.as_deref(), Some("s-1"));
        assert_eq!(listed.ended, None);
        let failed = read(line(
            json!({"settled": {"kind": "background", "sessionId": "s-1", "state": "failed",
                "error": "Credit balance is too low"}}),
        ));
        assert_eq!(
            failed.ended,
            Some(Ended {
                state: TurnState::Failed,
                error: Some("Credit balance is too low".into()),
            })
        );
        let stopped = read(line(
            json!({"settled": {"kind": "background", "sessionId": "s-1", "state": "stopped"}}),
        ));
        assert_eq!(
            stopped.ended.map(|ended| ended.state),
            Some(TurnState::Interrupted)
        );
        let refused = read(TurnRead {
            turn: "t".into(),
            stream: Stream::Stderr,
            line: "Workspace not trusted. Run `claude` in /w once".into(),
            delivery: None,
        });
        assert_eq!(refused.refused.as_deref(), Some("untrusted"));
    }

    #[test]
    fn a_turns_lines_say_whether_it_took_the_words() {
        let line = |text: Value| TurnRead {
            turn: "t".into(),
            stream: Stream::Stdout,
            line: text.to_string(),
            delivery: Some("d1".into()),
        };
        let entry = json!({"id": "6920e989", "sessionId": "s-1", "state": "working"});
        assert_eq!(
            read(line(json!({"listed": entry, "took": false}))).took,
            Some(false)
        );
        assert_eq!(
            read(line(json!({"listed": entry, "took": true}))).took,
            Some(true)
        );
        assert_eq!(
            read(line(json!({"listed": entry, "took": null}))).took,
            None
        );
        assert_eq!(
            read(line(
                json!({"settled": {"sessionId": "s-1", "state": "done"}, "took": true})
            ))
            .took,
            Some(true)
        );
    }

    #[test]
    fn the_words_are_taken_once_the_transcript_holds_them() {
        assert_eq!(
            delivery("go\n\nsteery-delivery:aim.t1.2) thanks").as_deref(),
            Some("aim.t1.2")
        );
        assert_eq!(delivery("no marker"), None);
        let dir = Scratch::new("taking");
        let session = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5c";
        let project = dir.path().join("projects/-private-tmp");
        std::fs::create_dir_all(&project).unwrap();
        let transcript = project.join(format!("{session}.jsonl"));
        let user = |said: &str| {
            json!({"type":"user","isSidechain":false,"sessionId":session,"cwd":"/private/tmp",
                "message":{"role":"user","content":said}})
            .to_string()
        };
        std::fs::write(&transcript, format!("{}\n", user("earlier words"))).unwrap();
        let mut taking = Taking::before("do it steery-delivery:d7", dir.path(), Some(session));
        let entry = json!({"sessionId": session});
        assert!(!taking.look(dir.path(), &entry));
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        writeln!(file, "{}", user("do it steery-delivery:d7")).unwrap();
        assert!(taking.look(dir.path(), &entry));
        assert!(taking.took);
        assert!(!taking.look(dir.path(), &entry), "taken is said once");
    }

    /// A `continue` into a conversation a background session holds never starts a turn
    /// beside it: refused before anything starts. And where Claude Code started a copy
    /// anyway, the copy is stopped and removed and the turn refused the same way. A
    /// conversation whose background job Claude Code keeps, stopped, is woken with
    /// nothing beside `--resume`, and a pick the job would not run is refused first.
    #[tokio::test]
    async fn a_continue_never_starts_beside_a_live_session_and_discards_a_copy() {
        let dir = Scratch::new("beside");
        let session = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
        let binary = dir.path().join("claude");
        let script = |listed: &str, launched: &str| {
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  agents) printf '%s\\n' '{listed}' ;;\n  --bg) echo \"$*\" >> '{d}/launched'; printf '%s\\n' '{launched}' ;;\n  stop|rm) echo \"$1 $2\" >> '{d}/discarded' ;;\nesac\n",
                d = dir.path().display()
            )
        };
        let held = format!(
            r#"[{{"id":"6b44d2be","kind":"background","sessionId":"{session}","pid":4242,"state":"blocked","status":"idle"}}]"#
        );
        crate::tests::stand_in(&binary, &script(&held, "backgrounded · 186b32ef"));
        let continued = Turn {
            binary: binary.clone(),
            ..turn(Act::Continue, Some(session), false)
        };
        let mut out = Vec::new();
        let error = follow(&continued, dir.path(), false, "go", &Default::default(), &mut out)
            .await
            .expect_err("a live session holds it");
        assert_eq!(agents::refusal_of(&error), "harness-busy");
        assert!(
            !dir.path().join("launched").exists(),
            "nothing started beside it"
        );

        let copy = format!(
            "note: session {session} is already running in the background, so this started a copy as 186b32ef.\nbackgrounded · 186b32ef"
        );
        crate::tests::stand_in(&binary, &script("[]", &copy));
        let error = follow(&continued, dir.path(), false, "go", &Default::default(), &mut out)
            .await
            .expect_err("a copy is no continue");
        assert_eq!(agents::refusal_of(&error), "harness-busy");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("discarded")).unwrap(),
            "stop 186b32ef\nrm 186b32ef\n",
            "the copy is stopped and removed"
        );
        assert!(out.is_empty());

        let job = dir.path().join("jobs/0199a1b2");
        std::fs::create_dir_all(&job).unwrap();
        std::fs::write(
            job.join("state.json"),
            json!({
                "sessionId": session,
                "respawnFlags": ["--settings", "{\"env\":{}}", "--model", "haiku", "--effort", "high"],
                "providerEnv": {"ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-haiku-5-5"},
            })
            .to_string(),
        )
        .unwrap();
        let woken = format!(
            r#"[{{"id":"0199a1b2","kind":"background","sessionId":"{session}","state":"done","startedAt":{}}}]"#,
            u64::MAX / 2
        );
        crate::tests::stand_in(&binary, &script(&woken, "backgrounded · 0199a1b2"));
        let launches = || std::fs::read_to_string(dir.path().join("launched")).unwrap();
        let before = launches();
        for (model, thinking) in [("sonnet", "high"), ("claude-haiku-5-5", "max")] {
            let other = Turn {
                model: Some(model.into()),
                thinking: Some(thinking.into()),
                ..continued.clone()
            };
            let error = follow(&other, dir.path(), false, "go", &Default::default(), &mut out)
                .await
                .expect_err("the job runs its own options");
            assert_eq!(agents::refusal_of(&error), "selection-unsupported", "{error}");
        }
        assert_eq!(launches(), before, "nothing started for another pick");
        let same = Turn {
            model: Some("claude-haiku-5-5".into()),
            thinking: Some("high".into()),
            ..continued.clone()
        };
        follow(&same, dir.path(), false, "go", &Default::default(), &mut out)
            .await
            .expect("the kept job is woken and followed");
        assert_eq!(
            launches().lines().last(),
            Some(format!("--bg --resume {session} -- go").as_str())
        );
        let last: Value =
            serde_json::from_str(String::from_utf8(out).unwrap().lines().last().unwrap()).unwrap();
        assert_eq!(last["settled"]["state"], "done", "{last}");
    }

    /// Claude Code replacing itself mid-turn leaves no `claude` to list its sessions for a
    /// while, and the session goes on: the follower reads past that stretch to the turn's
    /// own end instead of reading the turn as never started.
    #[tokio::test]
    async fn a_turn_is_followed_through_claude_code_replacing_itself() {
        let dir = Scratch::new("updating");
        let binary = dir.path().join("claude");
        let entry = |state: &str| {
            format!(
                r#"[{{"id":"186b32ef","kind":"background","sessionId":"s-1","pid":7,"startedAt":{},"state":"{state}","status":"busy"}}]"#,
                u64::MAX / 2
            )
        };
        crate::tests::stand_in(
            &binary,
            &format!(
                "#!/bin/sh\nd='{d}'\ncase \"$1\" in\n  --bg) echo 'backgrounded · 186b32ef' ;;\n  agents)\n    n=$(( $(cat \"$d/n\" 2>/dev/null || echo 0) + 1 )); echo $n > \"$d/n\"\n    case $n in\n      1) printf '%s\\n' '{working}' ;;\n      2) mv \"$0\" \"$0.away\"; exit 1 ;;\n      *) printf '%s\\n' '{done}' ;;\n    esac ;;\nesac\n",
                d = dir.path().display(),
                working = entry("working"),
                done = entry("done"),
            ),
        );
        let away = dir.path().join("claude.away");
        let restore = {
            let (away, binary) = (away.clone(), binary.clone());
            tokio::spawn(async move {
                while !away.exists() {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                tokio::time::sleep(Duration::from_millis(2500)).await;
                std::fs::rename(&away, &binary).unwrap();
            })
        };
        let new = Turn {
            binary: binary.clone(),
            ..turn(Act::New, None, false)
        };
        let mut out = Vec::new();
        follow(&new, dir.path(), false, "go", &Default::default(), &mut out)
            .await
            .expect("the turn is followed to its end");
        restore.await.unwrap();
        let last: Value =
            serde_json::from_str(String::from_utf8(out).unwrap().lines().last().unwrap()).unwrap();
        assert_eq!(last["settled"]["state"], "done", "{last}");
        let listings: u32 = std::fs::read_to_string(dir.path().join("n"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(listings >= 3, "it listed again once claude was back");
    }
}
