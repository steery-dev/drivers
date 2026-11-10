//! What a driver reads of other processes from the kernel: whether one exists, when it
//! started, in milliseconds since 1970, as the operating-system adapters give it, and the
//! processes it started; and how a driver ends those a stopped turn left running.

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

/// Every live process `pid` started, and those they started, each with when it
/// started: what a session runs beside it, its commands among them, read while it
/// lives, before anything is reparented away from it.
pub fn descendants(pid: u32) -> Vec<(u32, u64)> {
    let mut found: Vec<(u32, u64)> = Vec::new();
    let mut next = vec![pid];
    while let Some(parent) = next.pop() {
        for child in children(parent) {
            if child != pid
                && !found.iter().any(|(seen, _)| *seen == child)
                && let Some(start) = started(child)
            {
                found.push((child, start));
                next.push(child);
            }
        }
    }
    found
}

/// The processes `pid` started that live now.
#[cfg(target_os = "macos")]
fn children(pid: u32) -> Vec<u32> {
    const MOST: usize = 4096;
    let Ok(parent) = i32::try_from(pid) else {
        return Vec::new();
    };
    let mut pids = vec![0 as libc::pid_t; MOST];
    let Ok(size) = i32::try_from(MOST * std::mem::size_of::<libc::pid_t>()) else {
        return Vec::new();
    };
    // SAFETY: the buffer holds `MOST` pids, and `size` is its size in bytes.
    let listed = unsafe { libc::proc_listchildpids(parent, pids.as_mut_ptr().cast(), size) };
    let listed = usize::try_from(listed).unwrap_or_default().min(MOST);
    pids.truncate(listed);
    pids.into_iter()
        .filter_map(|child| u32::try_from(child).ok())
        .filter(|child| *child > 0)
        .collect()
}

/// The processes `pid` started that live now: each `/proc/<pid>/stat` whose parent,
/// the second field after the last `)`, is `pid`.
#[cfg(not(target_os = "macos"))]
fn children(pid: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|child| {
            std::fs::read_to_string(format!("/proc/{child}/stat"))
                .ok()
                .and_then(|text| {
                    let rest = &text[text.rfind(')')? + 1..];
                    rest.split_whitespace().nth(1)?.parse::<u32>().ok()
                })
                == Some(pid)
        })
        .collect()
}

/// Send `signal` to each of `processes` that is still the process read then (the
/// same pid, started at the same time), and to the process group it leads, where it
/// leads one; how many were still there.
pub fn signal(processes: &[(u32, u64)], signal: libc::c_int) -> usize {
    let mut there = 0;
    for &(pid, start) in processes {
        if started(pid) != Some(start) {
            continue;
        }
        let Ok(pid) = i32::try_from(pid) else {
            continue;
        };
        there += 1;
        // SAFETY: getpgid and kill only name a process and a signal.
        unsafe {
            if libc::getpgid(pid) == pid {
                libc::kill(-pid, signal);
            }
            libc::kill(pid, signal);
        }
    }
    there
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kernel_reads_this_process() {
        let me = std::process::id();
        assert!(exists(me as i32).unwrap());
        let started = started(me).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(started <= now && now - started < 3_600_000);
    }

    #[test]
    fn a_stopped_session_leaves_none_of_its_commands_running() {
        use std::os::unix::process::CommandExt as _;
        use std::time::{Duration, Instant};
        // A command in a group of its own, as Claude Code runs its tools' shells, with a
        // child of its own in that group. Read from the shell down, not from this test
        // process, whose other tests' children are theirs.
        let mut shell = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 30 & wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        let shell_pid = shell.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut running = loop {
            let running = descendants(shell_pid);
            if !running.is_empty() || Instant::now() >= deadline {
                break running;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(running.len(), 1, "the shell's child is read: {running:?}");
        running.push((shell_pid, started(shell_pid).unwrap()));
        assert_eq!(signal(&running, libc::SIGTERM), 2);
        shell.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while running
            .iter()
            .any(|&(pid, start)| started(pid) == Some(start))
        {
            assert!(Instant::now() < deadline, "still running: {running:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
        // What has ended is never signalled again: its pid may be another process's.
        assert_eq!(signal(&running, libc::SIGTERM), 0);
    }
}
