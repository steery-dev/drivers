//! A one-shot client of Copilot's own server, `copilot --headless --stdio`: one process per
//! request, over [`super::rpc`]. The server answers a request with no handshake first
//! (1.0.108). The driver asks only what costs no model turn.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use steery_driver_common::program::helper;

use super::rpc;

/// How long the server has to start and answer.
const WITHIN: Duration = Duration::from_secs(10);

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
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start `copilot --headless`: {e}"))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err("`copilot --headless` has no standard input or output".into());
    };
    let (client, _notes) = rpc::open(stdout, stdin);
    let result = client.ask(method, params, WITHIN).await;
    let _ = child.kill().await;
    result
}
