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
    let status = ask(binary, "auth.getStatus", json!({})).await?;
    status.get("isAuthenticated").and_then(Value::as_bool)
}

/// One request's result, or none where the server did not answer it within [`WITHIN`].
async fn ask(binary: &Path, method: &str, params: Value) -> Option<Value> {
    let mut command = helper(binary);
    command
        .args(["--headless", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = command.spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let mut stdout = BufReader::new(child.stdout.take()?);
    let asked = async {
        let body =
            json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        stdin
            .write_all(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
            .await
            .ok()?;
        stdin.flush().await.ok()?;
        // Notifications and requests of the server's own may come first.
        loop {
            let frame = frame(&mut stdout).await?;
            if frame.get("id") == Some(&json!(1)) && frame.get("method").is_none() {
                return frame.get("result").cloned();
            }
        }
    };
    let result = tokio::time::timeout(WITHIN, asked).await.ok().flatten();
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
