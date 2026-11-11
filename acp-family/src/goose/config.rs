//! The extensions of Goose's configuration: `extensions.<key>`, each
//! `{enabled, type, name, …}`. A `stdio` extension is an MCP server Goose starts, a
//! `streamable_http` one an MCP server at `uri`, a `builtin` one an MCP server Goose bundles;
//! a `platform` one is Goose's own tools, no MCP server. An extension with `enabled: false`
//! is not loaded.

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};

use super::place::Place;

/// Each extension by its key, with the fields of every file that has it, the later file's
/// field winning, as Goose merges them.
fn extensions(place: &Place) -> BTreeMap<String, Map<String, Value>> {
    let mut extensions: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
    for file in place.config_files() {
        let Some(Value::Object(listed)) = std::fs::read(&file)
            .ok()
            .and_then(|bytes| serde_norway::from_slice::<Value>(&bytes).ok())
            .and_then(|mut config| config.get_mut("extensions").map(Value::take))
        else {
            continue;
        };
        for (key, entry) in listed {
            if let Value::Object(fields) = entry {
                extensions.entry(key).or_default().extend(fields);
            }
        }
    }
    extensions
}

/// The name Goose gives an extension: its `name`, else its key.
fn name<'a>(key: &'a str, entry: &'a Map<String, Value>) -> &'a str {
    entry.get("name").and_then(Value::as_str).unwrap_or(key)
}

fn on(entry: &Map<String, Value>) -> bool {
    entry.get("enabled").and_then(Value::as_bool) == Some(true)
}

/// `mcp/list`: the MCP servers Goose loads. Goose reads no folder's file of its own, but a
/// plugin or a recipe can bring servers that this does not read.
pub(crate) fn mcp(place: &Place, _asked: McpList) -> McpServers {
    let servers = extensions(place)
        .iter()
        .filter(|(_, entry)| on(entry))
        .filter_map(|(key, entry)| {
            let url = match entry.get("type").and_then(Value::as_str) {
                Some("stdio" | "builtin") => None,
                Some("streamable_http") => entry.get("uri").and_then(Value::as_str),
                _ => return None,
            };
            Some(McpServer::new(name(key, entry), url))
        })
        .collect();
    McpServers {
        servers,
        complete: false,
        watch: place.config_files(),
    }
}

/// The names of every extension that is an MCP server, on or off: Goose names a tool of
/// one `<key>__<tool>`. Goose's own `platform` extensions are not among them.
pub(crate) fn server_names(place: &Place) -> Vec<String> {
    extensions(place)
        .iter()
        .filter(|(_, entry)| {
            matches!(
                entry.get("type").and_then(Value::as_str),
                Some("stdio" | "streamable_http" | "builtin")
            )
        })
        .map(|(key, entry)| name(key, entry).to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loaded_servers_and_names() {
        let home = std::env::temp_dir().join(format!("goose-config-{}", std::process::id()));
        let place = Place {
            home: home.clone(),
            config: home.join("config"),
            data: home.join("data"),
        };
        std::fs::create_dir_all(&place.config).unwrap();
        std::fs::write(
            place.config_yaml(),
            "extensions:\n  steery:\n    enabled: true\n    type: streamable_http\n    \
             name: steery\n    uri: http://127.0.0.1:1/mcp\n  github:\n    enabled: false\n    \
             type: stdio\n    name: GitHub Tools\n    cmd: gh-mcp\n  local:\n    enabled: true\n    \
             type: stdio\n    name: local\n    cmd: x\n  developer:\n    enabled: true\n    \
             type: platform\n    name: developer\n",
        )
        .unwrap();
        let listed = mcp(&place, McpList { cwd: None });
        let names: Vec<&str> = listed.servers.iter().map(|s| s.server.as_str()).collect();
        assert_eq!(names, ["local", "steery"]);
        assert_eq!(server_names(&place), ["GitHub Tools", "local", "steery"]);
        std::fs::remove_dir_all(&home).unwrap();
    }
}
