use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use steery_sdk::{Error, refusal};
use tokio::process::Command;

#[derive(Clone)]
pub struct Native {
    pub cli: PathBuf,
    pub cache: PathBuf,
    pub env: std::collections::BTreeMap<String, String>,
}

pub struct Delivery {
    before: usize,
    nonce: String,
}

pub fn refused(why: impl Into<String>) -> Error {
    Error::refused(refusal::REFUSED, why)
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

pub fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Error> {
    value.get(key).and_then(Value::as_str)
        .ok_or_else(|| refused(format!("native response lacks string {key}: {value}")))
}

pub fn timestamp(value: &Value, key: &str) -> Result<u64, Error> {
    let field = value.get(key).ok_or_else(|| refused(format!("native response lacks {key}: {value}")))?;
    if let Some(number) = field.as_u64() { return Ok(number); }
    if let Some(text) = field.as_str() {
        let time = chrono::DateTime::parse_from_rfc3339(text)
            .map_err(|error| refused(format!("native {key}: {error}")))?;
        return u64::try_from(time.timestamp_millis()).map_err(|error| refused(error.to_string()));
    }
    Err(refused(format!("native {key} is not a timestamp: {field}")))
}

pub fn running(agent: &Value) -> Result<bool, Error> {
    let turn = agent.get("isRunningTurn").and_then(Value::as_bool)
        .ok_or_else(|| refused(format!("native agent lacks boolean isRunningTurn: {agent}")))?;
    let composing = agent.get("isComposingMessage").and_then(Value::as_bool)
        .ok_or_else(|| refused(format!("native agent lacks boolean isComposingMessage: {agent}")))?;
    Ok(turn || composing)
}

pub fn person(entry: &Value) -> Option<&str> {
    (entry.get("kind")?.as_str()? == "message" && entry.get("role")?.as_str()? == "user")
        .then(|| entry.get("content").and_then(Value::as_str)).flatten()
}

pub fn reply(entry: &Value) -> Option<&str> {
    if entry.get("kind")?.as_str()? != "send-message" { return None; }
    let message = entry.get("message")?;
    if message.get("type")?.as_str()? != "text" { return None; }
    message.get("content")?.as_str()
}

pub fn marker(text: &str, delivery: &str) -> bool {
    let marker = format!("steery-delivery:{delivery}");
    text.match_indices(&marker).any(|(at, _)| {
        text[at + marker.len()..].chars().next()
            .is_none_or(|next| !(next.is_ascii_alphanumeric() || matches!(next, '.' | '-' | '_')))
    })
}

impl Native {
    pub async fn command(&self, args: &[String]) -> Result<Value, Error> {
        for key in ["GROK_ACCESS_TOKEN", "GROK_CLIENT_VERSION"] {
            if self.env.get(key).is_none_or(|value| value.is_empty()) {
                return Err(refused(format!("Set the {key} driver setting")));
            }
        }
        let output = tokio::time::timeout(Duration::from_secs(60),
            Command::new(&self.cli).args(args).envs(&self.env).kill_on_drop(true).output()).await
            .map_err(|_| refused(format!("{} command exceeded 60 seconds", self.cli.display())))?
            .map_err(|error| Error::refused(refusal::NO_BINARY, format!("{}: {error}", self.cli.display())))?;
        if !output.status.success() {
            return Err(refused(format!("{} exited {}: {}{}", self.cli.display(), output.status,
                String::from_utf8_lossy(&output.stderr), String::from_utf8_lossy(&output.stdout))));
        }
        let value: Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| refused(format!("native JSON: {error}: {}", String::from_utf8_lossy(&output.stdout))))?;
        if value.get("error").is_some() || value.get("ok").and_then(Value::as_bool) == Some(false) {
            return Err(refused(format!("native API error: {value}")));
        }
        Ok(value)
    }

    pub async fn api(&self, method: &str, params: Value) -> Result<Value, Error> {
        self.command(&["api".into(), method.into(), params.to_string()]).await
    }

    pub async fn agents(&self) -> Result<Vec<Value>, Error> {
        let response = self.api("listAgents", json!({})).await?;
        response.as_array().cloned().ok_or_else(|| refused(format!("listAgents is not an array: {response}")))
    }

    pub async fn agent(&self, id: &str) -> Result<Value, Error> {
        self.agents().await?.into_iter().find(|agent| agent.get("id").and_then(Value::as_str) == Some(id))
            .ok_or_else(|| Error::refused(refusal::NOT_FOUND, format!("Grok Bot has no agent {id}")))
    }

    pub async fn transcript(&self, id: &str) -> Result<Vec<Value>, Error> {
        let response = self.api("openAgent", json!({"id":id})).await?;
        let entries = response.as_array().cloned()
            .ok_or_else(|| refused(format!("openAgent is not an array: {response}")))?;
        let path = self.path(id);
        tokio::fs::create_dir_all(&self.cache).await.map_err(|error| refused(error.to_string()))?;
        let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let mut bytes = Vec::new();
        for entry in &entries {
            serde_json::to_writer(&mut bytes, entry).map_err(|error| refused(error.to_string()))?;
            bytes.push(b'\n');
        }
        tokio::fs::write(&temporary, bytes).await.map_err(|error| refused(error.to_string()))?;
        tokio::fs::rename(&temporary, &path).await.map_err(|error| refused(error.to_string()))?;
        Ok(entries)
    }

    pub fn path(&self, id: &str) -> PathBuf {
        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(id.as_bytes());
        self.cache.join(format!("{hash:x}.jsonl"))
    }

    pub async fn create(&self, cwd: &Path) -> Result<String, Error> {
        let response = self.command(&["bots".into(), "create".into(), "--gateway".into(),
            "--name".into(), "Steery conversation".into(), "--description".into(),
            format!("Steery conversation requested from {}", cwd.display()), "--json".into()]).await?;
        Ok(string(&response, "id")?.to_owned())
    }

    pub async fn send(&self, id: &str, words: &str) -> Result<Delivery, Error> {
        if running(&self.agent(id).await?)? {
            return Err(Error::refused(refusal::HARNESS_BUSY, "Grok Bot is running a turn"));
        }
        let before = self.transcript(id).await?.len();
        let nonce = uuid::Uuid::new_v4().to_string();
        let response = self.api("sendPrompt", json!({"agentId":id,"prompt":words,
            "clientNonce":nonce})).await?;
        if response.get("accepted").and_then(Value::as_bool) != Some(true) {
            return Err(refused(format!("sendPrompt did not accept the input: {response}")));
        }
        Ok(Delivery { before, nonce })
    }

    pub async fn prove_input(&self, id: &str, words: &str, delivery: Delivery) -> Result<usize, Error> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let entries = self.transcript(id).await?;
            if entries.len() < delivery.before { return Err(refused("native transcript changed during input delivery")); }
            if let Some(index) = entries.iter().enumerate().skip(delivery.before)
                .find_map(|(index, entry)| (person(entry) == Some(words)
                    && entry.get("clientNonce").and_then(Value::as_str) == Some(delivery.nonce.as_str())).then_some(index)) {
                return Ok(index);
            }
            if Instant::now() >= deadline { return Err(Error::refused(refusal::NO_ANSWER,
                "native accepted input but its transcript did not prove delivery within 60 seconds")); }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn finish(&self, id: &str, input: usize) -> Result<String, Error> {
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            let entries = self.transcript(id).await?;
            let source = entries.get(input).ok_or_else(|| refused("native input disappeared from transcript"))?;
            let request = string(source, "requestId")?;
            let answers: Vec<&str> = entries.iter().skip(input + 1).filter(|entry|
                entry.get("requestId").and_then(Value::as_str) == Some(request))
                .filter_map(reply).collect();
            if !running(&self.agent(id).await?)? && !answers.is_empty() {
                return Ok(answers.join("\n"));
            }
            if Instant::now() >= deadline { return Err(Error::refused(refusal::NO_ANSWER,
                "native turn did not prove an ended state with a response within 600 seconds")); }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}
