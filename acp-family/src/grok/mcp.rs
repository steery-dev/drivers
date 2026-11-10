//! `mcp/list`: the MCP servers Grok Build loads, as `grok inspect --json` names them, run in
//! the folder asked about, so Grok itself merges its sources (its own `config.toml`, the
//! project's `.grok/config.toml`, `~/.claude.json`, `~/.cursor/mcp.json`, `.mcp.json`) and
//! applies its own project trust and `enabled = false`. Each entry is `{name, transport:
//! stdio|http, target}`; `target` is the command of a `stdio` server and the URL of an
//! `http` one: only a server's name, and an `http` server's URL, are kept. A server that
//! Grok imports from another vendor but turned off (`compatibilityStatus` other than
//! `enabled`) is left out. The list is never complete: plugins and the servers an ACP
//! client forwards join only as a conversation starts.

use std::path::{Path, PathBuf};

use serde_json::Value;
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};
use steery_sdk::{Error, refusal};

/// The servers a conversation of Grok, whose home is `root`, loads in `asked.cwd`; asked
/// of no folder, where no project is, at `/`. `home` is the person's home.
pub async fn list(home: &Path, root: &Path, asked: McpList) -> Result<McpServers, Error> {
    let grok = super::binary(root)
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "grok is not installed"))?;
    let cwd = asked.cwd.unwrap_or_else(|| PathBuf::from("/"));
    let mut command = steery_driver_common::program::helper(&grok);
    command
        .env("RUST_LOG", "off")
        .env("GROK_HOME", root)
        .args(["inspect", "--json"])
        .current_dir(&cwd);
    // `mcp/list` refuses only `no-binary` and `refused`.
    let answer = steery_driver_common::program::ask(command)
        .await
        .map_err(|why| Error::refused(refusal::REFUSED, format!("`grok inspect` {why}")))?;
    let inspected = serde_json::from_slice::<Value>(&answer.stdout)
        .ok()
        .filter(|_| answer.code == Some(0))
        .ok_or_else(|| {
            Error::refused(
                refusal::REFUSED,
                match answer.said.is_empty() {
                    true => "`grok inspect` did not describe its setup".to_owned(),
                    false => format!("`grok inspect`: {}", answer.said),
                },
            )
        })?;
    let servers = inspected
        .get("mcpServers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|server| {
            server
                .get("compatibilityStatus")
                .and_then(Value::as_str)
                .is_none_or(|status| status == "enabled")
        })
        .filter_map(|server| {
            let name = server.get("name").and_then(Value::as_str)?;
            let url = server
                .get("transport")
                .filter(|transport| *transport == "http")
                .and_then(|_| server.get("target"))
                .and_then(Value::as_str);
            Some(McpServer::new(name, url))
        })
        .collect();
    Ok(McpServers {
        servers,
        complete: false,
        watch: vec![
            root.join("config.toml"),
            home.join(".claude.json"),
            home.join(".cursor/mcp.json"),
            cwd.join(".mcp.json"),
            cwd.join(".grok/config.toml"),
        ],
    })
}
