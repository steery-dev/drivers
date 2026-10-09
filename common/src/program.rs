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
