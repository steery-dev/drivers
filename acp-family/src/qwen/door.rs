//! Qwen Code's daemon, `qwen serve`, as a door: where its records say it listens, and HTTP/1.1
//! with JSON over a loopback port. Each daemon writes `daemon-<port>.json` under
//! `$QWEN_RUNTIME_DIR/daemon`, else `<root>/daemon` (`serve/daemon-discovery.ts:79`). The
//! record never holds a token: a daemon with `--require-auth` takes one from
//! `daemon-<port>-<pid>.token` beside it (`qwen daemon start --save-token`), else from
//! `QWEN_SERVER_TOKEN`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// How long a daemon has to answer one request.
const WITHIN: Duration = Duration::from_secs(8);
/// The longest answer, and the longest event, the driver reads.
const BODY_AT_MOST: usize = 8 * 1024 * 1024;

/// One daemon that a record names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Daemon {
    pub host: String,
    pub port: u16,
    pub token: Option<String>,
    /// The workspace that `qwen attach` reaches without `--workspace`.
    pub primary: Option<String>,
}

/// The folder of the daemons' records.
pub fn records(home: &Path, root: &Path) -> PathBuf {
    let base = std::env::var("QWEN_RUNTIME_DIR")
        .ok()
        .filter(|dir| !dir.is_empty())
        .map(|dir| match dir.strip_prefix('~') {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(dir),
        });
    base.unwrap_or_else(|| root.to_path_buf()).join("daemon")
}

/// The daemons that the records name, lowest port first. A record of a daemon that ended
/// stays until another starts on its port, so a caller learns which still answer.
pub fn daemons(home: &Path, root: &Path) -> Vec<Daemon> {
    let dir = records(home, root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut daemons: Vec<Daemon> = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("daemon-") && name.ends_with(".json")
        })
        .filter_map(|entry| {
            let record: Value = serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
            daemon(&dir, &record)
        })
        .collect();
    daemons.sort_by_key(|daemon| daemon.port);
    daemons
}

fn daemon(dir: &Path, record: &Value) -> Option<Daemon> {
    if record["v"].as_u64() != Some(1) {
        return None;
    }
    let port = u16::try_from(record["port"].as_u64()?).ok()?;
    let host = match record["hostname"].as_str().unwrap_or("127.0.0.1") {
        "0.0.0.0" | "" => "127.0.0.1".to_owned(),
        "::" => "::1".to_owned(),
        host => host.to_owned(),
    };
    let token = match record["auth"].as_str() {
        Some("bearer") => record["pid"]
            .as_u64()
            .and_then(|pid| {
                std::fs::read_to_string(dir.join(format!("daemon-{port}-{pid}.token"))).ok()
            })
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty())
            .or_else(|| {
                std::env::var("QWEN_SERVER_TOKEN")
                    .ok()
                    .filter(|t| !t.is_empty())
            }),
        _ => None,
    };
    Some(Daemon {
        host,
        port,
        token,
        primary: record["primaryWorkspace"].as_str().map(str::to_owned),
    })
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

    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The daemon's words for a refused request: its `error`, else the body.
    pub fn failure(&self) -> String {
        self.json()
            .and_then(|body| body["error"].as_str().map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(&self.body).trim().to_owned())
    }
}

impl Daemon {
    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    fn head(&self, method: &str, path: &str, extra: &str) -> String {
        let auth = self
            .token
            .as_ref()
            .map(|token| format!("Authorization: Bearer {token}\r\n"))
            .unwrap_or_default();
        format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\n{auth}{extra}\r\n",
            self.host_header()
        )
    }

    async fn connect(&self) -> Result<TcpStream, String> {
        TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn get(&self, path: &str) -> Result<Answer, String> {
        self.request("GET", path, None).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Answer, String> {
        self.request("POST", path, Some(body)).await
    }

    /// One request and its whole answer.
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Answer, String> {
        tokio::time::timeout(WITHIN, self.exchange(method, path, body))
            .await
            .map_err(|_| format!("Qwen Code's daemon did not answer `{method} {path}`"))?
    }

    async fn exchange(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Answer, String> {
        let mut stream = self.connect().await?;
        let body = body.map(Value::to_string).unwrap_or_default();
        let head = self.head(
            method,
            path,
            &format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            ),
        );
        stream
            .write_all(format!("{head}{body}").as_bytes())
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

    /// The server-sent events of `path`: each `data` line's JSON to `each`, until the stream
    /// ends. `opened` runs once the daemon answered 200.
    pub async fn events(
        &self,
        path: &str,
        opened: impl FnOnce(),
        mut each: impl FnMut(Value),
    ) -> Result<(), String> {
        let mut stream = self.connect().await?;
        let head = self.head("GET", path, "Accept: text/event-stream\r\n");
        stream
            .write_all(head.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(stream);
        let (status, chunked) = tokio::time::timeout(WITHIN, head_of(&mut reader))
            .await
            .map_err(|_| "Qwen Code's daemon did not open its events".to_owned())??;
        if status != 200 {
            return Err(format!(
                "Qwen Code's daemon answered {status} for its events"
            ));
        }
        opened();
        let mut pending = Vec::new();
        loop {
            if chunked {
                let size = line(&mut reader).await?;
                let size = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16)
                    .map_err(|_| "a chunk of the daemon's events has no size".to_owned())?;
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
                return Err("an event of Qwen Code's daemon is too long".into());
            }
        }
    }
}

/// The status line and headers: the status, and whether the body comes in chunks.
async fn head_of(reader: &mut BufReader<TcpStream>) -> Result<(u16, bool), String> {
    let status_line = line(reader).await?;
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("Qwen Code's daemon answered `{status_line}`"))?;
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

async fn line(reader: &mut BufReader<TcpStream>) -> Result<String, String> {
    let mut text = Vec::new();
    let read = (&mut *reader)
        .take(64 * 1024)
        .read_until(b'\n', &mut text)
        .await
        .map_err(|e| e.to_string())?;
    if read == 0 {
        return Err("Qwen Code's daemon closed the connection".into());
    }
    Ok(String::from_utf8_lossy(&text)
        .trim_end_matches(['\r', '\n'])
        .to_owned())
}

/// A chunked body, joined.
fn unchunk(mut raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let end = raw
            .windows(2)
            .position(|pair| pair == b"\r\n")
            .ok_or("a chunk of the daemon's answer has no size")?;
        let size = std::str::from_utf8(&raw[..end])
            .ok()
            .and_then(|size| usize::from_str_radix(size.split(';').next()?.trim(), 16).ok())
            .ok_or("a chunk of the daemon's answer has no size")?;
        raw = &raw[end + 2..];
        if size == 0 {
            return Ok(body);
        }
        let chunk = raw
            .get(..size)
            .ok_or("the daemon's answer ended inside a chunk")?;
        body.extend_from_slice(chunk);
        raw = raw.get(size + 2..).unwrap_or_default();
    }
}
