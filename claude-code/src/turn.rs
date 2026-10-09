//! One turn of Claude Code, run by this driver's own binary in its turn mode, and its
//! lines read back.
//!
//! `steery-driver-claude-code turn <claude> <act> [<id>] [--worktree] [--model <model>]
//! [--thinking <effort>] [--reads <folder>]…` takes the words on standard input, starts a
//! background session with `claude --bg … -- <words>` (the words follow `--`, which Claude
//! reads as the words and never as a flag), and follows it through `claude agents --json`
//! to where its turn stopped. Every change in how the session is listed goes to standard
//! output as `{"listed": <entry>}`, and the entry that settles the turn as `{"settled":
//! <entry>}`; what Claude Code refused, in its own words, goes to standard error, and the
//! mode exits 1.
//!
//! A `continue` never starts beside a session that is in the conversation: Claude Code
//! would start a copy. One already there is refused before anything starts, and a copy
//! Claude Code started anyway, in the moment between that look and the start, is stopped
//! and removed at once, and the turn refused the same way. Claude refuses a background
//! session in a folder the person has not trusted in Claude, as it would at their own
//! terminal.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::wire::harness::{Act, Ended, Stream, TurnLine, TurnRead, TurnState};

use crate::agents::{self, Follow, RUNNING};

/// The levels `claude --effort` takes, least first (`claude --help`).
pub const EFFORT: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The word that starts the turn mode, as `start`'s plan runs this binary.
pub const MODE: &str = "turn";

/// How often a background session is read back while its turn runs. Reading it costs
/// Claude Code about a second.
const FOLLOW_EVERY: Duration = Duration::from_secs(1);

/// How long a session `claude --bg` started has to be listed.
const LISTED_WITHIN: Duration = Duration::from_secs(90);

/// How much of a transcript's end is read for the last thing said in it.
const LAST_SAID: u64 = 1024 * 1024;

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
    /// Claude Code asks its person before a read anywhere else.
    fn launching(&self) -> Result<Vec<String>, String> {
        let mut args: Vec<String> = vec!["--bg".into()];
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
        if let Some(thinking) = &self.thinking {
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

/// The turn mode: run the turn its arguments say, the words on standard input. Exits 0
/// once the turn settled, 1 with Claude Code's words on standard error where it did not.
pub fn run(args: impl Iterator<Item = String>) -> std::process::ExitCode {
    let ran = (|| {
        let turn = Turn::parse(args)?;
        let words = std::io::read_to_string(std::io::stdin())
            .map_err(|e| format!("reading the words: {e}"))?;
        let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
        let root = crate::root(Path::new(&home))?;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?
            .block_on(follow(&turn, &root, &words, &mut std::io::stdout()))
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
/// stopped, writing each change to `out`.
async fn follow(turn: &Turn, root: &Path, words: &str, out: &mut impl Write) -> Result<(), String> {
    let registry = crate::registry::dir(root);
    if turn.act == Act::Continue
        && let Some(id) = turn.id.as_deref()
        && running_now(&turn.binary, &registry, id).await?
    {
        return Err(format!("session {id} {}", RUNNING[0]));
    }
    let launched = now();
    // The session writes the conversation: it keeps the `STEERY_STARTED` the runtime gave
    // this turn, never a helper's.
    let mut command = tokio::process::Command::new(&turn.binary);
    command.stdin(std::process::Stdio::null());
    let launch = command
        .args(turn.launching()?)
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
    if turn.act == Act::Continue && RUNNING.iter().any(|running| said.contains(running)) {
        if let Some(copy) = agents::backgrounded(&said) {
            discard(&turn.binary, &copy).await;
        }
        return Err(format!(
            "session {} {}: the copy Claude Code started beside it was stopped and removed",
            turn.id.as_deref().unwrap_or_default(),
            RUNNING[1]
        ));
    }
    let short = match (launch.status.success(), agents::backgrounded(&said)) {
        (true, Some(short)) => short,
        _ => return Err(said.trim().to_owned()),
    };
    let listed_by = tokio::time::Instant::now() + LISTED_WITHIN;
    let mut last: Option<Value> = None;
    let mut follow = Follow::default();
    loop {
        let listed = agents::agents(&turn.binary).await?;
        match listed
            .into_iter()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(&short))
        {
            Some(mut entry) => {
                // The conversation's transcript, read only where the listing leaves a
                // question: what went wrong, and whether this turn's end is recorded.
                let session = entry
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let transcript = || {
                    crate::transcripts::Store::new(root.to_owned())
                        .session(session.as_deref()?)
                        .ok()?
                        .transcripts
                        .pop()
                };
                let short_of_done = agents::end(&entry).is_some()
                    && entry.get("state").and_then(Value::as_str) != Some("done");
                if short_of_done
                    && let Some(error) = transcript().and_then(|at| stopped_on(&at))
                    && let Some(object) = entry.as_object_mut()
                {
                    object.insert("error".into(), Value::from(error));
                }
                let ended = || transcript().is_some_and(|at| turn_ended_since(&at, launched));
                if follow.settled_by(&entry, ended) {
                    return say(out, json!({ "settled": entry }));
                }
                if last.as_ref() != Some(&entry) {
                    say(out, json!({ "listed": entry }))?;
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

/// Now, in milliseconds since 1970.
fn now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or_default()
}

/// Whether a process is in the conversation now: the registry names a live one, or
/// `claude agents` lists a session in it with a process behind it. A session stopped
/// keeps its row with no process, and holds nothing.
async fn running_now(binary: &Path, registry: &Path, id: &str) -> Result<bool, String> {
    if crate::registry::live(registry, id).is_some() {
        return Ok(true);
    }
    Ok(agents::agents(binary).await?.iter().any(|entry| {
        entry.get("sessionId").and_then(Value::as_str) == Some(id)
            && entry.get("pid").and_then(Value::as_u64).is_some()
    }))
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

/// The complete records of a transcript's last `most` bytes.
fn tail(transcript: &Path, most: u64) -> Option<Vec<Value>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(transcript).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(most);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let whole = if start > 0 {
        &bytes[bytes.iter().position(|byte| *byte == b'\n')? + 1..]
    } else {
        &bytes[..]
    };
    Some(
        whole
            .split(|byte| *byte == b'\n')
            .filter_map(|line| serde_json::from_slice(line).ok())
            .collect(),
    )
}

/// Whether Claude Code recorded the end of a turn in `transcript` after `since`
/// (milliseconds): the `system` `turn_duration` record it writes as each turn ends.
fn turn_ended_since(transcript: &Path, since: u64) -> bool {
    tail(transcript, LAST_SAID).is_some_and(|tail| {
        tail.iter()
            .filter(|record| {
                record.get("type").and_then(Value::as_str) == Some("system")
                    && record.get("subtype").and_then(Value::as_str) == Some("turn_duration")
            })
            .filter_map(|record| {
                let at = record.get("timestamp").and_then(Value::as_str)?;
                chrono::DateTime::parse_from_rfc3339(at).ok()
            })
            .any(|at| u64::try_from(at.timestamp_millis()).is_ok_and(|at| at > since))
    })
}

/// What Claude Code said went wrong, when the last assistant record of a transcript is
/// its own error (`isApiErrorMessage`) rather than the model's words.
fn stopped_on(transcript: &Path) -> Option<String> {
    let tail = tail(transcript, LAST_SAID)?;
    let last = tail
        .iter()
        .rev()
        .find(|record| record.get("type").and_then(Value::as_str) == Some("assistant"))?;
    if last.get("isApiErrorMessage").and_then(Value::as_bool) != Some(true) {
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

/// `turn/read`: what one line of a turn says. Standard output is the turn mode's own
/// lines: a session listed names its conversation, and the settling one says how the turn
/// ended — `stopped` after it worked is a turn interrupted. Standard error is Claude
/// Code's own words for what it would not start.
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
    if let Some(entry) = line.get("listed") {
        return TurnLine {
            named: agents::named(entry),
            ..TurnLine::default()
        };
    }
    let Some(entry) = line.get("settled") else {
        return TurnLine::default();
    };
    TurnLine {
        named: agents::named(entry),
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
            picked.launching().unwrap(),
            [
                "--bg",
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
        assert_eq!(
            turn(Act::Fork, Some("s-1"), true).launching().unwrap(),
            ["--bg", "--resume", "s-1", "--fork-session", "-w"]
        );
        let new = Turn {
            reads: vec![PathBuf::from("/a/2026/09"), PathBuf::from("/a/2026/10")],
            ..turn(Act::New, None, false)
        };
        assert_eq!(
            new.launching().unwrap(),
            ["--bg", "--add-dir", "/a/2026/09", "--add-dir", "/a/2026/10"]
        );
        assert!(turn(Act::Continue, None, false).launching().is_err());
    }

    #[test]
    fn a_background_session_stopped_on_its_own_error() {
        let dir = Scratch::new("blocked");
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, concat!(
            r#"{"type":"assistant","message":{"model":"claude-sonnet-5","content":[{"type":"text","text":"working"}]}}"#, "\n",
            r#"{"type":"assistant","isApiErrorMessage":true,"message":{"model":"<synthetic>","content":[{"type":"text","text":"Autocompact is thrashing: the context refilled"}]}}"#, "\n",
            r#"{"type":"system","subtype":"turn_duration"}"#, "\n",
        )).unwrap();
        assert_eq!(
            stopped_on(&transcript).as_deref(),
            Some("Autocompact is thrashing: the context refilled")
        );
        std::fs::write(&transcript, concat!(
            r#"{"type":"assistant","message":{"model":"claude-sonnet-5","content":[{"type":"text","text":"May I run it?"}]}}"#, "\n",
        )).unwrap();
        assert_eq!(
            stopped_on(&transcript),
            None,
            "the model's own last word is no error"
        );
    }

    /// Claude Code writes `system` `turn_duration` as each turn ends. Only one written
    /// after the launch is this turn's.
    #[test]
    fn a_transcript_says_whether_a_turn_ended_since_the_launch() {
        let dir = Scratch::new("ended");
        let transcript = dir.path().join("t.jsonl");
        std::fs::write(&transcript, concat!(
            r#"{"type":"user","timestamp":"2026-09-27T20:43:44.000Z","entrypoint":"cli","message":{"role":"user","content":"go"}}"#, "\n",
            r#"{"type":"assistant","timestamp":"2026-09-27T20:47:30.100Z","message":{"stop_reason":"stop_sequence","content":[{"type":"text","text":"Which one?"}]}}"#, "\n",
            r#"{"type":"system","subtype":"turn_duration","timestamp":"2026-09-27T20:47:30.200Z"}"#, "\n",
            r#"{"type":"frame-link"}"#, "\n",
        )).unwrap();
        let launch = |at: &str| {
            u64::try_from(
                chrono::DateTime::parse_from_rfc3339(at)
                    .unwrap()
                    .timestamp_millis(),
            )
            .unwrap()
        };
        assert!(turn_ended_since(
            &transcript,
            launch("2026-09-27T20:38:55.515Z")
        ));
        assert!(
            !turn_ended_since(&transcript, launch("2026-09-27T21:07:00.000Z")),
            "an earlier turn's end"
        );
        assert!(!turn_ended_since(
            &dir.path().join("absent.jsonl"),
            launch("2026-09-27T20:38:55Z")
        ));
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

    /// A `continue` into a conversation a background session holds never starts a turn
    /// beside it: refused before anything starts. And where Claude Code started a copy
    /// anyway, the copy is stopped and removed and the turn refused the same way.
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
        let error = follow(&continued, dir.path(), "go", &mut out)
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
        let error = follow(&continued, dir.path(), "go", &mut out)
            .await
            .expect_err("a copy is no continue");
        assert_eq!(agents::refusal_of(&error), "harness-busy");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("discarded")).unwrap(),
            "stop 186b32ef\nrm 186b32ef\n",
            "the copy is stopped and removed"
        );
        assert!(out.is_empty());
    }
}
