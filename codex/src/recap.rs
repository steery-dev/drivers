//! `recap`: the thread's own model says where it stands, in an ephemeral fork of it.
//!
//! An app server of the driver's own, in the thread's home, forks the thread with
//! `ephemeral: true` (Codex writes no rollout of it and lists it nowhere) on the model and
//! thinking level the thread last ran on, and runs one turn there asking what is asked.
//! The fork holds the thread's whole context as Codex keeps it, its compactions' summaries
//! included, so the recap is written from what the model itself saw. Every way out is
//! taken away as from the reader ([`crate::drive::reader`]): each tool feature this Codex
//! knows, its hooks among them, and each MCP server the thread would reach from its folder,
//! under a `read-only` sandbox that asks no approval. The fork's last `agentMessage` is the
//! recap; the app server is closed after, and nothing of the fork is kept.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};
use steery_sdk::wire::harness::{Locate, ReaderTools, Recap, Text};
use steery_sdk::{Error, refusal};

use crate::appserver::{self, AppServer};
use crate::drive::{self, Drive};

/// How long the thread's model has to write a recap, the fork set up and all: inside the
/// 40 seconds the runtime waits for one.
const RECAP_WITHIN: Duration = Duration::from_secs(35);

impl Drive {
    /// `recap`: the thread's model, as its rollout's newest turn names it, answers what is
    /// asked in an ephemeral fork of the thread, within [`RECAP_WITHIN`].
    pub async fn recap(&self, recap: Recap) -> Result<Text, Error> {
        let unanswered = |why: String| Error::refused(refusal::NO_ANSWER, why);
        let thread = &recap.external_id;
        let missing = || unanswered(format!("Codex has no thread {thread} here"));
        uuid::Uuid::parse_str(thread).map_err(|_| missing())?;
        let home = self.thread_home(thread).map_err(|_| missing())?;
        let tail = self
            .store
            .latest(thread, drive::LATEST)
            .map_err(|_| missing())?;
        let model = tail
            .model
            .filter(|model| !model.is_empty())
            .ok_or_else(|| {
                Error::refused(
                    refusal::MODEL_MISSING,
                    "the thread's model has not answered in it yet",
                )
            })?;
        let cwd = self
            .store
            .locate(Locate {
                external_id: thread.clone(),
            })
            .ok()
            .and_then(|located| located.session.cwd)
            .filter(|cwd| cwd.is_dir())
            .unwrap_or_else(|| PathBuf::from("/"));
        let codex = drive::binary().map_err(|error| unanswered(error.to_string()))?;
        let asked = format!(
            "Do not carry on with this conversation's work, and use no tool: answer only \
             from what it holds. {}",
            recap.asked
        );
        let ask = Ask {
            codex: &codex,
            home: &home,
            cwd: &cwd,
            thread,
            model: &model,
            effort: tail.thinking.as_deref(),
            asked: &asked,
        };
        tokio::time::timeout(RECAP_WITHIN, ask.run())
            .await
            .map_err(|_| unanswered("the thread's model wrote no recap in time".into()))?
            .map(|text| Text { text })
            .map_err(unanswered)
    }
}

/// One recap asked of the Codex at `codex`, whose home `home` files the thread.
struct Ask<'a> {
    codex: &'a Path,
    home: &'a Path,
    cwd: &'a Path,
    thread: &'a str,
    model: &'a str,
    effort: Option<&'a str>,
    asked: &'a str,
}

impl Ask<'_> {
    /// The recap's words, or why there are none.
    async fn run(&self) -> Result<String, String> {
        let known = drive::features(self.codex)
            .await
            .map_err(|error| error.to_string())?;
        let off: Vec<&str> = drive::taken(ReaderTools::None, &known).collect();
        let servers = drive::servers(self.codex, Some(self.home), self.cwd, &off)
            .await
            .map_err(|error| error.to_string())?;
        let mut server = crate::native::reader(self.codex, self.home).await?;
        let answered = self.answered(&mut server, &off, &servers).await;
        // Nothing of an ephemeral fork is on disk to wait for.
        tokio::spawn(server.close());
        answered
    }

    async fn answered(
        &self,
        server: &mut AppServer,
        off: &[&str],
        servers: &[String],
    ) -> Result<String, String> {
        let forked = server
            .call("thread/fork", fork(self.thread, self.model, off, servers))
            .await
            .map_err(|e| e.to_string())?;
        let fork = forked
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .ok_or("Codex named no fork")?
            .to_owned();
        let mut start = json!({"threadId": fork, "input": appserver::said(self.asked)});
        if let Some(effort) = self.effort {
            start["effort"] = json!(effort);
        }
        let started = server
            .call("turn/start", start)
            .await
            .map_err(|e| e.to_string())?;
        let turn = started
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .ok_or("Codex named no turn")?
            .to_owned();
        let mut said = None;
        loop {
            let message = server.recv().await.map_err(|e| e.to_string())?;
            if let Some(ended) = heard(&message, &fork, &turn, &mut said) {
                return ended;
            }
        }
    }
}

/// `thread/fork` of `thread` into an ephemeral one on `model`, the features `off` and the
/// MCP servers `servers` turned off, read-only and asking no approval.
fn fork(thread: &str, model: &str, off: &[&str], servers: &[String]) -> Value {
    let mut config = Map::new();
    for feature in off {
        config.insert(format!("features.{feature}"), json!(false));
    }
    for server in servers {
        config.insert(format!("mcp_servers.{server}.enabled"), json!(false));
    }
    json!({
        "threadId": thread,
        "ephemeral": true,
        "excludeTurns": true,
        "model": model,
        "sandbox": "read-only",
        "approvalPolicy": "never",
        "config": config,
    })
}

/// What one message of the app server says of the fork's turn: its last agent message
/// kept in `said`, and once `turn/completed` ends it, the recap or why there is none.
fn heard(
    message: &Value,
    fork: &str,
    turn: &str,
    said: &mut Option<String>,
) -> Option<Result<String, String>> {
    let params = message.get("params")?;
    if params.get("threadId").and_then(Value::as_str) != Some(fork) {
        return None;
    }
    match message.get("method").and_then(Value::as_str)? {
        "item/completed" => {
            let item = params.get("item")?;
            if item.get("type").and_then(Value::as_str) == Some("agentMessage")
                && let Some(text) = item
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
            {
                *said = Some(text.to_owned());
            }
            None
        }
        "turn/completed" if params.pointer("/turn/id").and_then(Value::as_str) == Some(turn) => {
            let failed = params
                .pointer("/turn/error/message")
                .and_then(Value::as_str)
                .map(crate::events::one_line);
            Some(match (said.take(), failed) {
                (Some(text), None) => Ok(text),
                (_, Some(why)) => Err(format!("the thread's model failed: {why}")),
                (None, None) => Err("the thread's model wrote no recap".into()),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fork is ephemeral, on the thread's model, with every way out taken away.
    #[test]
    fn a_recap_forks_the_thread_ephemeral_with_nothing_to_reach() {
        let asked = fork(
            "t",
            "gpt-6.1-sol",
            &["shell_tool", "hooks"],
            &["docs".into()],
        );
        assert_eq!(asked["ephemeral"], json!(true));
        assert_eq!(asked["model"], json!("gpt-6.1-sol"));
        assert_eq!(asked["sandbox"], json!("read-only"));
        assert_eq!(asked["approvalPolicy"], json!("never"));
        assert_eq!(
            asked["config"],
            json!({"features.shell_tool": false, "features.hooks": false, "mcp_servers.docs.enabled": false})
        );
    }

    /// The fork's last agent message is the recap once its turn completes; another
    /// thread's lines, and a failed turn's words, are not.
    #[test]
    fn the_recap_is_the_forks_last_agent_message() {
        let mut said = None;
        let message = |thread: &str, text: &str| json!({"method": "item/completed", "params": {"threadId": thread, "turnId": "u", "item": {"type": "agentMessage", "id": "m", "text": text}}});
        assert_eq!(heard(&message("f", "first"), "f", "u", &mut said), None);
        assert_eq!(
            heard(&message("f", "where it stands"), "f", "u", &mut said),
            None
        );
        assert_eq!(
            heard(&message("other", "not this"), "f", "u", &mut said),
            None
        );
        let done = json!({"method": "turn/completed", "params": {"threadId": "f", "turn": {"id": "u", "status": "completed"}}});
        assert_eq!(
            heard(&done, "f", "u", &mut said),
            Some(Ok("where it stands".into()))
        );
        let mut said = Some("half".into());
        let failed = json!({"method": "turn/completed", "params": {"threadId": "f", "turn": {"id": "u", "status": "failed", "error": {"message": "usage limit"}}}});
        assert_eq!(
            heard(&failed, "f", "u", &mut said),
            Some(Err("the thread's model failed: usage limit".into()))
        );
    }
}
