//! `mcp/list`: the MCP servers Amp loads, as `amp mcp list --json` names them, run in the
//! folder asked about, so Amp itself merges its settings with the folder's
//! `.amp/settings.json`. Each item is `{name, type: url|command, source, spec}`, and `spec`
//! carries a server's command, arguments, environment, headers and URL verbatim: only a
//! server's name, and a `url` server's URL, bare, are kept. The list is never complete: Amp
//! adds the servers its skills, plugins and toolboxes bring as a thread starts.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};
use steery_sdk::{Error, refusal};

use crate::home::{self, Failed};

/// How long `amp mcp list` has, inside `mcp/list`'s 30 s. It starts in about 3 s.
const WITHIN: Duration = Duration::from_secs(15);
/// The most bytes it may print.
const CAP: usize = 4 << 20;

/// The servers a thread of the person at `home` loads in `asked.cwd`; asked of no folder,
/// where no workspace is, at `/`.
pub async fn list(home: &Path, asked: McpList) -> Result<McpServers, Error> {
    let binary = home::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "Amp is not installed here"))?;
    let cwd = asked.cwd.unwrap_or_else(|| PathBuf::from("/"));
    let ran = home::run(
        &binary,
        &["mcp", "list", "--json"],
        Some(&cwd),
        &[],
        WITHIN,
        CAP,
    )
    .await
    .map_err(|failed| match failed {
        Failed::Start(e) => Error::refused(refusal::NO_BINARY, format!("amp did not start: {e}")),
        failed => Error::refused(refusal::REFUSED, failed.to_string()),
    })?;
    let listed = serde_json::from_slice::<Value>(&ran.stdout)
        .ok()
        .filter(|_| ran.success)
        .and_then(|listed| match listed {
            Value::Array(servers) => Some(servers),
            _ => None,
        })
        .ok_or_else(|| {
            Error::refused(
                refusal::REFUSED,
                ran.line()
                    .unwrap_or("`amp mcp list` did not list its servers")
                    .to_owned(),
            )
        })?;
    let servers = listed
        .iter()
        .filter_map(|server| {
            let name = server.get("name").and_then(Value::as_str)?;
            let url = (server.get("type").and_then(Value::as_str) == Some("url"))
                .then(|| server.pointer("/spec/url").and_then(Value::as_str))
                .flatten();
            Some(McpServer::new(name, url))
        })
        .collect();
    let mut watch = vec![home::settings(home)];
    if cwd != Path::new("/") {
        watch.push(home::workspace_settings(&cwd));
    }
    Ok(McpServers {
        servers,
        complete: false,
        watch,
    })
}
