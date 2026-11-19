//! When a harness last wrote its login's credentials here, in milliseconds since 1970: the
//! `signed_in_at` of `models` (protocol 18). A login writes them, and so does a token the
//! harness refreshed. Each driver names where its harness keeps them: a file, a macOS
//! keychain item, or both.

use std::path::Path;

/// When `file` last changed; none where it is not here.
pub fn file_written(file: &Path) -> Option<u64> {
    let at = std::fs::metadata(file).and_then(|meta| meta.modified()).ok()?;
    let since = at.duration_since(std::time::UNIX_EPOCH).ok()?;
    u64::try_from(since.as_millis()).ok()
}

/// When the macOS keychain's generic password of `service`, and of `account` where named, was
/// last written: its `mdat`, as `security find-generic-password` prints the item's attributes,
/// without `-w` or `-g`, so never its secret: `"mdat"<timedate>=0x… "20260930092224Z\000"`.
/// None off macOS, and where it gave no such line.
pub async fn keychain_written(service: &str, account: Option<&str>) -> Option<u64> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let mut command = crate::program::helper(Path::new("/usr/bin/security"));
    command.args(["find-generic-password", "-s", service]);
    if let Some(account) = account {
        command.args(["-a", account]);
    }
    let answer = match crate::program::ask(command).await {
        Ok(answer) => answer,
        Err(why) => {
            eprintln!("security find-generic-password: {why}, so no sign-in time is read");
            return None;
        }
    };
    let printed = String::from_utf8_lossy(&answer.stdout);
    let stamp = printed
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("\"mdat\"<timedate>="))?
        .split('"')
        .nth(1)?;
    let digits = |at: std::ops::Range<usize>| stamp.get(at)?.parse::<u32>().ok();
    let at = chrono::NaiveDate::from_ymd_opt(
        i32::try_from(digits(0..4)?).ok()?,
        digits(4..6)?,
        digits(6..8)?,
    )?
    .and_hms_opt(digits(8..10)?, digits(10..12)?, digits(12..14)?)?;
    (stamp.get(14..15) == Some("Z"))
        .then(|| u64::try_from(at.and_utc().timestamp_millis()).ok())
        .flatten()
}
