//! Grok Build's own way in, from its command line, its registry and its files: who holds a
//! conversation, what its last turn did, how a turn starts and stops, how a person reopens
//! it, and its one-shot reader.
//!
//! A turn is `grok --prompt-file /dev/stdin --output-format streaming-messages-json`, run
//! by the runtime itself: the words on standard input, one Claude Code stream-json record
//! on each line of standard output ([`super::turn`]). Grok files such a run as
//! `session_kind: headless`, which `sessions/list` excludes and every read by id serves.
//!
//! The turn runs with `--always-approve`: a headless Grok cancels every call its rules do
//! not allow, and a hook's `allow` does not get past that ask. So Steery's `PreToolUse`
//! hook and the person's `deny` rules are its gates, and `start` refuses without the hook.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use steery_sdk::wire::harness::{
    Act, Beyond, Event, Execution, Held, HeldBy, Holder, HolderStop, Launch, Look, Looked, Marked,
    ReadAs, Reader, ReaderPlan, ReaderTools, Receipt, ReceiptFind, ReceiptMark, Sample, Sampled,
    Start, Stdin, Stop, Stopped, Terminal, TerminalCommand, TurnState,
};
use steery_sdk::{Error, refusal};

use super::{events, models, store::Store};

/// How a Grok TUI in a terminal holds its conversation, as the line that asks its person
/// to let it be stopped says it.
const IN_A_TERMINAL: &str = "in a terminal";

/// Grok's levels of `--reasoning-effort`, as its own `ReasoningEffort` spells them (1.0.77).
pub const EFFORT: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The argument of Grok's leader process, which serves every session that uses it.
const LEADER: &str = "--leader";

/// How much of a transcript's end `idle` reads for its last turn.
const IDLE_TAIL: u64 = 512 * 1024;

/// `start`: the plan of a turn that the runtime runs as Grok itself, the words on standard
/// input. A continue or a fork names a conversation Grok keeps here; a model is one that
/// `grok models` lists; a thinking pick is one of Grok's efforts.
pub async fn start(root: &Path, store: &Store, start: Start) -> Result<Launch, Error> {
    let binary = super::binary(root)
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "grok is not installed here"))?;
    if !super::setup::gated(root) {
        return Err(Error::refused(
            refusal::REFUSED,
            format!(
                "Steery's PreToolUse hook is not in {}, and Grok would run every call of the \
                 turn unasked. Set Grok Build up again",
                super::setup::hooks_file(root).display()
            ),
        ));
    }
    let resume = match (start.act, start.external_id.as_deref()) {
        (Act::Continue | Act::Fork, Some(id)) => {
            store.find(id)?;
            Some(id.to_owned())
        }
        (Act::Continue | Act::Fork, None) => {
            return Err(Error::invalid(
                "a continue or a fork must name its conversation",
            ));
        }
        (Act::New, _) => None,
        (Act::Hand, _) => {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "Grok Build does not run a hand over as a turn",
            ));
        }
    };
    if start.worktree {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Steery does not start Grok Build in a worktree of its own",
        ));
    }
    effort(start.thinking.as_deref())?;
    if let Some(model) = &start.model {
        let listed = models::list(root).await.map_err(|_| {
            Error::refused(
                refusal::MODEL_MISSING,
                format!("Grok Build listed no models here, so it cannot run {model}"),
            )
        })?;
        if !listed.models.iter().any(|listed| &listed.model == model) {
            return Err(Error::refused(
                refusal::MODEL_MISSING,
                format!("Grok Build does not run {model} here"),
            ));
        }
    }
    let mut argv = vec![
        text(&binary)?,
        "--prompt-file".into(),
        "/dev/stdin".into(),
        "--output-format".into(),
        "streaming-messages-json".into(),
        "--always-approve".into(),
    ];
    if let Some(id) = resume {
        argv.extend(["-r".into(), id]);
        if start.act == Act::Fork {
            argv.push("--fork-session".into());
        }
    }
    if let Some(model) = start.model {
        argv.extend(["-m".into(), model]);
    }
    if let Some(thinking) = start.thinking {
        argv.extend(["--reasoning-effort".into(), thinking]);
    }
    for read in &start.reads {
        argv.extend(["--add-dir".into(), text(read)?]);
    }
    Ok(Launch {
        argv,
        env: BTreeMap::new(),
        cwd: start.cwd,
        stdin: Stdin::Words,
        inbox: false,
    })
}

/// A thinking pick Grok takes: one of its efforts.
fn effort(thinking: Option<&str>) -> Result<(), Error> {
    match thinking {
        Some(thinking) if !EFFORT.contains(&thinking) => Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("Grok Build has no reasoning effort {thinking}"),
        )),
        _ => Ok(()),
    }
}

fn text(path: &Path) -> Result<String, Error> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::invalid(format!("{} is not text", path.display())))
}

/// `held`: the Grok TUI that `<root>/active_sessions.json` names for the conversation,
/// where its process still runs; else where the runtime looks. A headless turn registers
/// nowhere, and Grok's leader process serves every session that uses it.
pub fn held(root: &Path, store: &Store, held: Held) -> HeldBy {
    if let Some((pid, process_start)) = registered(root, &held.external_id) {
        return HeldBy::Holder(Holder::Person {
            door: None,
            stop: HolderStop::Process {
                pid,
                process_start,
                certain: true,
                how: IN_A_TERMINAL.into(),
            },
        });
    }
    let transcripts = store
        .find(&held.external_id)
        .map(|found| vec![found.transcript, found.history])
        .unwrap_or_default();
    HeldBy::Look(Looked {
        look: Look {
            transcripts,
            strays: true,
            beyond: vec![Beyond {
                arg: Some(LEADER.into()),
                line: "Grok's leader process holds it, and it serves every Grok session that \
                       uses it."
                    .into(),
            }],
        },
    })
}

/// The live process that Grok's registry names for `id`, and when it started.
pub(super) fn registered(root: &Path, id: &str) -> Option<(u32, u64)> {
    let text = std::fs::read(root.join("active_sessions.json")).ok()?;
    let entries: Value = serde_json::from_slice(&text).ok()?;
    entries
        .as_array()?
        .iter()
        .filter(|entry| entry["session_id"].as_str() == Some(id))
        .filter_map(|entry| u32::try_from(entry["pid"].as_u64()?).ok())
        .find_map(|pid| Some((pid, kernel::started(pid)?)))
}

/// `idle`: the last turn in the transcript where it ended; nothing where it did not, as a
/// run that died leaves a turn open forever.
pub fn idle(store: &Store, held: Held) -> Sampled {
    Sampled {
        sample: store
            .find(&held.external_id)
            .ok()
            .and_then(|found| {
                events::read(
                    &found.transcript,
                    None,
                    Some(IDLE_TAIL),
                    found.parent.as_deref(),
                )
                .ok()
            })
            .and_then(|read| sample(&read.events)),
    }
}

/// The sample of the last turn in `events`, where it ended.
fn sample(events: &[Event]) -> Option<Sample> {
    let mut model = None;
    let mut started_at = None;
    let mut ended = None;
    for event in events {
        match event {
            Event::Model { model: named, .. } => model = Some(named.clone()),
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
    let (ended_at, state, error) = ended?;
    Some(Sample {
        state: match state {
            TurnState::Finished => Execution::Finished,
            TurnState::Failed => Execution::Failed,
            TurnState::Interrupted => Execution::Interrupted,
        },
        observed_at: now(),
        started_at,
        ended_at: Some(ended_at),
        model,
        thinking: None,
        error,
        vacant: false,
        if_unheld: true,
    })
}

/// Milliseconds since 1970.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `stop`: Grok has no graceful stop of its own for a headless turn or a TUI; the
/// runtime's signals end it. A stop of Steery's own words cannot tell its turn.
pub fn stop(stop: Stop) -> Result<Stopped, Error> {
    if stop.say.is_some() {
        return Err(Error::refused(
            refusal::OTHER_TURN,
            "Grok Build cannot tell which words began the turn that runs",
        ));
    }
    Ok(Stopped {
        stopped: false,
        proof: Some(
            "A headless Grok turn has no graceful stop of its own; the runtime's signals end it."
                .into(),
        ),
    })
}

/// `receipt/mark`: no door, so no point to look after.
pub fn mark(_: ReceiptMark) -> Result<Marked, Error> {
    Err(Error::refused(
        refusal::SELECTION_UNSUPPORTED,
        "Grok Build has no door whose words need a receipt",
    ))
}

/// `receipt/find`: never asked, as `mark` gives no mark.
pub fn find(_: ReceiptFind) -> Result<Receipt, Error> {
    Err(Error::refused(
        refusal::REFUSED,
        "Grok Build gives no mark to look after",
    ))
}

/// `terminal`: `cd '<cwd>' && grok --leader --resume <id>`, with `GROK_HOME='<root>'` before
/// grok where Grok's home is not its default. `--leader` runs the window through Grok's
/// leader whatever `[cli] use_leader` says, so Steery's door reaches it.
pub fn terminal(
    home: &Path,
    root: &Path,
    store: &Store,
    terminal: Terminal,
) -> Result<TerminalCommand, Error> {
    let id = &terminal.external_id;
    store.find(id)?;
    if !plain_id(id) {
        return Err(Error::refused(
            refusal::NOT_FOUND,
            format!("Grok keeps no conversation `{id}` here"),
        ));
    }
    if !terminal.cwd.is_absolute() {
        return Err(Error::invalid("the folder is not an absolute path"));
    }
    let mut command = format!("cd {} && ", quoted(&text(&terminal.cwd)?));
    if root != home.join(".grok") {
        command.push_str(&format!("GROK_HOME={} ", quoted(&text(root)?)));
    }
    command.push_str(&format!("grok --leader --resume {id}"));
    let line = match &terminal.holder {
        Some(Holder::Person {
            stop: HolderStop::Process { pid, .. },
            ..
        }) => Some(format!(
            "Grok has it open already (pid {pid}). This command opens a second session on it."
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

/// Whether an id is one plain word that a command takes as it is, and not a flag.
fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The tools of a reader that looks (`grok --help`'s own example of read-only tools).
const READ_TOOLS: &str = "read_file,grep,list_dir";

/// `reader`: `grok --prompt-file /dev/stdin --output-format plain`, the words on standard
/// input, on the model and effort given: `--tools ""` allows no built-in tool (or only
/// [`READ_TOOLS`]), `--no-subagents` and `--disable-web-search` leave nothing to reach
/// beyond them, `--system-prompt-override` says the instructions. Grok has no flag that
/// keeps the run unsaved: it files it as a headless run, which `sessions/list` excludes.
/// The answer is the whole output.
pub fn reader(root: &Path, reader: Reader) -> Result<ReaderPlan, Error> {
    let binary = super::binary(root)
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "grok is not installed here"))?;
    if !reader.cwd.is_absolute() {
        return Err(Error::invalid(
            "the reader's folder is not an absolute path",
        ));
    }
    effort(reader.thinking.as_deref())?;
    let mut argv = vec![
        text(&binary)?,
        "--prompt-file".into(),
        "/dev/stdin".into(),
        "--output-format".into(),
        "plain".into(),
        "--no-subagents".into(),
        "--disable-web-search".into(),
    ];
    if let Some(model) = reader.model {
        argv.extend(["-m".into(), model]);
    }
    if let Some(thinking) = reader.thinking {
        argv.extend(["--reasoning-effort".into(), thinking]);
    }
    if let Some(system) = reader.system {
        argv.extend(["--system-prompt-override".into(), system]);
    }
    argv.extend([
        "--tools".into(),
        match reader.tools {
            ReaderTools::None => String::new(),
            ReaderTools::Read => READ_TOOLS.into(),
        },
    ]);
    Ok(ReaderPlan {
        argv,
        env: BTreeMap::new(),
        cwd: reader.cwd,
        stdin: Stdin::Words,
        files: Vec::new(),
        read: ReadAs::Whole,
    })
}

/// What this driver reads of other processes from the kernel: when one started, in
/// milliseconds since 1970, as the operating-system adapters give it. The crate links no
/// `libc`, so the one call it needs is declared here.
mod kernel {
    /// When the live process under `pid` started; `None` where there is none, or only a
    /// zombie waiting to be reaped.
    #[cfg(target_os = "macos")]
    pub fn started(pid: u32) -> Option<u64> {
        use std::os::raw::{c_int, c_void};
        /// `struct proc_bsdinfo` of `<sys/proc_info.h>`.
        #[repr(C)]
        struct BsdInfo {
            flags: u32,
            status: u32,
            xstatus: u32,
            pid: u32,
            ppid: u32,
            ids: [u32; 6],
            rfu: u32,
            comm: [u8; 16],
            name: [u8; 32],
            nfiles: u32,
            pgid: u32,
            pjobc: u32,
            tdev: u32,
            tpgid: u32,
            nice: i32,
            start_tvsec: u64,
            start_tvusec: u64,
        }
        unsafe extern "C" {
            fn proc_pidinfo(
                pid: c_int,
                flavor: c_int,
                arg: u64,
                buffer: *mut c_void,
                size: c_int,
            ) -> c_int;
        }
        const PROC_PIDTBSDINFO: c_int = 3;
        const SZOMB: u32 = 5;
        let pid = c_int::try_from(pid).ok().filter(|pid| *pid > 0)?;
        // SAFETY: an all-zero `BsdInfo` is a valid value of a plain C struct.
        let mut info: BsdInfo = unsafe { std::mem::zeroed() };
        let size = c_int::try_from(std::mem::size_of::<BsdInfo>()).ok()?;
        // SAFETY: the buffer is one `BsdInfo`, and `size` is its size.
        let read = unsafe { proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
        if read != size || info.status == SZOMB {
            return None;
        }
        Some(info.start_tvsec * 1000 + info.start_tvusec / 1000)
    }

    /// When the live process under `pid` started; `None` where there is none, or only a
    /// zombie waiting to be reaped. Fields after the last `)` of `/proc/<pid>/stat`: the
    /// command name may hold spaces and parentheses.
    #[cfg(not(target_os = "macos"))]
    pub fn started(pid: u32) -> Option<u64> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &text[text.rfind(')')? + 1..];
        let mut fields = rest.split_whitespace();
        if matches!(fields.next()?.chars().next()?, 'Z' | 'X' | 'x') {
            return None;
        }
        let ticks: u64 = fields.nth(18)?.parse().ok()?;
        let boot: u64 = std::fs::read_to_string("/proc/stat")
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .trim()
            .parse()
            .ok()?;
        // Linux has counted `/proc` times in 100 ticks a second (`USER_HZ`) on every
        // architecture it runs on since 2.6.
        Some(boot * 1000 + ticks * 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_has_a_start() {
        let started = kernel::started(std::process::id()).expect("this process runs");
        assert!(started <= now() && started > now() - 3_600_000);
    }

    #[test]
    fn a_stop_with_a_say_is_another_turn() {
        let holder = Holder::Nobody { proof: "x".into() };
        let stop = |say: Option<&str>| {
            super::stop(Stop {
                external_id: "s".into(),
                holder: holder.clone(),
                say: say.map(str::to_owned),
            })
        };
        assert_eq!(word(stop(Some("say-1")).unwrap_err()), refusal::OTHER_TURN);
        assert!(!stop(None).unwrap().stopped);
    }

    #[test]
    fn efforts_are_grok_s_own() {
        assert!(effort(Some("high")).is_ok());
        assert!(effort(None).is_ok());
        assert_eq!(
            word(effort(Some("ultra")).unwrap_err()),
            refusal::SELECTION_UNSUPPORTED
        );
    }

    fn word(error: Error) -> String {
        match error {
            Error::Refused { word, .. } => word,
            other => panic!("not a refusal: {other}"),
        }
    }

    #[test]
    fn a_word_is_quoted_whole() {
        assert_eq!(quoted("/tmp/it's"), r"'/tmp/it'\''s'");
        assert!(plain_id("01a1c56b-5ea1-7c72-89f2-ebe06b634618"));
        assert!(!plain_id("--leader"));
    }
}
