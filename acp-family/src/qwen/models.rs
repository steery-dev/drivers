//! `models`: the models of the session that `qwen --acp` opens, the current one first. Qwen
//! has no command that lists them (0.27.2); its ACP server lists, in `session/new`'s
//! `models.availableModels`, every model of the person's `modelProviders` and the one its
//! settings and environment run. The session is never prompted, so Qwen writes no chat file
//! of it. Where Qwen asks for a sign-in first, it lists nothing and says so.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use steery_driver_common::program::helper;
use steery_sdk::wire::harness::{Model, Models};
use steery_sdk::{Error, refusal};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// How long `qwen --acp` has to open a session that lists Qwen's models. It loads its
/// extensions and MCP servers first: 6 s on the machine that recorded the fixtures.
const LISTED_WITHIN: Duration = Duration::from_secs(30);

/// The ACP error of a session that Qwen opens only after a sign-in (`acpAgent`).
const AUTH_REQUIRED: &str = "Authentication required";

/// `models`, where Qwen's root is `root`.
pub async fn list(root: &Path) -> Result<Models, Error> {
    let qwen = super::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "qwen is not installed"))?;
    let watch = vec![root.join("settings.json")];
    let opened = tokio::time::timeout(LISTED_WITHIN, open(&qwen))
        .await
        .map_err(|_| {
            Error::refused(
                refusal::REFUSED,
                format!(
                    "`qwen --acp` opened no session within {} s",
                    LISTED_WITHIN.as_secs()
                ),
            )
        })??;
    if let Some(error) = opened.get("error") {
        let said = error["message"].as_str().unwrap_or("refused");
        if said.starts_with(AUTH_REQUIRED) {
            return Ok(Models {
                models: Vec::new(),
                unlisted: None,
                watch,
                signed_in: Some(false),
            });
        }
        return Err(Error::refused(
            refusal::REFUSED,
            format!("`qwen --acp` opened no session: {said}"),
        ));
    }
    Ok(Models {
        models: listed(&opened["result"]),
        unlisted: None,
        watch,
        signed_in: Some(true),
    })
}

/// Start `qwen --acp` in a folder of its own, initialize it, and ask for a new session;
/// Qwen's answer to that.
async fn open(qwen: &Path) -> Result<Value, Error> {
    let failed = |e: std::io::Error| {
        Error::refused(refusal::REFUSED, format!("`qwen --acp` did not run: {e}"))
    };
    let cwd = std::env::temp_dir().join(format!("steery-qwen-models-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).map_err(failed)?;
    let mut child = helper(qwen)
        .arg("--acp")
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(failed)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| failed(std::io::ErrorKind::BrokenPipe.into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failed(std::io::ErrorKind::BrokenPipe.into()))?;
    let mut lines = BufReader::new(stdout).lines();
    let requests = [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "session/new",
            "params": {"cwd": cwd.to_string_lossy(), "mcpServers": []}}),
    ];
    let mut answer = Value::Null;
    for request in requests {
        let id = request["id"].clone();
        stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .map_err(failed)?;
        answer = loop {
            let Some(line) = lines.next_line().await.map_err(failed)? else {
                return Err(Error::refused(
                    refusal::REFUSED,
                    "`qwen --acp` closed before it answered",
                ));
            };
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("method").is_none() && message["id"] == id {
                break message;
            }
        };
        if answer.get("error").is_some() {
            break;
        }
    }
    drop(stdin);
    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&cwd);
    Ok(answer)
}

/// The models of a session's answer, `currentModelId` first. A model's id there is
/// `<model>(<auth type>)`; `-m` takes its `_meta.baseModelId`, the name the chat file and
/// the turn's `init` line give it. Qwen offers no thinking levels.
fn listed(session: &Value) -> Vec<Model> {
    let current = session
        .pointer("/models/currentModelId")
        .and_then(Value::as_str);
    let mut first = None;
    let mut models: Vec<Model> = Vec::new();
    for listed in session
        .pointer("/models/availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(model) = listed
            .pointer("/_meta/baseModelId")
            .or_else(|| listed.get("modelId"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if models.iter().any(|known| known.model == model) {
            continue;
        }
        if current.is_some() && listed["modelId"].as_str() == current {
            first = Some(models.len());
        }
        models.push(Model {
            model: model.to_owned(),
            provider: None,
            label: listed["name"].as_str().map(str::to_owned),
            thinking: Vec::new(),
            cost: None,
        });
    }
    if let Some(at) = first {
        let current = models.remove(at);
        models.insert(0, current);
    }
    models
}
