//! The harness's executable: where it is, how the driver starts it for itself, and what
//! release it says it is.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// How long a probe — `--version` — has to answer before nothing is claimed of it.
pub const PROBE: Duration = Duration::from_secs(5);

/// The first executable named `name` on `PATH`.
pub fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| executable(dir.join(name)))
}

/// `path`, where it is an executable file.
pub fn executable(path: PathBuf) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(&path)
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .then_some(path)
}

/// The program protocol the drivers speak. Every runtime Steery supports offers it
/// (version 4 came in runtime 7.0.130; the oldest supported is later), so no driver answers
/// an older one.
pub const PROTOCOL: u32 = 4;

/// [`PROTOCOL`], where the runtime `offered` it; none where it did not.
pub fn protocol(offered: &[u32]) -> Option<u32> {
    offered.contains(&PROTOCOL).then_some(PROTOCOL)
}

/// Why the programs `names` are not all on `PATH`, as a line a person reads (`["sh",
/// "curl"]` with `curl` missing: "needs `sh` and `curl` on PATH: `curl` is not there"); none
/// where each is.
pub fn lacks(names: &[&str]) -> Option<String> {
    let missing: Vec<String> = names
        .iter()
        .filter(|name| on_path(name).is_none())
        .map(|name| format!("`{name}`"))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let all: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
    Some(format!(
        "needs {} on PATH: {} {} not there",
        all.join(" and "),
        missing.join(" and "),
        if missing.len() == 1 { "is" } else { "are" }
    ))
}

/// A line for a person where `name` is installed in more than one place here: the one the
/// driver runs, `used`, and the others, which it never does. The places are each folder on
/// `PATH` and the installer's folder `installed`; one reached through a link to the same
/// file is the same place. None where there is only `used`.
pub fn others_note(label: &str, name: &str, installed: Option<&Path>, used: &Path) -> Option<String> {
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    let mut others: Vec<PathBuf> = Vec::new();
    let folders = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(installed.map(Path::to_path_buf));
    for folder in folders {
        let Some(found) = executable(folder.join(name)) else {
            continue;
        };
        if !same(&found, used) && !others.iter().any(|other| same(other, &found)) {
            others.push(found);
        }
    }
    if others.is_empty() {
        return None;
    }
    let others: Vec<String> = others.iter().map(|path| path.display().to_string()).collect();
    Some(format!(
        "{label} is installed in more than one place here: Steery runs {} (the first on PATH, \
         else its installer's folder) and never {}.",
        used.display(),
        others.join(", ")
    ))
}

/// `binary`, started by the driver for something that writes no conversation: a release,
/// a model catalog, a listing, a stop. `STEERY_STARTED` says so to Steery's plugin, which
/// then stays out (PROTOCOL.md §3.3). It reads nothing, its errors go nowhere, and it is
/// killed once its handle is dropped, so a probe given up on never outlives it.
pub fn helper(binary: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(binary);
    command
        .env("STEERY_STARTED", format!("{}:nothing", std::process::id()))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

/// The first line `binary --version` prints, or none where it failed or did not answer
/// within [`PROBE`]; one that hangs is killed then.
pub async fn version(binary: &Path) -> Option<String> {
    let output = tokio::time::timeout(
        PROBE,
        helper(binary)
            .arg("--version")
            .stdout(Stdio::piped())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// What a probe printed and how it ended.
pub struct Answer {
    /// The exit code; none where a signal ended it.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    /// The first line standard error holds, trimmed; empty where it holds none.
    pub said: String,
}

/// `command` (a [`helper`], its arguments given) run to its end within [`PROBE`], its
/// standard output and error read. Why it gave no answer, in a few words, where it did not
/// start or did not end in time, in which case it is killed.
pub async fn ask(mut command: tokio::process::Command) -> Result<Answer, String> {
    let output = tokio::time::timeout(
        PROBE,
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| format!("did not answer within {} s", PROBE.as_secs()))?
    .map_err(|error| format!("did not start: {error}"))?;
    Ok(Answer {
        code: output.status.code(),
        stdout: output.stdout,
        said: String::from_utf8_lossy(&output.stderr)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .to_owned(),
    })
}

/// The first `major.minor.patch` a text names: `2.1.0 (Claude Code)`, `omp/18.4.12+abc`,
/// `codex-cli 0.162.0` and `v22.19.0` each name one.
pub fn release_within(text: &str) -> Option<(u64, u64, u64)> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|word| {
            let mut parts = word.trim_matches('.').split('.');
            let release = (
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
            );
            parts.next().is_none().then_some(release)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_is_the_first_three_numbers_a_text_names() {
        assert_eq!(release_within("2.1.0 (Claude Code)"), Some((2, 1, 0)));
        assert_eq!(release_within("omp/18.4.12+abc"), Some((18, 4, 12)));
        assert_eq!(release_within("codex-cli 0.162.0"), Some((0, 162, 0)));
        assert_eq!(release_within("v22.19.0"), Some((22, 19, 0)));
        assert_eq!(release_within("1.2 then 3.4.5"), Some((3, 4, 5)));
        assert_eq!(release_within("1.2.3.4"), None);
        assert_eq!(release_within("none"), None);
    }

    /// A probe that never answers is given up on within [`PROBE`] and killed then: a
    /// `--version` that hangs never holds the driver.
    #[tokio::test]
    async fn a_probe_that_hangs_is_given_up_on_and_killed() {
        let folder =
            std::env::temp_dir().join(format!("steery-common-probe-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let pid = folder.join("pid");
        let script = folder.join("hangs");
        let written = folder.join("hangs.tmp");
        std::fs::write(
            &written,
            format!("#!/bin/sh\necho $$ > {}\nexec sleep 60\n", pid.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&written, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&written, &script).unwrap();
        let began = std::time::Instant::now();
        assert_eq!(version(&script).await, None);
        let took = began.elapsed();
        assert!(
            took >= PROBE && took < PROBE + Duration::from_secs(3),
            "{took:?}"
        );
        let pid = std::fs::read_to_string(&pid).unwrap();
        // Gone, or a zombie nobody has reaped yet: either way it runs no more.
        let alive = || {
            std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", pid.trim()])
                .output()
                .is_ok_and(|ps| {
                    let stat = String::from_utf8_lossy(&ps.stdout);
                    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
                })
        };
        for _ in 0..50 {
            if !alive() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(), "the probe was killed");
        let _ = std::fs::remove_dir_all(&folder);
    }
}
