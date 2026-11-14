//! `mcp/list`: the MCP servers Warp starts on its own. Warp starts every server in each
//! channel's `~/.warp*/.mcp.json` (`auto_start_decision` in
//! `app/src/ai/mcp/file_based_manager.rs`), and each server the person installed in Warp
//! that it restores running (`mcp_server_installations.restore_running` in `warp.sqlite`).
//! The servers of other agents' files in the home folder (`~/.claude.json`,
//! `~/.codex/config.toml`, `~/.agents/.mcp.json`) start only where a setting of Warp's says
//! so, and a project's own files never start on their own; servers of a Warp team come
//! from Warp's servers. The driver reads none of these, so the list is not complete.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};
use steery_driver_common::mcp::{Servers, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{McpList, McpServers};

use crate::home;

/// Where Warp looks for servers in an MCP file, in its order
/// (`TemplatableMCPServer::find_servers_under_known_keys`).
const POINTERS: [&str; 4] = ["/mcp/servers", "/servers", "/mcpServers", "/mcp_servers"];

/// The pointer of an MCP file's servers: the first of Warp's keys that holds an object.
pub(crate) fn pointer(config: &Value) -> Option<&'static str> {
    POINTERS
        .into_iter()
        .find(|pointer| config.pointer(pointer).is_some_and(Value::is_object))
}

/// The servers of an MCP file's JSON, by name.
fn servers(config: &Value) -> Option<&Map<String, Value>> {
    config.pointer(pointer(config)?)?.as_object()
}

pub fn list(home: &Path, _: McpList) -> Result<McpServers, Error> {
    let mut found = Servers::default();
    let mut watch: Vec<PathBuf> = Vec::new();
    for config in home::configs(home) {
        let file = home::mcp_file(&config);
        if let Some(entries) = json(&file).as_ref().and_then(servers) {
            for (name, entry) in entries {
                found.add(name, entry, true);
            }
        }
        watch.push(file);
    }
    for db in home::stores(home) {
        for (name, entry) in installed(&db) {
            found.add(&name, &entry, true);
        }
        watch.push(db);
    }
    Ok(McpServers {
        servers: found.on(),
        complete: false,
        watch,
    })
}

/// The servers installed in Warp that it restores running, each by its name and its entry.
/// An installation keeps its entry as JSON text within `template.json`, under a known key
/// or alone.
fn installed(db: &Path) -> Vec<(String, Value)> {
    let Ok(connection) = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Vec::new();
    };
    let _ = connection.busy_timeout(Duration::from_secs(2));
    let rows: Vec<String> = connection
        .prepare(
            "select templatable_mcp_server from mcp_server_installations \
             where restore_running order by id",
        )
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get(0))
                .and_then(|rows| rows.collect())
        })
        .unwrap_or_default();
    rows.iter()
        .filter_map(|row| {
            let server: Value = serde_json::from_str(row).ok()?;
            let name = server["name"].as_str().filter(|name| !name.is_empty())?;
            let entry = server["template"]["json"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .and_then(|config| {
                    let entries = servers(&config).or_else(|| config.as_object())?;
                    entries.values().next().cloned()
                })
                .unwrap_or(Value::Null);
            Some((name.to_owned(), entry))
        })
        .collect()
}
