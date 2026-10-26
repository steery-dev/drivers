//! Claude Code's registry of the sessions somebody is in, read from its own files.
//!
//! Every session with messaging on writes `<pid>.json` under `sessions/` in its
//! configuration root — its session id, its working directory, its `messagingSocketPath`,
//! `kind` (`bg` for a session in Claude Code's own background service, `interactive` for
//! one in a terminal), the background session's `jobId` and `startedAt` — and the peer
//! token beside it in `<pid>.<hash>.key`. A session is in a process exactly when its
//! registry entry names a process that is alive and started no later than the session.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A session as its registry entry names it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Registered {
    pub pid: i32,
    pub session_id: String,
    #[serde(default)]
    pub messaging_socket_path: Option<String>,
    /// `bg` for a session in Claude Code's own background service, `interactive` for one
    /// in a terminal.
    #[serde(default)]
    pub kind: Option<String>,
    /// The short id `claude attach` and `claude stop` take for a background session.
    #[serde(default)]
    pub job_id: Option<String>,
    /// When the session started, in milliseconds: the process in it had started by then.
    #[serde(default)]
    pub started_at: Option<i64>,
    /// How Claude Code was started: `sdk-cli` where a program runs it.
    #[serde(default)]
    pub entrypoint: Option<String>,
    /// Where the session stands: `busy`, `waiting` on its person, or `idle`.
    #[serde(default)]
    pub status: Option<String>,
    /// What a `waiting` session waits on: `permission prompt`, `input needed`, `sandbox
    /// request`, `worker request` or `dialog open`.
    #[serde(default)]
    pub waiting_for: Option<String>,
    /// When `status` last changed, in milliseconds.
    #[serde(default)]
    pub status_updated_at: Option<u64>,
}

/// One session somebody is in right now: a registry entry whose process is alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    pub pid: i32,
    pub socket: Option<PathBuf>,
    /// The short id of the background session it is, when it is one.
    pub job: Option<String>,
    /// Where it stands: `busy`, `waiting` or `idle`, where it says.
    pub status: Option<String>,
}

/// Where the registry is: `sessions/` under Claude Code's configuration root.
pub fn dir(root: &Path) -> PathBuf {
    root.join("sessions")
}

/// Every entry the registry at `dir` holds that reads as one.
fn entries(dir: &Path) -> impl Iterator<Item = Registered> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension()? == "json").then_some(())?;
            serde_json::from_slice(&std::fs::read(&path).ok()?).ok()
        })
}

/// The registry entry for one session, when its process is alive.
pub fn live(dir: &Path, external_id: &str) -> Option<Live> {
    live_in(dir, external_id, alive)
}

/// Every registry entry whose process is alive.
pub fn living(dir: &Path) -> impl Iterator<Item = Registered> {
    entries(dir).filter(alive)
}

/// [`live`], with `alive` answering for an entry — the kernel, or a test's own answer.
fn live_in(dir: &Path, external_id: &str, alive: impl Fn(&Registered) -> bool) -> Option<Live> {
    entries(dir)
        .find(|registered| registered.session_id == external_id && alive(registered))
        .map(|registered| Live {
            pid: registered.pid,
            socket: registered.messaging_socket_path.map(PathBuf::from),
            job: (registered.kind.as_deref() == Some("bg"))
                .then_some(registered.job_id)
                .flatten(),
            status: registered.status,
        })
}

/// The token a session published beside its registry entry, for its inbox's auth line.
pub fn peer_token(dir: &Path, pid: i32) -> Option<String> {
    let prefix = format!("{pid}.");
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".key") {
            continue;
        }
        let value: Value = serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
        return value
            .get("peerToken")
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    None
}

/// Whether the process an entry names is still the one in its session: it exists and is
/// this user's to signal, and it started no later than the session did. An entry Claude
/// Code left behind names a pid another process may have taken since, and that process
/// is in no conversation.
fn alive(registered: &Registered) -> bool {
    let Ok(pid) = u32::try_from(registered.pid) else {
        return false;
    };
    if pid == 0 || !matches!(kernel::exists(registered.pid), Ok(true)) {
        return false;
    }
    match (registered.started_at, kernel::started(pid)) {
        // The kernel keeps a start to the clock tick; the registry to the millisecond.
        (Some(session), Some(process)) => {
            i64::try_from(process).is_ok_and(|process| process <= session + 2_000)
        }
        _ => true,
    }
}

/// A live session the registry names, proved: its process exists and its start is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Strict {
    pub pid: u32,
    /// Milliseconds since 1970.
    pub start: u64,
    pub job: Option<String>,
    pub kind: Option<String>,
}

/// The live session the registry names in each of `wanted` that one is in. Strict where
/// [`live`] is lenient: an entry that does not read, a process whose start cannot be
/// read, or two live entries for one session is an error, never a guess.
pub fn strict(
    dir: &Path,
    wanted: &HashSet<String>,
    exists: impl Fn(i32) -> std::io::Result<bool>,
    started: impl Fn(u32) -> Option<u64>,
) -> Result<HashMap<String, Strict>, String> {
    let mut found = HashMap::new();
    let listed = match std::fs::read_dir(dir) {
        Ok(listed) => listed,
        // No registry yet: no session was ever registered here.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(format!("Claude Code's registry is unreadable: {e}")),
    };
    for entry in listed {
        let path = entry
            .map_err(|e| format!("Claude Code's registry is unreadable: {e}"))?
            .path();
        if path.extension().and_then(|part| part.to_str()) != Some("json") {
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        let registered: Registered = serde_json::from_slice(&bytes)
            .map_err(|e| format!("decoding {}: {e}", path.display()))?;
        if !wanted.contains(&registered.session_id) {
            continue;
        }
        let pid = u32::try_from(registered.pid)
            .map_err(|_| format!("{} names no process", path.display()))?;
        if !exists(registered.pid).map_err(|e| format!("checking pid {pid}: {e}"))? {
            continue;
        }
        let start = started(pid).ok_or_else(|| format!("pid {pid} has no start to read"))?;
        if found.contains_key(&registered.session_id) {
            return Err(format!(
                "two live sessions are registered for {}",
                registered.session_id
            ));
        }
        found.insert(
            registered.session_id,
            Strict {
                pid,
                start,
                job: registered.job_id,
                kind: registered.kind,
            },
        );
    }
    Ok(found)
}

/// [`strict`] for one session, asked of the kernel.
pub fn strict_one(dir: &Path, session: &str) -> Result<Option<Strict>, String> {
    let wanted = HashSet::from([session.to_owned()]);
    Ok(strict(dir, &wanted, kernel::exists, kernel::started)?.remove(session))
}

/// A Claude Code background session that holds a conversation: what only Claude Code's
/// own `claude stop` ends, as the runtime keeps it (an opaque owner token).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub session: String,
    pub job: String,
    pub pid: u32,
    /// Milliseconds since 1970.
    pub start: u64,
}

impl Job {
    /// The owner token: this, as one JSON line, at most [`OWNER_MAX`] characters.
    pub fn token(&self) -> Option<String> {
        let token = serde_json::to_string(self).ok()?;
        (token.len() <= steery_sdk::wire::harness::OWNER_MAX).then_some(token)
    }

    /// The job an owner token names, or none where it is no token of this driver's.
    pub fn of(token: &str) -> Option<Self> {
        serde_json::from_str(token).ok()
    }
}

/// The background session holding a conversation now, where one does and its process's
/// start is read.
pub fn job(dir: &Path, external_id: &str) -> Option<Job> {
    let live = live(dir, external_id)?;
    let pid = u32::try_from(live.pid).ok()?;
    Some(Job {
        session: external_id.to_owned(),
        job: live.job?,
        pid,
        start: kernel::started(pid)?,
    })
}

/// What this driver reads of other processes from the kernel: whether one exists, and
/// when it started, in milliseconds since 1970, as the operating-system adapters give it.
pub mod kernel {
    /// Whether `pid` exists and is this user's to signal (signal 0 sends nothing).
    pub fn exists(pid: i32) -> std::io::Result<bool> {
        if pid <= 0 {
            return Ok(false);
        }
        // SAFETY: signal 0 checks the pid and sends nothing.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            _ => Err(error),
        }
    }

    /// When the live process under `pid` started; `None` where there is none, or only a
    /// zombie waiting to be reaped.
    #[cfg(target_os = "macos")]
    pub fn started(pid: u32) -> Option<u64> {
        let pid = i32::try_from(pid).ok()?;
        // SAFETY: an all-zero `proc_bsdinfo` is a valid value of a plain C struct.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: the buffer is one `proc_bsdinfo`, and `size` is its size.
        let read = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        if read != size || info.pbi_status == libc::SZOMB {
            return None;
        }
        Some(info.pbi_start_tvsec * 1000 + info.pbi_start_tvusec / 1000)
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
        let (boot, tick) = (*CLOCK)?;
        Some(boot + ticks * 1000 / tick)
    }

    /// The boot time in milliseconds since 1970 and the clock ticks per second.
    #[cfg(not(target_os = "macos"))]
    static CLOCK: std::sync::LazyLock<Option<(u64, u64)>> = std::sync::LazyLock::new(|| {
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        let boot: u64 = text
            .lines()
            .find_map(|line| line.strip_prefix("btime "))?
            .trim()
            .parse()
            .ok()?;
        // SAFETY: sysconf only reads a constant.
        let tick = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        (tick > 0).then(|| (boot * 1000, tick as u64))
    });
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub struct Scratch(pub PathBuf);

    impl Scratch {
        pub fn new(name: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "steery-claude-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn registry_with(entries: &[(i32, &str, Option<&str>)]) -> Scratch {
        let dir = Scratch::new("registry");
        for (pid, session, socket) in entries {
            let mut entry = json!({
                "pid": pid,
                "sessionId": session,
                "cwd": "/Users/person/src/repo",
                "kind": "interactive",
                "peerProtocol": 1,
            });
            if let Some(socket) = socket {
                entry["messagingSocketPath"] = json!(socket);
            }
            std::fs::write(dir.path().join(format!("{pid}.json")), entry.to_string()).unwrap();
            std::fs::write(
                dir.path().join(format!("{pid}.abc.key")),
                json!({"peerToken": format!("token-{pid}"), "pidDomain": "darwin"}).to_string(),
            )
            .unwrap();
        }
        dir
    }

    /// The registry names a session somebody is in; a dead pid is a session nobody is in.
    #[test]
    fn a_registered_session_with_a_live_process_is_in_a_terminal() {
        let dir = registry_with(&[
            (41, "alive-1", Some("/tmp/cc-socks/41.sock")),
            (42, "dead-1", Some("/tmp/cc-socks/42.sock")),
        ]);
        let alive = |registered: &Registered| registered.pid == 41;
        let found = live_in(dir.path(), "alive-1", alive).expect("41 is alive");
        assert_eq!(found.pid, 41);
        assert_eq!(
            found.socket.as_deref(),
            Some(Path::new("/tmp/cc-socks/41.sock"))
        );
        assert!(
            live_in(dir.path(), "dead-1", alive).is_none(),
            "its process is gone"
        );
        assert!(live_in(dir.path(), "unknown", alive).is_none());
        assert_eq!(peer_token(dir.path(), 41).as_deref(), Some("token-41"));
        assert!(peer_token(dir.path(), 7).is_none());
    }

    /// Every live session holds its conversation, a background session's and a terminal's
    /// alike; an entry whose pid another process took after Claude Code left it holds
    /// nothing, and neither does one whose process is gone.
    #[test]
    fn the_registry_holds_what_a_live_session_is_in_and_nothing_a_reused_pid_names() {
        let me = std::process::id() as i32;
        let dir = registry_with(&[]);
        let now = chrono::Utc::now().timestamp_millis();
        for (pid, session, kind, job, started) in [
            (me, "terminal-1", "interactive", None, now),
            (i32::MAX - 7, "gone-1", "bg", Some("aaaa1111"), now),
        ] {
            std::fs::write(
                dir.path().join(format!("{pid}.json")),
                json!({"pid": pid, "sessionId": session, "kind": kind, "jobId": job,
                    "startedAt": started})
                .to_string(),
            )
            .unwrap();
        }
        assert_eq!(
            live(dir.path(), "terminal-1").map(|live| live.pid),
            Some(me),
            "this process is alive and started before its session"
        );
        assert_eq!(live(dir.path(), "gone-1"), None, "its process is gone");
        std::fs::write(
            dir.path().join(format!("{me}.json")),
            json!({"pid": me, "sessionId": "terminal-1", "kind": "interactive",
                "startedAt": now - 3_600_000 * 24 * 365})
            .to_string(),
        )
        .unwrap();
        assert_eq!(
            live(dir.path(), "terminal-1"),
            None,
            "a session that started a year before its process is another process's pid"
        );
        std::fs::write(
            dir.path().join("41.json"),
            json!({"pid": 41, "sessionId": "bg-1", "kind": "bg", "jobId": "6b44d2be",
                "messagingSocketPath": "/tmp/cc-socks/41.sock"})
            .to_string(),
        )
        .unwrap();
        let found = live_in(dir.path(), "bg-1", |registered| registered.pid == 41).unwrap();
        assert_eq!(
            found.job.as_deref(),
            Some("6b44d2be"),
            "attach takes its short id"
        );
    }

    #[test]
    fn strict_registry_requires_verifiable_live_identity() {
        let dir = Scratch::new("strict");
        let wanted = HashSet::from(["s1".to_owned()]);
        std::fs::write(
            dir.path().join("1.json"),
            br#"{"pid":123,"sessionId":"s1","kind":"bg","jobId":"j1"}"#,
        )
        .unwrap();
        let live = strict(dir.path(), &wanted, |_| Ok(true), |_| Some(7))
            .unwrap()
            .remove("s1")
            .unwrap();
        assert_eq!((live.pid, live.job.as_deref()), (123, Some("j1")));
        assert!(strict(dir.path(), &wanted, |_| Ok(true), |_| None).is_err());
        std::fs::write(
            dir.path().join("3.json"),
            br#"{"pid":124,"sessionId":"s1","kind":"bg","jobId":"j2"}"#,
        )
        .unwrap();
        assert!(strict(dir.path(), &wanted, |_| Ok(true), |_| Some(7)).is_err());
        std::fs::remove_file(dir.path().join("3.json")).unwrap();
        assert!(
            strict(dir.path(), &wanted, |_| Ok(false), |_| None)
                .unwrap()
                .is_empty()
        );
        std::fs::write(dir.path().join("2.json"), b"{").unwrap();
        assert!(strict(dir.path(), &wanted, |_| Ok(false), |_| None).is_err());
    }

    #[test]
    fn an_owner_token_names_its_job_and_nothing_else() {
        let job = Job {
            session: "6920e989-aa9e-467c-9b64-569ba2ca6059".into(),
            job: "6920e989".into(),
            pid: 74329,
            start: 1_790_237_098_062,
        };
        assert_eq!(Job::of(&job.token().unwrap()), Some(job));
        assert_eq!(Job::of(r#"{"thread":"x"}"#), None);
    }

    #[test]
    fn the_kernel_reads_this_process() {
        let me = std::process::id();
        assert!(kernel::exists(me as i32).unwrap());
        let started = kernel::started(me).unwrap();
        let now = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap();
        assert!(started <= now && now - started < 3_600_000);
    }
}
