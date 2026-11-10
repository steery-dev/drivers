//! `mcp/list`: the MCP servers of Hermes's `config.yaml` under `mcp_servers`, and of the
//! managed overlay `/etc/hermes/config.yaml`, whose entry of a name wins. An entry with
//! `enabled: false` is off. Plugins can bring servers of their own, which this does not read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use steery_sdk::wire::harness::{McpServer, McpServers};

/// Hermes's managed overlay of its configuration.
const MANAGED: &str = "/etc/hermes/config.yaml";

fn files(root: &Path) -> [PathBuf; 2] {
    [root.join("config.yaml"), PathBuf::from(MANAGED)]
}

/// Each server by its name, and whether it is on: the overlay's entry of a name wins.
fn entries(root: &Path) -> BTreeMap<String, Value> {
    let mut entries = BTreeMap::new();
    for file in files(root) {
        let Some(Value::Object(servers)) = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_norway::from_slice::<Value>(&bytes).ok())
            .and_then(|config| config.get("mcp_servers").cloned())
        else {
            continue;
        };
        entries.extend(servers);
    }
    entries
}

fn on(entry: &Value) -> bool {
    entry.get("enabled").and_then(Value::as_bool) != Some(false)
}

pub fn list(root: &Path) -> McpServers {
    McpServers {
        servers: entries(root)
            .iter()
            .filter(|(_, entry)| on(entry))
            .map(|(name, entry)| {
                McpServer::new(
                    name,
                    entry
                        .get("url")
                        .and_then(Value::as_str)
                        .filter(|_| entry.get("command").is_none()),
                )
            })
            .collect(),
        complete: false,
        watch: files(root).to_vec(),
    }
}

/// The names of the configured servers, on or off: the server part of an MCP tool's name.
pub fn names(root: &Path) -> Vec<String> {
    entries(root).into_keys().collect()
}
