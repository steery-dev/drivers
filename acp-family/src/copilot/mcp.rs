//! `mcp/list`: the MCP servers Copilot loads, as its own `copilot mcp list --json` merges
//! them: the person's `mcp-config.json`, installed plugins' servers and Copilot's built-in
//! ones, each with whether `disabledMcpServers` in `settings.json` turned it off.
//!
//! A folder's own servers (`.mcp.json`, `.github/mcp.json`) are not in that listing
//! (1.0.108, a trusted git folder), though a conversation started there may load them: a
//! listing for a folder is never complete.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use steery_driver_common::mcp::{self, Servers};
use steery_driver_common::program::helper;
use steery_sdk::wire::harness::{McpList, McpServers};
use steery_sdk::{Error, refusal};

use super::home;

/// How long `copilot mcp list` has to answer: it took 1.5 s here.
const WITHIN: Duration = Duration::from_secs(20);

/// The servers a conversation started in `asked.cwd`, or anywhere, loads, for the Copilot
/// whose home is `root`.
pub async fn list(root: &Path, asked: McpList) -> Result<McpServers, Error> {
    let binary = home::binary()
        .ok_or_else(|| Error::refused(refusal::NOT_FOUND, "Copilot is not installed here"))?;
    let mut command = helper(&binary);
    command
        .args(["mcp", "list", "--json"])
        .stdout(Stdio::piped());
    if let Some(cwd) = asked.cwd.as_deref().filter(|cwd| cwd.is_dir()) {
        command.current_dir(cwd);
    }
    let output = tokio::time::timeout(WITHIN, command.output())
        .await
        .map_err(|_| refused("`copilot mcp list` did not answer in time"))?
        .map_err(|e| refused(format!("`copilot mcp list` did not run: {e}")))?;
    if !output.status.success() {
        return Err(refused(format!(
            "`copilot mcp list` failed: {}",
            output.status
        )));
    }
    let listed: Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| refused(format!("`copilot mcp list` printed no JSON: {e}")))?;
    let mut servers = Servers::default();
    for (name, entry) in mcp::entries(&listed) {
        let on = entry.get("enabled").and_then(Value::as_bool) != Some(false);
        servers.add(name, entry, on);
    }
    Ok(McpServers {
        servers: servers.on(),
        complete: asked.cwd.is_none(),
        watch: vec![
            root.join("mcp-config.json"),
            root.join("settings.json"),
            root.join("installed-plugins"),
        ],
    })
}

fn refused(line: impl Into<String>) -> Error {
    Error::refused(refusal::REFUSED, line)
}
