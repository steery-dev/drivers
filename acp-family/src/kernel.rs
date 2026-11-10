//! What the drivers read of other processes from the kernel: when one started, in
//! milliseconds since 1970, as the operating-system adapters give it.

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
