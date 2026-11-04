//! A harness's own sign-in, run unmodified on a terminal of the driver's (PROTOCOL.md §4.1,
//! Needs): the driver relays the link it prints and gives it back what its person's browser
//! came back with. An address on the login's own loopback port is fetched from inside this
//! host, so the harness's own listener completes it; a code is typed into the login's prompt,
//! where it has one; nothing at all is the harness having caught its return itself. The
//! driver never signs in to a vendor itself and keeps nothing a sign-in gives.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use steery_sdk::{Error, refusal};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, Command};
use tokio::sync::watch;

/// How long a started sign-in waits for its person before it is ended.
pub const KEPT: Duration = Duration::from_secs(10 * 60);
/// How long a login given its return has to end: inside `needs/give`'s 30 s.
const ENDS_WITHIN: Duration = Duration::from_secs(20);
/// How long the login's own listener has to answer the address it is given.
const VISIT_WITHIN: Duration = Duration::from_secs(5);

/// One harness login running on a terminal of the driver's: what it printed so far, read
/// off that terminal until it closes. It is killed once dropped.
pub struct Login {
    id: u64,
    /// The step it waits on, by its way's id.
    way: String,
    /// Whether it reads a code or an address on its terminal.
    reads_code: bool,
    /// The loopback address its link returns to, once a link is known.
    back: Option<Loopback>,
    child: Child,
    terminal: File,
    said: watch::Receiver<Vec<u8>>,
}

impl Login {
    /// `command` (the harness's login, its arguments given) on a new terminal: a login that
    /// reads a pasted code needs one, and every login prints its link to it. `way` is the
    /// step it will wait on; `reads_code` whether a code typed into it reaches it.
    pub fn start(mut command: Command, way: String, reads_code: bool) -> std::io::Result<Self> {
        let (terminal, theirs) = terminal()?;
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
        command.kill_on_drop(true);
        let child = command.spawn()?;
        drop(theirs);
        let (tell, said) = watch::channel(Vec::new());
        let mut reading = terminal.try_clone()?;
        // Reading ends as the terminal closes: the login ended, or was killed.
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
            way,
            reads_code,
            back: None,
            child,
            terminal,
            said,
        })
    }

    /// The first thing `found` finds in what the login shows, waiting for it at most
    /// `within`; none where it found nothing by then or the login closed its terminal.
    pub async fn shows<T>(&self, within: Duration, found: impl Fn(&str) -> Option<T>) -> Option<T> {
        let mut said = self.said.clone();
        tokio::time::timeout(within, async {
            let mut seen = None;
            let _ = said
                .wait_for(|said| {
                    seen = found(&plain(said));
                    seen.is_some()
                })
                .await;
            seen
        })
        .await
        .ok()
        .flatten()
    }

    /// Whether `found` finds something in what it showed so far.
    pub fn showed<T>(&self, found: impl Fn(&str) -> Option<T>) -> Option<T> {
        found(&plain(&self.said.borrow()))
    }

    /// Its last line of its own, for a person: why it ended or stopped.
    pub fn last_line(&self) -> Option<String> {
        own_line(&self.said.borrow(), 0, None)
    }

    /// Type `line` into it, then Enter.
    pub fn type_line(&mut self, line: &str) -> std::io::Result<()> {
        self.terminal.write_all(format!("{line}\r").as_bytes())
    }

    /// Its link, read off what it printed: where it returns to a loopback address, an
    /// address on that port is what its person gives back. Answers whether it does.
    pub fn link(&mut self, url: &str) -> bool {
        self.back = Loopback::of_link(url);
        self.back.is_some()
    }
}

/// The login waiting for its person on this host: one at a time, a new one ending the one
/// before. A driver started again has none.
#[derive(Default, Clone)]
pub struct Waiting(Arc<Mutex<Option<Login>>>);

impl Waiting {
    fn held(&self) -> MutexGuard<'_, Option<Login>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keep `login` until [`KEPT`] from now, ending one kept before; answers when it ends,
    /// in milliseconds since 1970, a way's `ends_at`.
    pub fn keep(&self, login: Login) -> u64 {
        let id = login.id;
        *self.held() = Some(login);
        let kept = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(KEPT).await;
            let mut held = kept.held();
            if held.as_ref().is_some_and(|login| login.id == id) {
                *held = None;
            }
        });
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        now.saturating_add(u64::try_from(KEPT.as_millis()).unwrap_or(u64::MAX))
    }

    /// End the login kept here, where one is.
    pub fn end(&self) {
        *self.held() = None;
    }

    /// Whether a login waits on the step `way`.
    pub fn waits_on(&self, way: &str) -> bool {
        self.held().as_ref().is_some_and(|login| login.way == way)
    }

    /// Put `login` back, unless another was kept meanwhile.
    fn put_back(&self, login: Login) {
        self.held().get_or_insert(login);
    }

    /// `needs/give` for the step `way` a kept login waits on, `given` what its person gave
    /// (`label` names the harness in a line): done once the login ended well. One still
    /// running is kept, so what is given again reaches it.
    pub async fn give(&self, way: &str, given: &str, label: &str) -> Result<(), Error> {
        let mut login = self
            .held()
            .take_if(|login| login.way == way)
            .ok_or_else(|| {
                gone(format!("No {label} sign-in waits on this host any more: start it again"))
            })?;
        let given = given.trim();
        let before = login.said.borrow().len();
        if !given.is_empty() {
            match login.back.as_ref().and_then(|back| back.reached_by(given)) {
                Some(visit) => {
                    if let Err(why) = visit.go().await {
                        self.put_back(login);
                        return Err(refused(why));
                    }
                }
                None if login.reads_code => {
                    if let Err(e) = login.type_line(given) {
                        return Err(refused(format!("{label}'s sign-in took nothing: {e}")));
                    }
                }
                None => {
                    let line = login.back.as_ref().map_or_else(
                        || format!("{label}'s sign-in reads nothing here: leave it empty"),
                        |back| format!("Paste the whole address your browser ended on: {}", back.starts()),
                    );
                    self.put_back(login);
                    return Err(refused(line));
                }
            }
        }
        let echo = (!given.is_empty()).then_some(given);
        match tokio::time::timeout(ENDS_WITHIN, login.child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => {
                // Its last words reach the terminal before it closes.
                let mut said = login.said.clone();
                let _ = tokio::time::timeout(Duration::from_secs(1), said.wait_for(|_| false)).await;
                let own = own_line(&said.borrow(), before, echo);
                Err(refused(
                    own.unwrap_or_else(|| format!("{label}'s sign-in ended with {status}")),
                ))
            }
            Ok(Err(e)) => Err(refused(format!("{label}'s sign-in: {e}"))),
            Err(_) => {
                let line = match (&login.back, given.is_empty()) {
                    (Some(back), true) => format!(
                        "{label} has not finished signing in: paste the address your browser \
                         ended on ({})",
                        back.starts()
                    ),
                    (None, true) => format!(
                        "{label} has not finished signing in: finish on the page, then continue"
                    ),
                    (_, false) => own_line(&login.said.borrow(), before, echo).unwrap_or_else(|| {
                        format!(
                            "{label} did not finish signing in within {} s",
                            ENDS_WITHIN.as_secs()
                        )
                    }),
                };
                self.put_back(login);
                Err(refused(line))
            }
        }
    }
}

/// The loopback address a login listens on for its browser's return: `localhost`,
/// `127.0.0.1` or `[::1]`, and a port.
struct Loopback {
    host: String,
    port: u16,
}

/// A fetch of an address on a login's own port, from inside this host.
struct Visit {
    host: String,
    port: u16,
    target: String,
}

impl Loopback {
    /// Where `link` (an authorize URL) sends the browser back to, where that is a loopback
    /// address with its port: its `redirect_uri`, or OpenRouter's `callback_url`.
    fn of_link(link: &str) -> Option<Self> {
        let query = link.split_once('?')?.1;
        let query = query.split('#').next().unwrap_or(query);
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            if key != "redirect_uri" && key != "callback_url" {
                return None;
            }
            let (host, port, _) = loopback(&decoded(value)?)?;
            Some(Self { host, port })
        })
    }

    /// How an address on it starts, for a person.
    fn starts(&self) -> String {
        format!("http://{}:{}/…", self.host, self.port)
    }

    /// The fetch `given` is, where it is an address on this port.
    fn reached_by(&self, given: &str) -> Option<Visit> {
        let (host, port, target) = loopback(given)?;
        (port == self.port).then_some(Visit { host, port, target })
    }
}

impl Visit {
    /// GET it and read the listener's status line: the login then ends as its own return
    /// would have ended it.
    async fn go(&self) -> Result<(), String> {
        let unreached = |e: std::io::Error| {
            format!(
                "Nothing answered on {}:{} ({e}): start the sign-in again",
                self.host, self.port
            )
        };
        let host = self.host.trim_start_matches('[').trim_end_matches(']');
        tokio::time::timeout(VISIT_WITHIN, async {
            let mut stream = tokio::net::TcpStream::connect((host, self.port))
                .await
                .map_err(unreached)?;
            stream
                .write_all(
                    format!(
                        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
                        self.target, self.host, self.port
                    )
                    .as_bytes(),
                )
                .await
                .map_err(unreached)?;
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while head.len() < 1024 && !head.ends_with(b"\r\n") {
                match stream.read(&mut byte).await.map_err(unreached)? {
                    0 => break,
                    _ => head.push(byte[0]),
                }
            }
            if head.starts_with(b"HTTP/") {
                Ok(())
            } else {
                Err(format!("{}:{} answered no HTTP", self.host, self.port))
            }
        })
        .await
        .map_err(|_| {
            format!(
                "{}:{} did not answer within {} s",
                self.host,
                self.port,
                VISIT_WITHIN.as_secs()
            )
        })?
    }
}

/// The host, port and target of `address`, where it is a plain `http` address on a
/// loopback host with its port, and holds nothing a request line cannot carry.
fn loopback(address: &str) -> Option<(String, u16, String)> {
    let rest = address.strip_prefix("http://")?;
    if rest.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let split = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, target) = rest.split_at(split);
    let (host, port) = authority.rsplit_once(':')?;
    if !matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
        return None;
    }
    let target = match target {
        "" => "/".to_owned(),
        target if target.starts_with('?') => format!("/{target}"),
        target => target.to_owned(),
    };
    Some((host.to_owned(), port.parse().ok()?, target))
}

/// `value` with its percent escapes and `+` decoded, where that is UTF-8.
fn decoded(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// The first `https` link in `shown`, once it is whole: it ends at the first character no
/// URL holds (RFC 3986), and something follows it.
pub fn link(shown: &str) -> Option<String> {
    let rest = &shown[shown.find("https://")?..];
    let end = rest.find(|c: char| {
        !(c.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(c))
    })?;
    Some(rest[..end].to_owned())
}

/// The rest of the first line of `shown` holding `after`, trimmed, once a line follows it.
pub fn after(shown: &str, after: &str) -> Option<String> {
    let start = shown.find(after)? + after.len();
    let rest = &shown[start..];
    let end = rest.find(['\r', '\n'])?;
    let found = rest[..end].trim();
    (!found.is_empty()).then(|| found.to_owned())
}

/// The login's own last line after the first `from` bytes it printed: one it had not
/// printed before them (its prompt, drawn again), and not `echo`, what its terminal echoed.
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
                // A control sequence ends at its final byte. It reads as a space: a cursor
                // move or an erase stands between what is printed before and after it.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            break;
                        }
                    }
                    shown.push(' ');
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

/// A new terminal: the side the driver reads and writes, and the side the login is given.
/// Both are opened close-on-exec from the start, so nothing else the driver starts meanwhile
/// inherits either and keeps the terminal open: `/dev/ptmx` and the side it names, through
/// `std::fs`, which opens every file `O_CLOEXEC`, on macOS and Linux alike (`posix_openpt`
/// is `open("/dev/ptmx")` on both, and macOS's takes only `O_RDWR` and `O_NOCTTY`). Wide, so
/// a link is printed on one line.
fn terminal() -> std::io::Result<(File, OwnedFd)> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let open = |path: &Path| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(path)
    };
    let ours = open(Path::new("/dev/ptmx"))?;
    let fd = ours.as_raw_fd();
    // SAFETY: `grantpt` and `unlockpt` on the primary side this function owns.
    if unsafe { libc::grantpt(fd) } != 0 || unsafe { libc::unlockpt(fd) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut name = [0 as libc::c_char; 128];
    // SAFETY: `ptsname_r` writes at most `name.len()` bytes, its closing NUL among them.
    match unsafe { ptsname_r(fd, name.as_mut_ptr(), name.len()) } {
        0 => {}
        // macOS says -1 and sets `errno`; Linux answers the error number itself.
        -1 => return Err(std::io::Error::last_os_error()),
        error => return Err(std::io::Error::from_raw_os_error(error)),
    }
    // SAFETY: `ptsname_r` wrote a NUL-terminated name into `name`.
    let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) };
    let theirs = open(Path::new(std::ffi::OsStr::from_bytes(name.to_bytes())))?;
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 1000,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `TIOCSWINSZ` reads one `winsize`. The cast meets each platform's type of the
    // request.
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &raw const size) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((ours, OwnedFd::from(theirs)))
}

// The libc crate names no `ptsname_r` on macOS, which has it since 10.13.4 (ptsname(3)).
#[cfg(target_vendor = "apple")]
unsafe extern "C" {
    fn ptsname_r(fd: libc::c_int, buffer: *mut libc::c_char, length: libc::size_t) -> libc::c_int;
}
#[cfg(not(target_vendor = "apple"))]
use libc::ptsname_r;

fn gone(line: impl Into<String>) -> Error {
    Error::refused(refusal::NOT_FOUND, line)
}

fn refused(line: impl Into<String>) -> Error {
    Error::refused(refusal::REFUSED, line)
}
