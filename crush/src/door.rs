//! HTTP/1.1 over the Unix socket of Crush's server (`crush server`, the process that a TUI
//! opened with `CRUSH_CLIENT_SERVER=1` starts and shares): one request per connection, and
//! the server-sent events of a workspace.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// How long the server has to answer one request.
const WITHIN: Duration = Duration::from_secs(10);
/// How long the server has to list its workspaces for a read of the stores.
const LISTED_WITHIN: Duration = Duration::from_secs(1);
/// The longest socket path Crush binds before it falls back to `/tmp` (`maxUnixSocketPathLen`).
const PATH_AT_MOST: usize = 104;
/// The longest answer or event line the driver reads.
const BODY_AT_MOST: usize = 64 * 1024 * 1024;

/// The socket of the person's Crush server, as `server.DefaultHost` names it:
/// `crush-<uid>.sock` in `$XDG_RUNTIME_DIR`, else the temporary folder, else `/tmp` where that
/// path is too long to bind. The person owns their home, so its owner is their uid.
pub fn socket(home: &Path) -> Option<PathBuf> {
    let uid = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(home).ok()?);
    let name = format!("crush-{uid}.sock");
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map_or_else(temp_dir, PathBuf::from);
    let path = dir.join(&name);
    if path.as_os_str().len() > PATH_AT_MOST {
        return Some(Path::new("/tmp").join(name));
    }
    Some(path)
}

/// Go's `os.TempDir`: `$TMPDIR`, else `/tmp`. On a Mac a process that the runtime starts can
/// lack `TMPDIR`, so the driver asks the system for the person's own temporary folder, which
/// is what `TMPDIR` names in their terminal.
fn temp_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TMPDIR").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    static ASKED: OnceLock<PathBuf> = OnceLock::new();
    ASKED.get_or_init(system_temp_dir).clone()
}

fn system_temp_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    if let Some(dir) = std::process::Command::new("/usr/bin/getconf")
        .arg("DARWIN_USER_TEMP_DIR")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|dir| dir.trim().to_owned())
        .filter(|dir| !dir.is_empty())
    {
        return PathBuf::from(dir);
    }
    PathBuf::from("/tmp")
}

/// The projects that the person's Crush server has open, each its folder and its store's
/// folder. Crush 0.100.0 lists in `projects.json` only a project that a TUI opened in its
/// own process (`setupLocalWorkspace`), never one that the server opened. None where no
/// server runs or it does not answer within a second.
pub fn open_projects(home: &Path) -> Vec<(PathBuf, PathBuf)> {
    let Some(listed) = socket(home).and_then(|socket| workspaces(&socket)) else {
        return Vec::new();
    };
    listed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|workspace| {
            let path = PathBuf::from(workspace["path"].as_str()?);
            let data_dir = PathBuf::from(workspace["data_dir"].as_str()?);
            (path.is_absolute() && data_dir.is_absolute()).then_some((path, data_dir))
        })
        .collect()
}

/// `GET /v1/workspaces`, blocking: the stores are read outside the runtime's tasks.
fn workspaces(socket: &Path) -> Option<Value> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(LISTED_WITHIN)).ok()?;
    stream.set_write_timeout(Some(LISTED_WITHIN)).ok()?;
    stream
        .write_all(b"GET /v1/workspaces HTTP/1.1\r\nHost: crush\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut raw = Vec::new();
    stream
        .take(BODY_AT_MOST as u64)
        .read_to_end(&mut raw)
        .ok()?;
    let end = raw.windows(4).position(|four| four == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..end]).ok()?;
    if head.split(' ').nth(1) != Some("200") {
        return None;
    }
    let chunked = head.lines().any(|header| {
        header.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value.trim().eq_ignore_ascii_case("chunked")
        })
    });
    let body = &raw[end + 4..];
    let body = if chunked {
        unchunk(body).ok()?
    } else {
        body.to_vec()
    };
    serde_json::from_slice(&body).ok()
}

/// One answer: its status and its body.
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }

    /// The server's words for a refused request: its `message`, else the body.
    pub fn failure(&self) -> String {
        self.json()
            .and_then(|body| body["message"].as_str().map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(&self.body).trim().to_owned())
    }
}

/// One request to the server and its whole answer.
pub async fn request(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Answer, String> {
    tokio::time::timeout(WITHIN, exchange(socket, method, path, body))
        .await
        .map_err(|_| format!("Crush's server did not answer `{method} {path}`"))?
}

async fn exchange(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<Answer, String> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| e.to_string())?;
    let body = body.map(Value::to_string).unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: crush\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    stream
        .write_all(body.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let (status, chunked) = head_of(&mut reader).await?;
    let mut raw = Vec::new();
    (&mut reader)
        .take(BODY_AT_MOST as u64)
        .read_to_end(&mut raw)
        .await
        .map_err(|e| e.to_string())?;
    let body = if chunked { unchunk(&raw)? } else { raw };
    Ok(Answer { status, body })
}

/// The status line and headers: the status, and whether the body comes in chunks.
async fn head_of(reader: &mut BufReader<UnixStream>) -> Result<(u16, bool), String> {
    let status_line = line(reader).await?;
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("Crush's server answered `{status_line}`"))?;
    let mut chunked = false;
    loop {
        let header = line(reader).await?;
        if header.is_empty() {
            return Ok((status, chunked));
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("transfer-encoding")
            && value.trim().eq_ignore_ascii_case("chunked")
        {
            chunked = true;
        }
    }
}

async fn line(reader: &mut BufReader<UnixStream>) -> Result<String, String> {
    let mut text = String::new();
    let read = reader
        .read_line(&mut text)
        .await
        .map_err(|e| e.to_string())?;
    if read == 0 {
        return Err("Crush's server closed the connection".into());
    }
    Ok(text.trim_end_matches(['\r', '\n']).to_owned())
}

/// A chunked body, joined.
fn unchunk(mut raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let end = raw
            .windows(2)
            .position(|pair| pair == b"\r\n")
            .ok_or("a chunk of Crush's answer has no size")?;
        let size = std::str::from_utf8(&raw[..end])
            .ok()
            .and_then(|size| usize::from_str_radix(size.split(';').next()?.trim(), 16).ok())
            .ok_or("a chunk of Crush's answer has no size")?;
        raw = &raw[end + 2..];
        if size == 0 {
            return Ok(body);
        }
        let chunk = raw
            .get(..size)
            .ok_or("Crush's answer ended inside a chunk")?;
        body.extend_from_slice(chunk);
        raw = raw.get(size + 2..).unwrap_or_default();
    }
}

/// The server-sent events of `path`: each `data` line's JSON, to `each`, until the stream
/// ends. An error where the server refused the stream.
pub async fn events(socket: &Path, path: &str, mut each: impl FnMut(Value)) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| e.to_string())?;
    let head = format!("GET {path} HTTP/1.1\r\nHost: crush\r\nAccept: text/event-stream\r\n\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let (status, chunked) = tokio::time::timeout(WITHIN, head_of(&mut reader))
        .await
        .map_err(|_| "Crush's server did not open its events".to_owned())??;
    if status != 200 {
        return Err(format!("Crush's server answered {status} for its events"));
    }
    let mut pending = Vec::new();
    loop {
        if chunked {
            let size = line(&mut reader).await?;
            let size = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| "a chunk of Crush's events has no size".to_owned())?;
            if size == 0 {
                return Ok(());
            }
            let mut chunk = vec![0u8; size + 2];
            reader
                .read_exact(&mut chunk)
                .await
                .map_err(|e| e.to_string())?;
            chunk.truncate(size);
            pending.extend_from_slice(&chunk);
        } else {
            let mut more = vec![0u8; 64 * 1024];
            let read = reader.read(&mut more).await.map_err(|e| e.to_string())?;
            if read == 0 {
                return Ok(());
            }
            pending.extend_from_slice(&more[..read]);
        }
        while let Some(end) = pending.iter().position(|&byte| byte == b'\n') {
            let text: Vec<u8> = pending.drain(..=end).collect();
            if let Some(data) = text.strip_prefix(b"data:")
                && let Ok(value) = serde_json::from_slice::<Value>(data.trim_ascii())
            {
                each(value);
            }
        }
        if pending.len() > BODY_AT_MOST {
            return Err("an event of Crush's server is too long".into());
        }
    }
}

/// A random version-4 UUID, as Crush's server wants a client's id.
pub fn uuid() -> String {
    let mut bytes = [0u8; 16];
    let read = std::fs::File::open("/dev/urandom")
        .and_then(|mut random| std::io::Read::read_exact(&mut random, &mut bytes));
    if read.is_err() {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
            ^ (u128::from(std::process::id()) << 64);
        bytes = seed.to_le_bytes();
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchunks_a_body() {
        let body = unchunk(b"4\r\n[1,2\r\n2\r\n,3\r\n1\r\n]\r\n0\r\n\r\n").unwrap();
        assert_eq!(body, b"[1,2,3]");
    }
}
