//! Cline's MCP settings: one JSON file, `mcpServers` by name, each either Cline's nested
//! shape (`transport: {type, url | command}`) or the flat shape it still reads (`url` or
//! `command` beside `type`). A server with `disabled: true` is not loaded.

use serde_json::Value;
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};

use super::place::Place;

/// `mcp/list`: the servers Cline loads. Cline reads one file, whatever the folder.
pub(crate) fn mcp(place: &Place, _asked: McpList) -> McpServers {
    let file = place.mcp();
    let mut complete = true;
    let mut servers = Vec::new();
    if let Ok(text) = std::fs::read_to_string(&file) {
        match serde_json::from_str::<Value>(&text) {
            Ok(document) => {
                for (name, server) in document["mcpServers"].as_object().into_iter().flatten() {
                    if server.get("disabled").and_then(Value::as_bool) == Some(true) {
                        continue;
                    }
                    let url = server
                        .pointer("/transport/url")
                        .or_else(|| server.get("url"))
                        .and_then(Value::as_str);
                    servers.push(McpServer::new(name.clone(), url));
                }
            }
            Err(_) => complete = false,
        }
    }
    McpServers {
        servers,
        complete,
        watch: vec![file],
    }
}

/// The names of the servers Cline loads, to tell an MCP tool by its name: Steery's always,
/// the longest first, so `a_b` wins over `a` for a tool `a_b__c`.
pub(crate) fn server_names(place: &Place) -> Vec<String> {
    let mut names: Vec<String> = mcp(place, McpList { cwd: None })
        .servers
        .into_iter()
        .map(|server| server.server)
        .collect();
    if !names.iter().any(|name| name == "steery") {
        names.push("steery".into());
    }
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_servers_are_not_loaded() {
        let home = std::env::temp_dir().join(format!("steery-cline-mcp-{}", std::process::id()));
        let place = Place::new(&home);
        std::fs::create_dir_all(place.settings()).unwrap();
        std::fs::write(
            place.mcp(),
            r#"{"mcpServers": {
                "steery": {"transport": {"type": "streamableHttp", "url": "http://127.0.0.1:1/mcp"}},
                "old": {"command": "x", "disabled": true},
                "flat": {"url": "http://example.com/sse"}
            }}"#,
        )
        .unwrap();
        let listed = mcp(&place, McpList { cwd: None });
        let names: Vec<&str> = listed.servers.iter().map(|s| s.server.as_str()).collect();
        assert!(listed.complete);
        assert!(names.contains(&"steery") && names.contains(&"flat"));
        assert!(!names.contains(&"old"));
        std::fs::remove_dir_all(&home).unwrap();
    }
}
