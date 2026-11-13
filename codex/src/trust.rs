//! Codex's second need (PROTOCOL.md §4.1, Needs): its person's trust in Steery's hooks.
//!
//! Codex runs a hook only after its person trusted the hook's exact content. Until then it
//! runs none of Steery's, and two things stop. A moment of Codex's (a tool call that needs a
//! decision, a request for more permissions) never reaches the person. And a turn Steery
//! starts cannot get more permissions: no person sits at the app server that runs it, so
//! Codex refuses each request at once. On 2026-11-08 every Steery hook on Mark's Mac was
//! untrusted, and each Codex test stopped at its first `git fetch`.
//!
//! The person trusts the hooks on Steery's card. The driver then writes the trust through
//! Codex's own app server (`config/batchWrite`, `hooks.state`), as Codex's `/hooks` review
//! writes it: each Steery hook Codex lists as not run, at the hash Codex lists. Nothing
//! else is trusted, and a hook whose content changes later needs its person again.

use std::path::Path;

use serde_json::{Map, Value, json};
use steery_sdk::wire::harness::{Give, Given, Need, Way};
use steery_sdk::{Error, refusal};

use crate::appserver::AppServer;

/// The need, by its id in every plan.
pub const NEED: &str = "hooks";
/// Its one way.
const TRUST: &str = "trust";

/// The need, where Codex lists a Steery hook that it does not run.
pub fn need() -> Need {
    Need {
        id: NEED.into(),
        title: "Let Codex run Steery's hooks?".into(),
        line: "Codex runs a hook only after you trust it. Until then, a Codex session cannot \
               ask you for more permissions (for example, to use the network), and Steery \
               does not see when Codex waits for you."
            .into(),
        ways: vec![Way {
            id: TRUST.into(),
            label: "Trust Steery's hooks".into(),
            about: Some(
                "Codex runs Steery's command at each of these moments. If the hooks change, \
                 Codex asks you again."
                    .into(),
            ),
            link: None,
            fields: Vec::new(),
            ends_at: None,
        }],
    }
}

/// `needs/give`: trust each Steery hook that Codex in its home `root` lists as not run.
/// `hook` is the runtime's hook line, from the plan that asked the need.
pub async fn give(
    binary: &Path,
    root: &Path,
    hook: Option<&str>,
    give: Give,
) -> Result<Given, Error> {
    if give.way != TRUST {
        return Err(gone(format!(
            "Codex has no way {} to trust its hooks",
            give.way
        )));
    }
    let hook = hook.ok_or_else(|| {
        Error::refused(
            refusal::REFUSED,
            "This driver has not planned Codex's setup since it started. Ask again after the next plan.",
        )
    })?;
    let hooks = crate::setup::steery_hooks(binary, root, hook)
        .await
        .map_err(|why| {
            Error::refused(
                refusal::REFUSED,
                format!("Codex did not list its hooks: {why}"),
            )
        })?;
    if hooks.untrusted.is_empty() {
        return Err(gone("Codex runs every Steery hook already"));
    }
    let state: Map<String, Value> = hooks
        .untrusted
        .iter()
        .map(|(key, hash)| (key.clone(), json!({"trusted_hash": hash})))
        .collect();
    let mut command = steery_driver_common::program::helper(binary);
    command.env("CODEX_HOME", root);
    let mut server = AppServer::open(command, false)
        .await
        .map_err(|e| Error::refused(refusal::REFUSED, e.to_string()))?;
    let written = server
        .call(
            "config/batchWrite",
            json!({
                "edits": [{"keyPath": "hooks.state", "value": state, "mergeStrategy": "upsert"}],
                "reloadUserConfig": true,
            }),
        )
        .await;
    server.close().await;
    written.map_err(|e| {
        Error::refused(
            refusal::REFUSED,
            format!("Codex did not keep the trust: {e}"),
        )
    })?;
    Ok(Given::Done {
        line: format!(
            "Codex runs Steery's {} hooks on this host",
            hooks.untrusted.len()
        ),
        env: Default::default(),
        unset: Vec::new(),
    })
}

fn gone(line: impl Into<String>) -> Error {
    Error::refused(refusal::NOT_FOUND, line)
}
