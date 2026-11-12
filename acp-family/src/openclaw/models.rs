//! `models`: what `openclaw models list --json` lists (the configured providers' models;
//! `--all` would add the whole catalog), the default first, and whether `openclaw models
//! status --json --check` says they have their sign-in.
//!
//! OpenClaw 2026.11.3 prints, exit 0 (one entry of 468 here):
//!
//! ```text
//! {"count": 468, "models": [{"key": "ollama/qwen3:1.7b", "name": "qwen3:1.7b",
//!   "input": "text", "contextWindow": 40960, "contextTokens": 32768, "local": true,
//!   "available": true, "tags": ["default"]}, …]}
//! ```

use std::path::{Path, PathBuf};

use serde_json::Value;
use steery_driver_common::program::{ask, helper};
use steery_sdk::wire::harness::{Model, Models};
use steery_sdk::{Error, refusal};

/// `models` for OpenClaw with the person's `home` and its state folder `root`: `no-binary`
/// where OpenClaw is not here, refused with why where `openclaw models list` gave no list.
pub async fn list(home: &Path, root: &Path) -> Result<Models, Error> {
    let binary = super::setup::binary(home)
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "openclaw is not installed here"))?;
    let mut command = helper(&binary);
    command.args(["models", "list", "--json"]);
    let (listed, status) = tokio::join!(ask(command), super::sign_in::status(&binary, &[]));
    let listed = listed
        .map_err(|why| Error::refused(refusal::REFUSED, format!("`openclaw models list` {why}")))?;
    let models = serde_json::from_slice::<Value>(&listed.stdout)
        .ok()
        .filter(|_| listed.code == Some(0))
        .and_then(|listed| models(&listed))
        .ok_or_else(|| {
            Error::refused(
                refusal::REFUSED,
                match listed.said.is_empty() {
                    true => "`openclaw models list` gave no list".to_owned(),
                    false => format!("`openclaw models list`: {}", listed.said),
                },
            )
        })?;
    Ok(Models {
        models,
        unlisted: None,
        watch: watched(root),
        signed_in: status.ok().and_then(|status| status.signed_in()),
    })
}

/// The listed models OpenClaw can run, the one tagged `default` first. A key is
/// `<provider>/<model>`, the provider and model id a transcript's `model_change` names.
fn models(listed: &Value) -> Option<Vec<Model>> {
    let mut models = Vec::new();
    for entry in listed.get("models")?.as_array()? {
        if entry.get("available") == Some(&Value::Bool(false)) {
            continue;
        }
        let Some(key) = entry.get("key").and_then(Value::as_str) else {
            continue;
        };
        let (provider, id) = match key.split_once('/') {
            Some((provider, id)) => (Some(provider.to_owned()), id),
            None => (None, key),
        };
        let model = Model {
            provider,
            model: id.to_owned(),
            label: entry.get("name").and_then(Value::as_str).map(str::to_owned),
            thinking: Vec::new(),
            cost: None,
        };
        let default = entry
            .get("tags")
            .and_then(Value::as_array)
            .is_some_and(|tags| tags.iter().any(|tag| tag == "default"));
        if default {
            models.insert(0, model);
        } else {
            models.push(model);
        }
    }
    Some(models)
}

/// The files whose change changes the list: OpenClaw's configuration, and the main agent's
/// `models.json`, where providers it found are kept.
fn watched(root: &Path) -> Vec<PathBuf> {
    vec![
        root.join("openclaw.json"),
        root.join("agents/main/agent/models.json"),
    ]
}
