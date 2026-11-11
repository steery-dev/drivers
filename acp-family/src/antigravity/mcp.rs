//! `mcp/list`: the MCP servers a conversation of Antigravity CLI starts, read from the files
//! its MCP page names and as Antigravity CLI 1.5.5 was seen to load them, offline, with
//! servers that only list one tool each:
//!
//! - `<config>/mcp_config.json`, the person's own, then `<cwd>/.agents/mcp_config.json`,
//!   the workspace's, each under `mcpServers` by name. Where both name a server, the
//!   person's is the one that starts.
//! - Each plugin's `mcp_config.json`, the person's in `<config>/plugins/<plugin>/` and the
//!   workspace's in `<cwd>/.agents/plugins/<plugin>/`, its servers named
//!   `<plugin>_<server>`. A plugin is on unless `<config>/config.json` has
//!   `plugins.<plugin>.enabled` false; Antigravity adds a plugin it has not seen there, on.
//! - A server with `disabled` true does not start; one with no `command` and no
//!   `serverUrl` (or `url`) cannot.
//!
//! The list is never complete: the workspace is the folder Antigravity was opened in, which
//! a `--add-dir` widens, and a plugin installed from the marketplace while it runs is read
//! on the next start.

use std::path::{Path, PathBuf};

use serde_json::Value;
use steery_driver_common::mcp::{entries, json};
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};

const FILE: &str = "mcp_config.json";
const PLUGINS: &str = "plugins";

/// The servers a conversation of Antigravity, in the person's home `home`, starts in
/// `asked.cwd`; asked of no folder, the person's own.
pub fn list(home: &Path, asked: McpList) -> McpServers {
    let config = super::config(home);
    let enablement = config.join("config.json");
    let mut watch = vec![config.join(FILE), config.join(PLUGINS), enablement.clone()];
    let plugins_on = json(&enablement);
    let mut servers: Vec<(String, Option<String>, bool)> = Vec::new();
    let mut add = |name: String, entry: &Value| {
        if servers.iter().any(|(known, ..)| *known == name) {
            return;
        }
        let url = ["serverUrl", "url"]
            .iter()
            .find_map(|key| entry.get(*key).and_then(Value::as_str))
            .filter(|url| !url.is_empty());
        let command = entry
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| !command.is_empty());
        if !command && url.is_none() {
            return;
        }
        let on = entry.get("disabled").and_then(Value::as_bool) != Some(true);
        servers.push((name, url.filter(|_| !command).map(str::to_owned), on));
    };

    let workspace = asked.cwd.as_deref().map(|cwd| cwd.join(".agents"));
    let mut files = vec![config.join(FILE)];
    if let Some(workspace) = &workspace {
        files.push(workspace.join(FILE));
        watch.push(workspace.join(FILE));
        watch.push(workspace.join(PLUGINS));
    }
    for file in &files {
        for (name, entry) in json(file).iter().flat_map(entries) {
            add(name.to_owned(), entry);
        }
    }
    let mut folders = vec![config.join(PLUGINS)];
    folders.extend(workspace.map(|workspace| workspace.join(PLUGINS)));
    for (plugin, folder) in folders.iter().flat_map(|folder| plugins(folder)) {
        let off = plugins_on
            .as_ref()
            .and_then(|on| on.pointer(&format!("/plugins/{plugin}/enabled")))
            .and_then(Value::as_bool)
            == Some(false);
        if off {
            continue;
        }
        let file = folder.join(FILE);
        watch.push(file.clone());
        for (name, entry) in json(&file).iter().flat_map(entries) {
            add(format!("{plugin}_{name}"), entry);
        }
    }
    McpServers {
        servers: servers
            .into_iter()
            .filter(|(.., on)| *on)
            .map(|(name, url, _)| McpServer::new(&name, url.as_deref()))
            .collect(),
        complete: false,
        watch,
    }
}

/// The plugins in `folder`, by their folder's name, in name order.
fn plugins(folder: &Path) -> Vec<(String, PathBuf)> {
    let mut found: Vec<(String, PathBuf)> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().join("plugin.json").is_file())
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
        .collect();
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The servers of the recorded `mcp2` scene, where each that started listed its tool to
    /// the model: the person's `dup` and `g`, the workspace's `w` (its `dup` lost to the
    /// person's), and each plugin's `ps`; `off` is disabled.
    #[test]
    fn lists_the_servers_antigravity_started() {
        let home =
            std::env::temp_dir().join(format!("steery-antigravity-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let cwd = home.join("work");
        let write = |path: PathBuf, text: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        let config = home.join(".gemini/config");
        write(
            config.join(FILE),
            r#"{"mcpServers":{"dup":{"command":"python3"},"g":{"command":"python3"},
                "off":{"command":"python3","disabled":true}}}"#,
        );
        write(
            cwd.join(".agents").join(FILE),
            r#"{"mcpServers":{"dup":{"serverUrl":"http://w/mcp"},"w":{"command":"python3"},
                "u":{"url":"http://u/mcp"},"none":{}}}"#,
        );
        for (folder, plugin) in [
            (&config, "gp"),
            (&cwd.join(".agents"), "wp"),
            (&config, "gq"),
        ] {
            write(
                folder.join(PLUGINS).join(plugin).join("plugin.json"),
                &format!(r#"{{"name":"{plugin}"}}"#),
            );
            write(
                folder.join(PLUGINS).join(plugin).join(FILE),
                r#"{"mcpServers":{"ps":{"command":"python3"}}}"#,
            );
        }
        write(
            config.join("config.json"),
            r#"{"plugins":{"gp":{"enabled":true},"gq":{"enabled":false}}}"#,
        );

        let listed = list(&home, McpList { cwd: Some(cwd) });
        let names: Vec<(&str, Option<&str>)> = listed
            .servers
            .iter()
            .map(|server| (server.server.as_str(), server.url.as_deref()))
            .collect();
        assert_eq!(
            names,
            [
                ("dup", None),
                ("g", None),
                ("u", Some("http://u/mcp")),
                ("w", None),
                ("gp_ps", None),
                ("wp_ps", None),
            ]
        );
        assert!(!listed.complete);
        let mine = list(&home, McpList { cwd: None });
        assert_eq!(mine.servers.len(), 3);
        let _ = std::fs::remove_dir_all(&home);
    }
}
