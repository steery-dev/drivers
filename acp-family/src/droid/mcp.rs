//! `mcp/list`: the servers a conversation loads. Droid reads its person's
//! `~/.factory/mcp.json`, and with `cwd` a `.factory/mcp.json` in the project folder and in
//! each folder above it. Where a name is defined at more than one level Droid loads one
//! definition, but its documentation does not say which: this list ranks the nearest folder
//! first, then the farther ones, then the person's file. Servers its organization manages win
//! over all of them and are not readable here, so the list is never complete.

use std::path::{Path, PathBuf};

use steery_driver_common::mcp::{self, Servers};
use steery_sdk::wire::harness::{McpList, McpServers};

pub fn list(root: &Path, asked: McpList) -> McpServers {
    let own = root.join("mcp.json");
    let mut files: Vec<PathBuf> = asked
        .cwd
        .iter()
        .flat_map(|cwd| cwd.ancestors())
        .map(|folder| folder.join(".factory/mcp.json"))
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
