//! The MCP servers a harness loads, read from the JSON shape omp, pi and Claude Code share:
//! `{"mcpServers": {"<name>": entry}}`, an entry being a URL server (`url`) or one run over
//! standard input and output (`command`). Only a server's name and its URL leave an entry,
//! bare ([`McpServer::new`]): never its command, arguments, environment or headers.

use std::path::Path;

use serde_json::Value;
use steery_sdk::wire::harness::McpServer;

/// A JSON file, a leading byte-order mark allowed; none where it is missing or no JSON.
pub fn json(path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(bytes.strip_prefix("\u{feff}".as_bytes()).unwrap_or(&bytes)).ok()
}

/// Each server `config` names under `mcpServers`, with its entry.
pub fn entries(config: &Value) -> impl Iterator<Item = (&str, &Value)> {
    config
        .get("mcpServers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(name, entry)| (name.as_str(), entry))
}

/// The strings of the array at `key` in `config`: a list of server names.
pub fn names<'a>(config: &'a Value, key: &str) -> impl Iterator<Item = &'a str> {
    config
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

/// Servers by name, the first entry of a name deciding it, whether it is on or off: a
/// harness's sources are added from the one it ranks highest.
#[derive(Debug, Default)]
pub struct Servers(Vec<(McpServer, bool)>);

impl Servers {
    /// `name`, as `entry` says it, on or off, unless an entry of that name came first. Its
    /// URL is its entry's `url`, where it runs no `command`.
    pub fn add(&mut self, name: &str, entry: &Value, on: bool) {
        if self.0.iter().any(|(server, _)| server.server == name) {
            return;
        }
        let url = entry
            .get("url")
            .and_then(Value::as_str)
            .filter(|_| entry.get("command").is_none());
        self.0.push((McpServer::new(name, url), on));
    }

    /// Turn the server named `name` off, wherever it came from.
    pub fn off(&mut self, name: &str) {
        for (server, on) in &mut self.0 {
            if server.server == name {
                *on = false;
            }
        }
    }

    /// The servers that are on.
    pub fn on(self) -> Vec<McpServer> {
        self.0
            .into_iter()
            .filter_map(|(server, on)| on.then_some(server))
            .collect()
    }
}
