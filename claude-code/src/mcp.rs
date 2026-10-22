//! `mcp/list`: the MCP servers Claude Code loads, from its own files, as it ranks them.
//!
//! - Everywhere: the user-scope servers, `mcpServers` in its state file
//!   ([`crate::state_file`]).
//! - In a folder, ahead of those: the local-scope servers, that file's
//!   `projects["<folder>"].mcpServers`, keyed by the exact folder; then the folder's
//!   `.mcp.json`, only where the folder is trusted (`hasTrustDialogAccepted`) and the server
//!   approved (`enableAllProjectMcpServers` or `enabledMcpjsonServers`) and not rejected
//!   (`disabledMcpjsonServers`) by the person's settings, the folder's, or its entry in the
//!   state file. The folder's `disabledMcpServers` turns any server off there. Approval
//!   counts because a conversation Steery starts runs as `claude --bg` ([`crate::turn`]):
//!   an interactive background session, which waits on it, not a `-p` run, which skips it.
//!
//! An entry with a `url` and no `type`, or of `type: "sdk"`, Claude Code skips, and so does
//! this list. The list is incomplete where an administrator's managed configuration is
//! here, a plugin is turned on, or a claude.ai account is signed in, whose connectors
//! Claude Code fetches as it starts.

use std::path::Path;

use serde_json::Value;
use steery_driver_common::mcp::{self, Servers};
use steery_sdk::wire::harness::{McpList, McpServers};

/// Where an administrator's managed configuration sits on this system.
#[cfg(target_os = "macos")]
const MANAGED: &str = "/Library/Application Support/ClaudeCode";
#[cfg(not(target_os = "macos"))]
const MANAGED: &str = "/etc/claude-code";

/// The servers a conversation started in `asked.cwd`, or anywhere, loads, for the person
/// whose home is `home` and whose Claude Code configuration root is `root`.
pub fn list(home: &Path, root: &Path, asked: McpList) -> McpServers {
    let state_file = crate::state_file(home, root);
    let state = mcp::json(&state_file).unwrap_or(Value::Null);
    let managed = Path::new(MANAGED);
    let mut watch = vec![
        state_file,
        root.join("settings.json"),
        managed.join("managed-mcp.json"),
        managed.join("managed-settings.json"),
    ];
    let mut settings: Vec<Value> = mcp::json(&root.join("settings.json")).into_iter().collect();
    let mut servers = Servers::default();
    let mut off = Vec::new();
    if let Some(cwd) = asked.cwd.as_deref() {
        let project = cwd
            .to_str()
            .and_then(|folder| state.get("projects")?.get(folder))
            .unwrap_or(&Value::Null);
        add(&mut servers, project, |_| true);
        let shared = [
            cwd.join(".claude/settings.json"),
            cwd.join(".claude/settings.local.json"),
        ];
        let trusted = project
            .get("hasTrustDialogAccepted")
            .and_then(Value::as_bool)
            == Some(true);
        if trusted {
            settings.extend(shared.iter().filter_map(|file| mcp::json(file)));
            let decided: Vec<&Value> = settings.iter().chain([project]).collect();
            let approved = |name: &str| {
                decided.iter().any(|said| {
                    said.get("enableAllProjectMcpServers")
                        .and_then(Value::as_bool)
                        == Some(true)
                        || mcp::names(said, "enabledMcpjsonServers").any(|n| n == name)
                })
            };
            let rejected = |name: &str| {
                decided
                    .iter()
                    .any(|said| mcp::names(said, "disabledMcpjsonServers").any(|n| n == name))
            };
            if let Some(file) = mcp::json(&cwd.join(".mcp.json")) {
                add(&mut servers, &file, |name| {
                    approved(name) && !rejected(name)
                });
            }
        }
        off.extend(mcp::names(project, "disabledMcpServers").map(str::to_owned));
        watch.push(cwd.join(".mcp.json"));
        watch.extend(shared);
    }
    add(&mut servers, &state, |_| true);
    for name in &off {
        servers.off(name);
    }
    let plugins = settings.iter().any(|said| {
        said.get("enabledPlugins")
            .and_then(Value::as_object)
            .is_some_and(|plugins| plugins.values().any(|on| on.as_bool() == Some(true)))
    });
    let managed = watch[2..4].iter().any(|file| file.exists());
    McpServers {
        servers: servers.on(),
        complete: !plugins && !managed && state.get("oauthAccount").is_none(),
        watch,
    }
}

/// Each server `config` names that Claude Code loads and `loads` keeps.
fn add(servers: &mut Servers, config: &Value, loads: impl Fn(&str) -> bool) {
    for (name, entry) in mcp::entries(config) {
        let kind = entry.get("type").and_then(Value::as_str);
        if kind == Some("sdk") || (kind.is_none() && entry.get("url").is_some()) || !loads(name) {
            continue;
        }
        servers.add(name, entry, true);
    }
}
