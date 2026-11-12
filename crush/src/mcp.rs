//! `mcp/list`: the MCP servers Crush loads, as Crush 0.100.0 reads its configuration
//! (`lookupConfigs` in `internal/config/load.go`). Everywhere: `/etc/crush/crush.json`, the
//! person's `crush.json`, then the `crush.json` that Crush writes in its data folder. In a
//! folder, ahead of those: each folder from the project's root (its git worktree) down to
//! it, `crush.json` then `.crush.json` in each, then the project's own `.crush/crush.json`.
//! A later file's entry wins over an earlier's, field by field. `disabled: true` is off;
//! the project's store keeps a person's toggles (`mcp_disabled_servers`,
//! `mcp_enabled_servers`), which win over every file. A `crushrc` is a shell script that
//! Crush runs to make JSON, which the driver does not run.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value};
use steery_sdk::Error;
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};

use crate::home;

/// Crush's system-wide configuration.
const SYSTEM: &str = "/etc/crush/crush.json";

/// The shell configurations, which the driver cannot read, by the names Crush looks for.
const SHELL: [&str; 2] = ["crushrc", ".crushrc"];

/// The servers a session started in `cwd` loads, or the person's own without one.
pub fn list(home: &Path, asked: McpList) -> Result<McpServers, Error> {
    let (servers, files) = loaded(home, asked.cwd.as_deref());
    let complete = !files.iter().any(|file| {
        file.file_name()
            .is_some_and(|name| SHELL.iter().any(|shell| name == *shell))
            && file.exists()
    });
    Ok(McpServers {
        servers: servers
            .into_iter()
            .map(|(name, entry)| McpServer::new(name, entry.get("url").and_then(Value::as_str)))
            .collect(),
        complete,
        watch: files,
    })
}

/// The names of the servers on in `cwd`, for telling a server's tool by its name.
pub fn names(home: &Path, cwd: &Path) -> Vec<String> {
    loaded(home, Some(cwd))
        .0
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// The servers on, each its name and its merged entry, in name order.
type Servers = Vec<(String, Map<String, Value>)>;

/// The servers on, and every file that changes them, in Crush's order.
fn loaded(home: &Path, cwd: Option<&Path>) -> (Servers, Vec<PathBuf>) {
    let config = home::config(home);
    let mut files = vec![
        PathBuf::from(SYSTEM),
        home::settings(home),
        config.join("crushrc"),
        home::data(home).join("crush.json"),
    ];
    let store = cwd.map(|cwd| {
        let root = root(cwd);
        let mut folders: Vec<&Path> = cwd
            .ancestors()
            .take_while(|dir| dir.starts_with(&root))
            .collect();
        folders.reverse();
        for folder in folders {
            files.extend(
                ["crush.json", ".crush.json", "crushrc", ".crushrc"]
                    .iter()
                    .map(|name| folder.join(name)),
            );
        }
        let data = data_folder(cwd, &root);
        files.push(data.join("crush.json"));
        data.join("crush.db")
    });
    let mut servers: Map<String, Value> = Map::new();
    for file in &files {
        let Some(mcp) = steery_driver_common::mcp::json(file)
            .and_then(|config| config.get("mcp").and_then(Value::as_object).cloned())
        else {
            continue;
        };
        for (name, entry) in mcp {
            let Value::Object(entry) = entry else {
                continue;
            };
            let merged = servers
                .entry(name)
                .or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(merged) = merged {
                merged.extend(entry);
            }
        }
    }
    let (off, on) = store.as_deref().map(toggles).unwrap_or_default();
    let servers = servers
        .into_iter()
        .filter_map(|(name, entry)| {
            let Value::Object(entry) = entry else {
                return None;
            };
            let disabled = entry.get("disabled") == Some(&Value::Bool(true));
            let kept = if off.contains(&name) {
                false
            } else {
                !disabled || on.contains(&name)
            };
            kept.then_some((name, entry))
        })
        .collect();
    if let Some(store) = store {
        files.push(store);
    }
    (servers, files)
}

/// The project's root: its git worktree, else the folder itself.
fn root(cwd: &Path) -> PathBuf {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd)
        .to_owned()
}

/// The project's data folder: the nearest `.crush` folder up to its root, else `cwd`'s.
fn data_folder(cwd: &Path, root: &Path) -> PathBuf {
    cwd.ancestors()
        .take_while(|dir| dir.starts_with(root))
        .map(|dir| dir.join(".crush"))
        .find(|dir| dir.is_dir())
        .unwrap_or_else(|| cwd.join(".crush"))
}

/// The servers a person turned off and on in this project, from its store.
fn toggles(db: &Path) -> (Vec<String>, Vec<String>) {
    let Ok(connection) = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return Default::default();
    };
    let names = |table: &str| -> Vec<String> {
        connection
            .prepare(&format!("select name from {table}"))
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get(0))
                    .and_then(|rows| rows.collect())
            })
            .unwrap_or_default()
    };
    (names("mcp_disabled_servers"), names("mcp_enabled_servers"))
}
