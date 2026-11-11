//! An Antigravity window that Steery can reach. Each interactive `agy` runs its own language
//! server, which serves Connect (HTTP/1.1, JSON) on a loopback port. The server takes a
//! request only with its CSRF token, which the window takes from `TOKEN` in its environment
//! as it starts; without it the token is random and stays inside the window. So a window has
//! a door only where it started with `TOKEN`, as the ones Steery opens do
//! ([`super::drive::terminal`]), and the driver reads the token from the window's
//! environment. The window shows the conversation whose presence lock it holds open.
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// The variable that gives an Antigravity window its server's CSRF token.
pub(crate) const TOKEN: &str = "ANTIGRAVITY_SERVER_CSRF_TOKEN";

/// The service of the window's language server.
const SERVICE: &str = "exa.language_server_pb.LanguageServerService";

/// How long a window has to answer a request.
pub const WITHIN: Duration = Duration::from_secs(10);

/// The longest header line, and the longest message, the driver reads.
const LINE_AT_MOST: usize = 16 * 1024;
const MESSAGE_AT_MOST: usize = 64 * 1024 * 1024;

/// A window's server: its process, its HTTP port and its token.
#[derive(Clone)]
pub struct Window {
    pub pid: u32,
    pub port: u16,
    token: String,
}

/// The window that holds `lock`, the presence lock of a conversation, open; else why no
/// door reaches the conversation.
pub async fn find(lock: &Path) -> Result<Window, String> {
    let file = std::fs::metadata(lock)
        .map(|meta| file_id(&meta))
        .map_err(|_| "Antigravity runs no window of this conversation".to_owned())?;
    let found = tokio::task::spawn_blocking(move || {
        let ports = ports();
        let mut pids: Vec<u32> = ports.iter().map(|(pid, _)| *pid).collect();
        pids.dedup();
        let pid = pids.into_iter().find(|pid| holds(*pid, file))?;
        let mine: Vec<u16> = ports
            .iter()
            .filter(|(of, _)| *of == pid)
            .map(|(_, port)| *port)
            .collect();
        Some((pid, mine, environment(pid, TOKEN)))
    })
    .await
    .ok()
    .flatten();
    let Some((pid, ports, token)) = found else {
        return Err("no Antigravity window with its own server shows this conversation".into());
    };
    let Some(token) = token else {
        return Err(format!(
            "the Antigravity window (pid {pid}) started without `{TOKEN}`, so its server takes \
             no request from outside it; open the conversation with Steery's terminal command"
        ));
    };
    // The window serves gRPC over TLS on one port and Connect on the other.
    for port in ports {
        let window = Window {
            pid,
            port,
            token: token.clone(),
        };
        if window
            .call("Heartbeat", &serde_json::json!({}))
            .await
            .is_ok()
        {
            return Ok(window);
        }
    }
    Err(format!(
        "the Antigravity window (pid {pid}) did not answer on its ports"
    ))
}

impl Window {
    /// One unary call: its answer, or what went wrong.
    pub async fn call(&self, method: &str, body: &Value) -> Result<Value, String> {
        let body = serde_json::to_vec(body).map_err(|e| e.to_string())?;
        let ask = async {
            let mut reply = self.post(method, "application/json", &body).await?;
            let mut bytes = Vec::new();
            loop {
                let mut piece = [0u8; 8192];
                let read = reply.read(&mut piece).await?;
                if read == 0 {
                    break;
                }
                if bytes.len() + read > MESSAGE_AT_MOST {
                    return Err("the window's answer is too long".to_owned());
                }
                bytes.extend_from_slice(&piece[..read]);
            }
            let answer: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            if reply.status != 200 {
                return Err(error(&answer, reply.status));
            }
            Ok(answer)
        };
        tokio::time::timeout(WITHIN, ask)
            .await
            .map_err(|_| format!("the Antigravity window did not answer `{method}`"))?
    }

    /// A server stream: the window's messages, one at a time.
    pub async fn stream(&self, method: &str, body: &Value) -> Result<Stream, String> {
        let json = serde_json::to_vec(body).map_err(|e| e.to_string())?;
        let mut framed = Vec::with_capacity(json.len() + 5);
        framed.push(0);
        framed.extend_from_slice(&u32::try_from(json.len()).unwrap_or(u32::MAX).to_be_bytes());
        framed.extend_from_slice(&json);
        let reply = tokio::time::timeout(
            WITHIN,
            self.post(method, "application/connect+json", &framed),
        )
        .await
        .map_err(|_| format!("the Antigravity window did not answer `{method}`"))??;
        if reply.status != 200 {
            return Err(format!("the Antigravity window answered {}", reply.status));
        }
        Ok(Stream { body: reply })
    }

    async fn post(&self, method: &str, kind: &str, body: &[u8]) -> Result<Body, String> {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port))
            .await
            .map_err(|e| e.to_string())?;
        let head = format!(
            "POST /{SERVICE}/{method} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\
             Content-Type: {kind}\r\nConnect-Protocol-Version: 1\r\n\
             x-codeium-csrf-token: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.port,
            self.token,
            body.len()
        );
        stream
            .write_all(head.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        stream.write_all(body).await.map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(stream);
        let status_line = line(&mut reader).await?;
        let status = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .ok_or_else(|| format!("the window answered `{status_line}`"))?;
        let mut chunked = false;
        let mut length = None;
        loop {
            let header = line(&mut reader).await?;
            if header.is_empty() {
                break;
            }
            let Some((name, value)) = header.split_once(':') else {
                continue;
            };
            let value = value.trim();
            if name.eq_ignore_ascii_case("transfer-encoding") {
                chunked = value.eq_ignore_ascii_case("chunked");
            } else if name.eq_ignore_ascii_case("content-length") {
                length = value.parse::<usize>().ok();
            }
        }
        Ok(Body {
            reader,
            status,
            chunked,
            left: if chunked {
                0
            } else {
                length.unwrap_or(usize::MAX)
            },
            done: !chunked && length == Some(0),
        })
    }
}

/// The body of one answer, read past its chunking.
pub struct Body {
    reader: BufReader<TcpStream>,
    status: u16,
    chunked: bool,
    /// What is left of the chunk, or of the body where it is not chunked.
    left: usize,
    done: bool,
}

impl Body {
    /// Bytes of the body into `into`: 0 where it ended.
    async fn read(&mut self, into: &mut [u8]) -> Result<usize, String> {
        if self.done || into.is_empty() {
            return Ok(0);
        }
        if self.left == 0 && self.chunked {
            let size = line(&mut self.reader).await?;
            let size = size.split(';').next().unwrap_or_default().trim();
            self.left = usize::from_str_radix(size, 16)
                .map_err(|_| format!("the window sent a chunk of size `{size}`"))?;
            if self.left == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let most = into.len().min(self.left);
        let read = self
            .reader
            .read(&mut into[..most])
            .await
            .map_err(|e| e.to_string())?;
        if read == 0 {
            self.done = true;
            return Ok(0);
        }
        self.left -= read;
        if self.left == 0 && self.chunked {
            line(&mut self.reader).await?;
        }
        Ok(read)
    }

    async fn exact(&mut self, into: &mut [u8]) -> Result<bool, String> {
        let mut at = 0;
        while at < into.len() {
            let read = self.read(&mut into[at..]).await?;
            if read == 0 {
                return Ok(false);
            }
            at += read;
        }
        Ok(true)
    }
}

/// The messages of a server stream.
pub struct Stream {
    body: Body,
}

impl Stream {
    /// The next message; none where the stream ended, an error where it ended on one.
    pub async fn next(&mut self) -> Result<Option<Value>, String> {
        let mut head = [0u8; 5];
        if !self.body.exact(&mut head).await? {
            return Ok(None);
        }
        let length = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        if length > MESSAGE_AT_MOST {
            return Err("the window sent a message too long to read".to_owned());
        }
        let mut message = vec![0u8; length];
        if !self.body.exact(&mut message).await? {
            return Ok(None);
        }
        let message: Value = serde_json::from_slice(&message).map_err(|e| e.to_string())?;
        // Flag 2 is the end of the stream, whose message carries its error, if any.
        if head[0] & 2 != 0 {
            return match message.get("error") {
                Some(error) => Err(self::error(error, 200)),
                None => Ok(None),
            };
        }
        Ok(Some(message))
    }
}

/// The text of a Connect error.
fn error(answer: &Value, status: u16) -> String {
    answer
        .get("message")
        .and_then(Value::as_str)
        .map_or_else(|| format!("the window answered {status}"), str::to_owned)
}

async fn line(reader: &mut BufReader<TcpStream>) -> Result<String, String> {
    let mut bytes = Vec::new();
    let read = (&mut *reader)
        .take(LINE_AT_MOST as u64)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(|e| e.to_string())?;
    if read == 0 {
        return Err("the window closed the connection".to_owned());
    }
    Ok(String::from_utf8_lossy(&bytes).trim_end().to_owned())
}

/// The loopback TCP ports each process listens on.
fn ports() -> Vec<(u32, u16)> {
    let Ok(listening) = listeners::get_all() else {
        return Vec::new();
    };
    let mut ports: Vec<(u32, u16)> = listening
        .into_iter()
        .filter(|l| {
            l.protocol == listeners::Protocol::TCP
                && l.state == listeners::SocketState::Listen
                && l.socket.ip().is_loopback()
        })
        .map(|l| (l.process.pid, l.socket.port()))
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// A file as its device and inode.
fn file_id(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt as _;
    (meta.dev(), meta.ino())
}

/// Whether process `pid` holds the file `file` open.
#[cfg(target_os = "macos")]
fn holds(pid: u32, file: (u64, u64)) -> bool {
    /// `proc_fileinfo` then `vnode_info`, as `PROC_PIDFDVNODEINFO` fills them.
    #[repr(C)]
    struct VnodeFd {
        file: [u64; 3],
        vnode: libc::vnode_info,
    }
    const PROC_PIDFDVNODEINFO: libc::c_int = 1;
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer asks only for the size of the list.
    let size =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    let Ok(size) = usize::try_from(size) else {
        return false;
    };
    // Room for descriptors opened meanwhile.
    let mut fds: Vec<libc::proc_fdinfo> = Vec::with_capacity(size / entry + 16);
    let Ok(room) = i32::try_from(fds.capacity() * entry) else {
        return false;
    };
    // SAFETY: the buffer has room for `room` bytes of `proc_fdinfo`.
    let read =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), room) };
    let Ok(read) = usize::try_from(read) else {
        return false;
    };
    // SAFETY: the kernel filled `read` bytes, whole entries, within the capacity.
    unsafe { fds.set_len((read / entry).min(fds.capacity())) };
    let wanted = i32::try_from(std::mem::size_of::<VnodeFd>()).unwrap_or(i32::MAX);
    fds.iter()
        .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        .any(|fd| {
            // SAFETY: an all-zero `VnodeFd` is a valid value of plain C structs.
            let mut info: VnodeFd = unsafe { std::mem::zeroed() };
            // SAFETY: the buffer is one `VnodeFd`, and `wanted` is its size.
            let got = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDVNODEINFO,
                    (&raw mut info).cast(),
                    wanted,
                )
            };
            got == wanted
                && (
                    info.vnode.vi_stat.vst_dev as u64,
                    info.vnode.vi_stat.vst_ino,
                ) == file
        })
}

/// Whether process `pid` holds the file `file` open.
#[cfg(not(target_os = "macos"))]
fn holds(pid: u32, file: (u64, u64)) -> bool {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    fds.flatten()
        .any(|fd| std::fs::metadata(fd.path()).is_ok_and(|meta| file_id(&meta) == file))
}

/// The value of `name` in the environment process `pid` started with.
#[cfg(target_os = "macos")]
fn environment(pid: u32, name: &str) -> Option<String> {
    let pid = libc::c_int::try_from(pid).ok()?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: a null buffer asks only for the size.
    let asked = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if asked != 0 || size == 0 {
        return None;
    }
    let mut bytes = vec![0u8; size];
    // SAFETY: the buffer has `size` bytes.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    bytes.truncate(size);
    // `argc`, the executable's path, padding, `argc` arguments, then the environment.
    let argc = usize::try_from(i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?)).ok()?;
    let mut words = bytes[4..]
        .split(|b| *b == 0)
        .filter(|word| !word.is_empty());
    words.next()?;
    let mut words = words.skip(argc);
    let prefix = format!("{name}=");
    words.find_map(|word| {
        word.strip_prefix(prefix.as_bytes())
            .map(|value| String::from_utf8_lossy(value).into_owned())
    })
}

/// The value of `name` in the environment process `pid` started with.
#[cfg(not(target_os = "macos"))]
fn environment(pid: u32, name: &str) -> Option<String> {
    let bytes = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    let prefix = format!("{name}=");
    bytes.split(|b| *b == 0).find_map(|word| {
        word.strip_prefix(prefix.as_bytes())
            .map(|value| String::from_utf8_lossy(value).into_owned())
    })
}
