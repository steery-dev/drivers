//! `mcp/list`: the MCP servers the harness loads, from the `mcp.json` files it reads itself.
//!
//! - omp, in the order it ranks them: a folder's `.omp/mcp.json` and `.omp/.mcp.json`, then
//!   the agent folder's `mcp.json` and `.mcp.json`, then the folder's own `mcp.json` and
//!   `.mcp.json`; the first entry of a name decides it. An entry `enabled: false` is off
//!   unless the agent folder's `enabledServers` names it; `disabledServers` there turns a
//!   server off whatever its source. `mcp.enableProjectConfig: false` in its `config.yml`
//!   drops every file of the folder.
//! - pi: the agent folder's `mcp.json`, and a folder's `.pi/mcp.json` ahead of it where the
//!   agent folder's `trust.json` trusts that folder.
//!
//! Neither list is complete: omp also reads other tools' configuration, plugins and
//! extension packages, and a pi extension may register a server of its own.

use std::path::{Path, PathBuf};

use serde_json::Value;
use steery_driver_common::mcp::{self, Servers};
use steery_sdk::wire::harness::{McpList, McpServers};

use crate::Harness;
use crate::harness::{OMP_CONFIG, omp_setting};

/// The servers a conversation of `harness`, whose agent folder is `root`, loads: in
/// `asked.cwd`, or everywhere.
pub fn list(harness: Harness, root: &Path, asked: McpList) -> McpServers {
    let cwd = asked.cwd.as_deref();
    let (files, overrides) = match harness {
        Harness::Omp => (
            omp(root, cwd),
            vec![root.join("mcp.json"), root.join(".mcp.json")],
        ),
        Harness::Pi => (pi(root, cwd), Vec::new()),
    };
    let overrides: Vec<Value> = overrides
        .iter()
        .filter_map(|file| mcp::json(file))
        .collect();
    let listed = |key| {
        overrides
            .iter()
            .flat_map(move |config| mcp::names(config, key))
    };
    let enabled: Vec<&str> = listed("enabledServers").collect();
    let mut servers = Servers::default();
    for config in files.iter().filter_map(|file| mcp::json(file)) {
        for (name, entry) in mcp::entries(&config) {
            let on = entry.get("enabled").and_then(Value::as_bool) != Some(false)
                || enabled.contains(&name);
            servers.add(name, entry, on);
        }
    }
    for name in listed("disabledServers") {
        servers.off(name);
    }
    let mut watch = files;
    match harness {
        Harness::Omp => watch.extend(OMP_CONFIG.map(|name| root.join(name))),
        Harness::Pi => watch.push(root.join("trust.json")),
    }
    McpServers {
        servers: servers.on(),
        complete: false,
        watch,
    }
}

/// omp's files, the one it ranks highest first.
fn omp(root: &Path, cwd: Option<&Path>) -> Vec<PathBuf> {
    let projects = omp_setting(root, "mcp", "enableProjectConfig").as_deref() != Some("false");
    let cwd = cwd.filter(|_| projects);
    let mut files = Vec::new();
    if let Some(cwd) = cwd {
        files.extend([cwd.join(".omp/mcp.json"), cwd.join(".omp/.mcp.json")]);
    }
    files.extend([root.join("mcp.json"), root.join(".mcp.json")]);
    if let Some(cwd) = cwd {
        files.extend([cwd.join("mcp.json"), cwd.join(".mcp.json")]);
    }
    files
}

/// pi's files, the one it ranks highest first: a folder's own only where `trust.json`
/// says `true` of exactly that folder.
fn pi(root: &Path, cwd: Option<&Path>) -> Vec<PathBuf> {
    let trusted = |cwd: &Path| {
        mcp::json(&root.join("trust.json"))
            .and_then(|decisions| {
                decisions.as_object().map(|decisions| {
                    decisions.iter().any(|(folder, trusted)| {
                        trusted.as_bool() == Some(true) && Path::new(folder) == cwd
                    })
                })
            })
            .unwrap_or(false)
    };
    let mut files = Vec::new();
    if let Some(cwd) = cwd.filter(|cwd| trusted(cwd)) {
        files.push(cwd.join(".pi/mcp.json"));
    }
    files.push(root.join("mcp.json"));
    files
}
