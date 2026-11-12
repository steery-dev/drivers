//! `mcp/list`: the MCP servers OpenClaw loads, as `mcp.servers` of `<root>/openclaw.json`
//! names them. A server with `enabled: false` is off (`src/agents/bundle-mcp-config.ts:128`).
//! A server with a `command` runs it, whatever its `url` (`src/config/zod-schema.mcp-server.ts:135`),
//! so only a server without one keeps its URL. OpenClaw reads no MCP file per folder: a
//! folder asked about lists the same servers. The list is never complete: enabled plugins
//! bring servers of their own (`loadEnabledBundleMcpConfig`).

use std::path::Path;

use serde_json::Value;
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};
use steery_sdk::{Error, refusal};

/// The servers a conversation of OpenClaw, whose state folder is `root`, loads; `asked.cwd`
/// changes nothing. `home` is the person's home.
pub async fn list(_home: &Path, root: &Path, _asked: McpList) -> Result<McpServers, Error> {
    let config = root.join("openclaw.json");
    let servers = match std::fs::read_to_string(&config) {
        Err(_) => Vec::new(),
        Ok(text) => {
            // OpenClaw reads JSON5; comments and trailing commas are read here too.
            let config = crate::jsonc::parse(&text).map_err(|e| {
                Error::refused(
                    refusal::REFUSED,
                    format!("{} is not JSON the driver reads: {e}", config.display()),
                )
            })?;
            servers(&config)
        }
    };
    Ok(McpServers {
        servers,
        complete: false,
        watch: vec![config],
    })
}

fn servers(config: &Value) -> Vec<McpServer> {
    config
        .pointer("/mcp/servers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, server)| server.get("enabled") != Some(&Value::Bool(false)))
        .map(|(name, server)| {
            let url = server
                .get("command")
                .is_none()
                .then(|| server.get("url").and_then(Value::as_str))
                .flatten();
            McpServer::new(name, url)
        })
        .collect()
}
