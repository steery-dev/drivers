//! A minimal client of Copilot's own server, `copilot --headless --stdio`: JSON-RPC 2.0 in
//! `Content-Length` frames, as Copilot's SDK speaks it. The server answers a request with no
//! handshake first (1.0.108). The driver asks only what costs no model turn.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use steery_driver_common::program::helper;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// How long the server has to start and answer.
const WITHIN: Duration = Duration::from_secs(10);
/// The largest frame the driver reads.
const FRAME_AT_MOST: usize = 1 << 20;

/// Whether Copilot is signed in, by its server's `auth.getStatus`; none where it did not
/// answer.
pub async fn signed_in(binary: &Path) -> Option<bool> {
    let status = ask(binary, "auth.getStatus", json!({})).await.ok()?;
    status.get("isAuthenticated").and_then(Value::as_bool)
}

/// `models.list`: the models Copilot offers this account, as its server lists them.
pub async fn models(binary: &Path) -> Result<Vec<Value>, String> {
    let listed = ask(binary, "models.list", json!({})).await?;
    match listed.get("models") {
        Some(Value::Array(models)) => Ok(models.clone()),
        _ => Err("Copilot's models.list answered no list".into()),
    }
}

/// `sessions.fork`: a new conversation with the whole persisted history of `id`, and the
/// new conversation's id. It runs no model turn.
pub async fn fork(binary: &Path, id: &str) -> Result<String, String> {
    let forked = ask(binary, "sessions.fork", json!({"sessionId": id})).await?;
    forked
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "Copilot's sessions.fork named no new session".into())
}

/// `sessions.delete`: Copilot removes the conversation `id` from disk, through its own
/// lifecycle.
pub async fn delete(binary: &Path, id: &str) -> Result<(), String> {
    ask(binary, "sessions.delete", json!({"sessionId": id}))
        .await
        .map(drop)
}

/// One request's result; else the server's error, or why it did not answer within
/// [`WITHIN`].
async fn ask(binary: &Path, method: &str, params: Value) -> Result<Value, String> {
    let mut command = helper(binary);
    command
        .args(["--headless", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start `copilot --headless`: {e}"))?;
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err("`copilot --headless` has no standard input or output".into());
    };
    let mut stdout = BufReader::new(stdout);
    let gone = || format!("`copilot --headless` ended before it answered {method}");
    let asked = async {
        let body =
            json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        stdin
            .write_all(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
            .await
            .map_err(|_| gone())?;
        stdin.flush().await.map_err(|_| gone())?;
        // Notifications and requests of the server's own may come first.
        loop {
            let frame = frame(&mut stdout).await.ok_or_else(gone)?;
            if frame.get("id") != Some(&json!(1)) || frame.get("method").is_some() {
                continue;
            }
            return match (frame.get("result"), frame.pointer("/error/message")) {
                (Some(result), _) => Ok(result.clone()),
                (None, Some(Value::String(error))) => Err(format!("Copilot: {error}")),
                _ => Err(format!(
                    "Copilot answered {method} with neither a result nor an error"
                )),
            };
        }
    };
    let result = tokio::time::timeout(WITHIN, asked)
        .await
        .unwrap_or_else(|_| Err(format!("Copilot did not answer {method} within 10 s")));
    let _ = child.kill().await;
    result
}

/// The next frame's JSON; none at the end of the stream or a frame that is not one.
async fn frame(stdout: &mut (impl AsyncBufReadExt + Unpin)) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if stdout.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let length = length.filter(|length| *length <= FRAME_AT_MOST)?;
    let mut body = vec![0; length];
    stdout.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}
