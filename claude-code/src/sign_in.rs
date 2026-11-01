//! The one need Claude Code has (PROTOCOL.md §4.1, Needs): a sign-in, where it is found and
//! `claude auth status` says it is not signed in. Its person gives it with an Anthropic API
//! key, which the runtime keeps and sets as `ANTHROPIC_API_KEY` on every host, or through
//! Claude Code's own `claude auth login`, run unmodified on a terminal of the driver's:
//! the driver relays the link it prints and writes back the code its person pastes. Claude
//! Code keeps what that sign-in gives; the driver reads no token and keeps none.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use steery_sdk::wire::harness::{Field, Give, Given, Need, NeedLink, Way};
use steery_sdk::{Error, refusal};
use tokio::process::Child;
use tokio::sync::watch;

/// The need, by its id in every plan.
const NEED: &str = "sign-in";
/// Its ways, the driver's pick first, and the step a started sign-in asks next.
const API_KEY: &str = "api-key";
const CLAUDE_SIGN_IN: &str = "claude-sign-in";
const CLAUDE_SIGN_IN_CODE: &str = "claude-sign-in-code";
/// The fields their person fills.
const KEY: &str = "key";
const CODE: &str = "code";

/// The variable Claude Code reads an Anthropic API key from. Its docs
/// (https://code.claude.com/docs/en/authentication.md, "Authentication precedence"): in
/// non-interactive mode (`-p`) the key is always used when present, over a plan's sign-in;
/// an interactive session asks once to approve it. `claude auth status --json` counts it as
/// signed in (`"authMethod": "api_key"`, `"apiKeySource": "ANTHROPIC_API_KEY"`, Claude Code
/// 2.1.310).
const VARIABLE: &str = "ANTHROPIC_API_KEY";
/// How a Claude API key starts, as https://platform.claude.com/docs/en/manage-claude/authentication
/// says it: "Static `sk-ant-api...` secret". An Admin key (`sk-ant-admin01-`) or an OAuth
/// token (`sk-ant-oat01-`) is none.
const KEY_STARTS: &str = "sk-ant-api";
/// Where a key is made, as the same page names it.
const KEYS_PAGE: &str = "https://platform.claude.com/settings/keys";

/// How long `claude auth login` has to print its link: it printed it at once (2.1.310).
const LINK_WITHIN: Duration = Duration::from_secs(15);
/// How long a code written to it has to end it: inside `needs/give`'s 30 s.
const CODE_WITHIN: Duration = Duration::from_secs(20);
/// How long a started sign-in waits for its code before it is ended.
const KEPT: Duration = Duration::from_secs(10 * 60);

/// The need, where Claude Code is found here and not signed in.
pub fn need() -> Need {
    Need {
        id: NEED.into(),
        title: "Sign Claude Code in?".into(),
        line: "Claude Code is not signed in on this host, so it runs no turn here.".into(),
        ways: vec![
            Way {
                id: API_KEY.into(),
                label: "Use your Anthropic API key".into(),
                about: Some(
                    "Billed per token to your Claude Console account, on every host. Claude Code \
                     always uses it in non-interactive runs, over any plan it is signed in with."
                        .into(),
                ),
                link: Some(NeedLink {
                    label: "Create a key".into(),
                    url: KEYS_PAGE.into(),
                    fills: None,
                }),
                fields: vec![Field {
                    name: KEY.into(),
                    label: "API key".into(),
                    about: Some(format!("Starts with {KEY_STARTS}")),
                    secret: true,
                }],
                ends_at: None,
            },
            Way {
                id: CLAUDE_SIGN_IN.into(),
                label: "Sign in with your Claude plan".into(),
                about: Some(
                    "Claude Code's own sign-in, kept on this host: each new host asks again."
                        .into(),
                ),
                link: None,
                fields: Vec::new(),
                ends_at: None,
            },
        ],
    }
}

/// The sign-ins started here and waiting for their code, by need. A driver started again
/// has none.
#[derive(Default, Clone)]
pub struct SignIns(Arc<Mutex<HashMap<String, Login>>>);

/// One `claude auth login` running on a terminal of the driver's: what it printed so far,
/// read off that terminal until it closes.
struct Login {
    id: u64,
    child: Child,
    terminal: File,
    said: watch::Receiver<Vec<u8>>,
}

impl SignIns {
    /// `needs/give`.
    pub async fn give(&self, give: Give) -> Result<Given, Error> {
        if give.need != NEED {
            return Err(gone(format!("Claude Code has no need {} here", give.need)));
        }
        match give.way.as_str() {
            API_KEY => {
                self.waiting().remove(NEED);
                key(give.values.get(KEY))
            }
            CLAUDE_SIGN_IN => self.start().await,
            CLAUDE_SIGN_IN_CODE => {
                let code = give.values.get(CODE).map_or("", |code| code.trim());
                if code.is_empty() {
                    return Err(refused("Paste the code the sign-in page shows"));
                }
                self.finish(code).await
            }
            way => Err(gone(format!("Claude Code signs in no way {way}"))),
        }
    }

    fn waiting(&self) -> MutexGuard<'_, HashMap<String, Login>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start `claude auth login` and answer the step its link and code are: the link it
    /// prints for a browser that cannot return to it, whose page shows the code. Claude
    /// Code 2.1.310 printed that one with `redirect_uri=https://platform.claude.com/oauth/code/callback`,
    /// no loopback address, so nothing fills the code but its person. A sign-in started
    /// before is ended.
    async fn start(&self) -> Result<Given, Error> {
        let binary = crate::binary().ok_or_else(|| gone("Claude Code is not installed here"))?;
        let login = Login::start(&binary)
            .map_err(|e| refused(format!("claude auth login did not start: {e}")))?;
        let mut said = login.said.clone();
        let printed = tokio::time::timeout(LINK_WITHIN, async {
            said.wait_for(|said| link(said).is_some())
                .await
                .ok()
                .and_then(|said| link(&said))
        })
        .await
        .ok()
        .flatten();
        let Some(url) = printed else {
            let own = own_line(&login.said.borrow(), 0, None);
            return Err(refused(own.unwrap_or_else(|| {
                format!(
                    "claude auth login printed no sign-in link within {} s",
                    LINK_WITHIN.as_secs()
                )
            })));
        };
        let id = login.id;
        let now = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or_default();
        let ends_at = now + KEPT.as_secs();
        self.waiting().insert(NEED.into(), login);
        let kept = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(KEPT).await;
            let mut waiting = kept.waiting();
            if waiting.get(NEED).is_some_and(|login| login.id == id) {
                waiting.remove(NEED);
            }
        });
        Ok(Given::Next(Way {
            id: CLAUDE_SIGN_IN_CODE.into(),
            label: "Paste the code Claude shows".into(),
            about: Some("Sign in on Claude's page; it then shows a code to paste here.".into()),
            link: Some(NeedLink {
                label: "Sign in to Claude".into(),
                url,
                fills: None,
            }),
            fields: vec![Field {
                name: CODE.into(),
                label: "Code".into(),
                about: Some("Shown once you sign in".into()),
                secret: true,
            }],
            ends_at: Some(ends_at),
        }))
    }

    /// Write `code` to the sign-in waiting here, and answer how Claude Code ended it. One
    /// still running after [`CODE_WITHIN`] waits again, so a code typed again reaches it.
    async fn finish(&self, code: &str) -> Result<Given, Error> {
        let mut login = self.waiting().remove(NEED).ok_or_else(|| {
            gone("No Claude Code sign-in waits on this host any more: start it again")
        })?;
        let before = login.said.borrow().len();
        login
            .terminal
            .write_all(format!("{code}\r").as_bytes())
            .map_err(|e| refused(format!("claude auth login took no code: {e}")))?;
        match tokio::time::timeout(CODE_WITHIN, login.child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(Given::Done {
                line: "Claude Code is signed in on this host".into(),
                env: BTreeMap::new(),
            }),
            Ok(Ok(status)) => {
                // Its last words reach the terminal before it closes.
                let mut said = login.said.clone();
                let _ = tokio::time::timeout(Duration::from_secs(1), said.wait_for(|_| false)).await;
                let own = own_line(&said.borrow(), before, Some(code));
                Err(refused(own.unwrap_or_else(|| {
                    format!("claude auth login ended with {status}")
                })))
            }
            Ok(Err(e)) => Err(refused(format!("claude auth login: {e}"))),
            Err(_) => {
                let own = own_line(&login.said.borrow(), before, Some(code));
                self.waiting().entry(NEED.into()).or_insert(login);
                Err(refused(own.unwrap_or_else(|| {
                    format!(
                        "Claude Code did not finish signing in within {} s",
                        CODE_WITHIN.as_secs()
                    )
                })))
            }
        }
    }
}

impl Login {
    /// `claude auth login` on a new terminal, which it needs: with no terminal to read a
    /// pasted code from it stops ("Couldn't start the local listener that receives the
    /// browser sign-in", Claude Code's docs, for input piped into it). It is killed once
    /// dropped.
    fn start(binary: &Path) -> std::io::Result<Self> {
        let (terminal, theirs) = terminal()?;
        let mut command = steery_driver_common::program::helper(binary);
        command.args(["auth", "login"]).stdout(Stdio::null());
        let side = theirs.as_raw_fd();
        // SAFETY: between fork and exec the child calls only `login_tty`, which makes `side`
        // its controlling terminal and its standard input, output and error: `setsid`,
        // `ioctl`, `dup2` and `close`, each safe there.
        unsafe {
            command.pre_exec(move || match libc::login_tty(side) {
                0 => Ok(()),
                _ => Err(std::io::Error::last_os_error()),
            });
        }
        let child = command.spawn()?;
        drop(theirs);
        let (tell, said) = watch::channel(Vec::new());
        let mut reading = terminal.try_clone()?;
        // Reading ends as the terminal closes: Claude Code ended, or was killed.
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match reading.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => tell.send_modify(|said| said.extend_from_slice(&chunk[..read])),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Ok(Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            child,
            terminal,
            said,
        })
    }
}

/// A new terminal: the side the driver reads and writes, and the side Claude Code is given.
/// Wide, so a link is printed on one line; neither side is inherited by anything else the
/// driver starts.
fn terminal() -> std::io::Result<(File, OwnedFd)> {
    let (mut ours, mut theirs) = (-1, -1);
    let settings: *mut libc::termios = std::ptr::null_mut();
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 1000,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `openpty` writes one descriptor to each of `ours` and `theirs` and reads
    // `size`; it is given no name buffer and no settings. The casts meet each platform's
    // spelling of the same pointers (`*mut` on macOS, `*const` on Linux).
    let opened = unsafe {
        libc::openpty(
            &mut ours,
            &mut theirs,
            std::ptr::null_mut(),
            settings as _,
            (&raw mut size) as _,
        )
    };
    if opened != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `openpty` opened both descriptors, and nothing else owns them.
    let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(ours), OwnedFd::from_raw_fd(theirs)) };
    for side in [&ours, &theirs] {
        // SAFETY: `F_SETFD` on a descriptor this function owns.
        if unsafe { libc::fcntl(side.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok((File::from(ours), theirs))
}

/// `ANTHROPIC_API_KEY` for this harness, where `key` reads as an Anthropic API key.
fn key(key: Option<&String>) -> Result<Given, Error> {
    let key = key.map_or("", |key| key.trim());
    if key.is_empty() {
        return Err(refused("Paste your Anthropic API key"));
    }
    if !key.starts_with(KEY_STARTS) {
        return Err(refused(format!(
            "An Anthropic API key starts with {KEY_STARTS}"
        )));
    }
    Ok(Given::Done {
        line: "Claude Code uses your Anthropic API key".into(),
        env: BTreeMap::from([(VARIABLE.to_owned(), key.to_owned())]),
    })
}

/// The first link in what Claude Code printed, once it is whole: a space or a line's end
/// follows it.
fn link(said: &[u8]) -> Option<String> {
    let text = plain(said);
    let rest = &text[text.find("https://")?..];
    let end = rest.find(char::is_whitespace)?;
    Some(rest[..end].to_owned())
}

/// Claude Code's own last line after the first `from` bytes it printed: one it had not
/// printed before them (its prompt, drawn again), and not the code `echo` its terminal
/// echoed.
fn own_line(said: &[u8], from: usize, echo: Option<&str>) -> Option<String> {
    let from = from.min(said.len());
    let earlier = plain(&said[..from]);
    let earlier: HashSet<&str> = lines(&earlier).collect();
    let later = plain(&said[from..]);
    lines(&later)
        .filter(|line| !earlier.contains(line) && echo.is_none_or(|echo| !line.contains(echo)))
        .last()
        .map(str::to_owned)
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
}

/// What a terminal shows of `said`: its text without escape sequences (a colour, a cursor
/// move, a hyperlink's target, whose text stays) or bells.
fn plain(said: &[u8]) -> String {
    let text = String::from_utf8_lossy(said);
    let mut shown = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // A control sequence ends at its final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                }
                // An operating-system command ends at a bell or at `ESC \`.
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\x07' => {}
            c => shown.push(c),
        }
    }
    shown
}

fn gone(line: impl Into<String>) -> Error {
    Error::refused(refusal::NOT_FOUND, line)
}

fn refused(line: impl Into<String>) -> Error {
    Error::refused(refusal::REFUSED, line)
}
