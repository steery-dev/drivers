//! `mcp/list`: the servers a conversation loads. Kiro reads its person's
//! `~/.kiro/settings/mcp.json`, and with `cwd` the workspace's `.kiro/settings/mcp.json`,
//! which overrides the person's where a name is in both (Kiro's own system prompt says so).
//! Servers that Kiro's powers and agents add are not read here, so the list is never
//! complete.

use std::path::{Path, PathBuf};

use steery_driver_common::mcp::{self, Servers};
use steery_sdk::wire::harness::{McpList, McpServers};

/// Where Kiro keeps a level's MCP servers, under that level's `.kiro`.
const FILE: &str = "settings/mcp.json";

pub fn list(root: &Path, asked: McpList) -> McpServers {
    let own = root.join(FILE);
    let mut files: Vec<PathBuf> = asked
        .cwd
        .iter()
        .map(|cwd| cwd.join(".kiro").join(FILE))
        .filter(|file| *file != own)
        .collect();
    files.push(own);
    let mut servers = Servers::default();
    for config in files.iter().filter_map(|file| mcp::json(file)) {
        for (name, entry) in mcp::entries(&config) {
            let on = entry.get("disabled").and_then(serde_json::Value::as_bool) != Some(true);
            servers.add(name, entry, on);
        }
    }
    McpServers {
        servers: servers.on(),
        complete: false,
        watch: files,
    }
}
