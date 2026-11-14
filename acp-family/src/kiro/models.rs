//! `models`: the models of a session that `kiro-cli acp` opens. Its `model` config option
//! lists each model the person's plan runs, its default first, and under
//! `_meta.kiro.effortLevels` the effort levels of a model that has any (Kiro 3.3.0 names
//! that option `effortLevel`). The session is deleted again, so Kiro keeps no folder of it.
//! `kiro-cli chat --list-models` names the same models, but not their effort levels.
//!
//! Kiro sets any model a session is told, one it does not run too, so `start` refuses a
//! model this list lacks before anything runs ([`crate::turn::drive::offered`]).
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use steery_driver_common::program::{PROBE, helper};
use steery_sdk::wire::harness::{Model, Models};
use steery_sdk::{Error, refusal};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout};

use super::EXECUTABLE;

/// How long `kiro-cli acp` has to open a session that lists Kiro's models.
const LISTED_WITHIN: Duration = Duration::from_secs(30);

/// The config option of a session that picks its model.
const MODEL: &str = "model";

/// `models` for the person at `home`, with Kiro's folder `root`: none, signed out, where
/// `kiro-cli whoami` names no account.
pub async fn list(home: &Path, root: &Path) -> Result<Models, Error> {
    let binary = super::setup::binary(home)
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "Kiro CLI is not installed"))?;
    let watch = vec![root.join("settings").join("cli.json")];
    if !signed_in(&binary, home).await {
        return Ok(Models {
            models: Vec::new(),
            unlisted: None,
            watch,
            signed_in: Some(false),
        });
    }
    let opened = tokio::time::timeout(LISTED_WITHIN, open(&binary, home))
        .await
        .map_err(|_| {
            Error::refused(
                refusal::REFUSED,
                format!(
                    "`{EXECUTABLE} acp` opened no session within {} s",
                    LISTED_WITHIN.as_secs()
                ),
            )
        })??;
    if let Some(error) = opened.get("error") {
        let said = error["message"].as_str().unwrap_or("refused");
        return Err(Error::refused(
            refusal::REFUSED,
            format!("`{EXECUTABLE} acp` opened no session: {said}"),
        ));
    }
    let models = listed(&opened["result"]);
    if models.is_empty() {
        return Err(Error::refused(
            refusal::REFUSED,
            format!("`{EXECUTABLE} acp` listed no models"),
        ));
    }
    Ok(Models {
        models,
        unlisted: None,
        watch,
        signed_in: Some(true),
    })
}

/// Whether `kiro-cli whoami` names an account: it exits 1 where none is signed in.
async fn signed_in(binary: &Path, home: &Path) -> bool {
    let mut command = helper(binary);
    command
        .args(["whoami", "--format", "json"])
        .env("HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    matches!(
        tokio::time::timeout(PROBE, command.status()).await,
        Ok(Ok(status)) if status.success()
    )
}

/// Start `kiro-cli acp` in a folder of its own, initialize it, ask for a new session and
/// delete it again; Kiro's answer to the new session.
async fn open(binary: &Path, home: &Path) -> Result<Value, Error> {
    let cwd = std::env::temp_dir().join(format!("steery-kiro-models-{}", std::process::id()));
    std::fs::create_dir_all(&cwd).map_err(failed)?;
    let mut child = tokio::process::Command::new(binary)
        .arg("acp")
        .env("HOME", home)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
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
    let mut answer = ask(
        &mut stdin,
        &mut lines,
        1,
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    )
    .await?;
    if answer.get("error").is_none() {
        answer = ask(
            &mut stdin,
            &mut lines,
            2,
            "session/new",
            json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}),
        )
        .await?;
    }
    if let Some(session) = answer.pointer("/result/sessionId").cloned() {
        let deleted = ask(
            &mut stdin,
            &mut lines,
            3,
            "session/delete",
            json!({"sessionId": session}),
        )
        .await?;
        if let Some(error) = deleted.get("error") {
            return Err(Error::refused(
                refusal::REFUSED,
                format!("`{EXECUTABLE} acp` kept the session it listed models in: {error}"),
            ));
        }
    }
    drop(stdin);
    let _ = child.kill().await;
    let _ = std::fs::remove_dir_all(&cwd);
    Ok(answer)
}

/// Send one request and read lines until its answer.
async fn ask(
    stdin: &mut ChildStdin,
    lines: &mut Lines<BufReader<ChildStdout>>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value, Error> {
    let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .await
        .map_err(failed)?;
    loop {
        let Some(line) = lines.next_line().await.map_err(failed)? else {
            return Err(Error::refused(
                refusal::REFUSED,
                format!("`{EXECUTABLE} acp` closed before it answered"),
            ));
        };
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message.get("method").is_none() && message["id"] == id {
            return Ok(message);
        }
    }
}

fn failed(e: std::io::Error) -> Error {
    Error::refused(
        refusal::REFUSED,
        format!("`{EXECUTABLE} acp` did not run: {e}"),
    )
}

/// The models of a session's answer: the options of its `model` config option, its
/// `currentValue` first, each with its `_meta.kiro.effortLevels`.
pub(crate) fn listed(session: &Value) -> Vec<Model> {
    let Some(option) = session["configOptions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|option| option["id"] == MODEL)
    else {
        return Vec::new();
    };
    let mut models: Vec<Model> = option["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| {
            Some(Model {
                model: value["value"].as_str()?.to_owned(),
                provider: None,
                label: value["name"].as_str().map(str::to_owned),
                thinking: value
                    .pointer("/_meta/kiro/effortLevels")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|level| level.as_str().map(str::to_owned))
                    .collect(),
                cost: None,
            })
        })
        .collect();
    if let Some(at) = option["currentValue"]
        .as_str()
        .and_then(|current| models.iter().position(|model| model.model == current))
    {
        let first = models.remove(at);
        models.insert(0, first);
    }
    models
}
