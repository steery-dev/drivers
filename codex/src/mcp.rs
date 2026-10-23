//! `mcp/list`: the MCP servers Codex loads, as `codex mcp list --json` names them, run in
//! the folder asked about, so Codex itself merges its layers and applies its own project
//! trust. Its output carries each server's command, arguments, environment and headers
//! verbatim: only a server's name, and a `streamable_http` server's URL, bare, are kept.
//! A server it lists turned off is left out. The list is never complete: Codex fetches a
//! ChatGPT account's connectors as it starts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};
use steery_sdk::{Error, refusal};

/// The servers a conversation of the Codex at `codex`, whose home is `root`, loads in
/// `asked.cwd`; asked of no folder, where no project is, at `/`.
pub async fn list(codex: &Path, root: &Path, asked: McpList) -> Result<McpServers, Error> {
    let cwd = asked.cwd.unwrap_or_else(|| PathBuf::from("/"));
    // `mcp/list` refuses only `no-binary` and `refused`.
    let entries = listed(codex, Some(root), &cwd, &[])
        .await
        .map_err(|error| match error {
            Error::Refused { word, line } if word == refusal::NO_ANSWER => Error::Refused {
                word: refusal::REFUSED.into(),
                line,
            },
            error => error,
        })?;
    let servers = entries
        .iter()
        .filter(|server| server.get("enabled").and_then(Value::as_bool) != Some(false))
        .filter_map(|server| {
            let name = server.get("name").and_then(Value::as_str)?;
            let transport = server.get("transport");
            let url = transport
                .filter(|t| t.get("type").and_then(Value::as_str) == Some("streamable_http"))
                .and_then(|t| t.get("url"))
                .and_then(Value::as_str);
            Some(McpServer::new(name, url))
        })
        .collect();
    let mut watch = vec![
        root.join("config.toml"),
        PathBuf::from("/etc/codex/config.toml"),
        PathBuf::from("/etc/codex/managed_config.toml"),
    ];
    if cwd != Path::new("/") {
        watch.push(cwd.join(".codex/config.toml"));
    }
    Ok(McpServers {
        servers,
        complete: false,
        watch,
    })
}

/// What `codex mcp list --json` prints, run in `cwd` with the features `off` taken away and,
/// where given, `CODEX_HOME` set to `root`: one object per server.
pub async fn listed(
    codex: &Path,
    root: Option<&Path>,
    cwd: &Path,
    off: &[&str],
) -> Result<Vec<Value>, Error> {
    let mut command = steery_driver_common::program::helper(codex);
    if let Some(root) = root {
        command.env("CODEX_HOME", root);
    }
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        command
            .args(off.iter().flat_map(|feature| ["--disable", *feature]))
            .args(["mcp", "list", "--json"])
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| {
        Error::refused(
            refusal::NO_ANSWER,
            "`codex mcp list` did not answer in time",
        )
    })?
    .map_err(|e| Error::refused(refusal::NO_BINARY, format!("codex would not start: {e}")))?;
    serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .filter(|_| output.status.success())
        .and_then(|listed| match listed {
            Value::Array(servers) => Some(servers),
            _ => None,
        })
        .ok_or_else(|| {
            Error::refused(
                refusal::REFUSED,
                "`codex mcp list` did not list its servers",
            )
        })
}
