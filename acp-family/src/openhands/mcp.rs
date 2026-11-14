//! `mcp/list`: the servers a conversation loads. OpenHands reads only its person's
//! `<root>/mcp.json`, `{"mcpServers": {"<name>": entry}}`, and loads each entry whose
//! `enabled` is not false (`openhands_cli/mcp/mcp_utils.py`, `list_enabled_servers`), the
//! same in every folder.

use steery_driver_common::mcp::{self, Servers};
use steery_sdk::wire::harness::{McpList, McpServers};

use super::place::Place;

pub fn list(place: &Place, _asked: McpList) -> McpServers {
    let mut servers = Servers::default();
    if let Some(config) = mcp::json(&place.mcp()) {
        for (name, entry) in mcp::entries(&config) {
            servers.add(name, entry, enabled(entry));
        }
    }
    McpServers {
        servers: servers.on(),
        complete: true,
        watch: vec![place.mcp()],
    }
}

/// The names of the servers OpenHands loads, which name the tools of their calls.
pub fn names(place: &Place) -> Vec<String> {
    mcp::json(&place.mcp())
        .map(|config| {
            mcp::entries(&config)
                .filter(|(_, entry)| enabled(entry))
                .map(|(name, _)| name.to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn enabled(entry: &serde_json::Value) -> bool {
    entry.get("enabled").and_then(serde_json::Value::as_bool) != Some(false)
}
