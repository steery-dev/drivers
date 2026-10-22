//! `mcp/list`: the MCP servers Claude Code loads, read from its own files as it reads them
//! (PROTOCOL.md §4.1), each as its name and a remote one's URL without a secret. As
//! Claude Code 2.1 does, read off `claude mcp list` in folders built for it:
//!
//! - user scope: `mcpServers` in `.claude.json`;
//! - in a folder, first the project's local scope, `projects[<project>].mcpServers` of
//!   the same file, `<project>` being the folder's git repository root (a worktree's main
//!   checkout), else the folder itself;
//! - then project scope: each `.mcp.json` from the filesystem root down to the folder, the
//!   nearer one winning a name, a server counting only once approved and not rejected. A
//!   server is approved by `enabledMcpjsonServers` or `enableAllProjectMcpServers` in the
//!   person's `settings.json`, by the project's `enabledMcpjsonServers` in
//!   `.claude.json`, or by the folder's own `.claude/settings.json` and
//!   `.claude/settings.local.json` once the person trusted the project
//!   (`hasTrustDialogAccepted`); `disabledMcpjsonServers` in any of them rejects it;
//! - then user scope; the first scope naming a server wins, and the project's
//!   `disabledMcpServers` turns one off.
//!
//! Plugins' servers, managed configuration and claude.ai connectors, which Claude Code
//! fetches as it runs, are not read here.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use steery_sdk::Error;
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};

/// The servers Claude Code, its configuration in `root`, loads where `asked` says.
pub fn list(home: &Path, root: &Path, asked: McpList) -> Result<McpServers, Error> {
    if asked.place.as_deref().is_some_and(|place| !place.is_absolute()) {
        return Err(Error::invalid("`place` is no absolute folder"));
    }
    let config = read(&crate::claude_json(home, root)).unwrap_or(Value::Null);
    let mut scopes: Vec<Map<String, Value>> = Vec::new();
    let mut off = Vec::new();
    if let Some(place) = asked.place {
        let place = std::fs::canonicalize(&place).unwrap_or(place);
        let key = project(&place);
        let project = key
            .to_str()
            .and_then(|key| config.get("projects")?.get(key))
            .unwrap_or(&Value::Null);
        off = names(project, "disabledMcpServers");
        let person = read(&root.join("settings.json"));
        let folder = [
            read(&place.join(".claude/settings.json")),
            read(&place.join(".claude/settings.local.json")),
        ];
        let trusted = project.get("hasTrustDialogAccepted") == Some(&Value::Bool(true));
        let mut approving: Vec<&Value> = person.iter().collect();
        if trusted {
            approving.extend(folder.iter().flatten());
        }
        let rejected: Vec<String> = person
            .iter()
            .chain(folder.iter().flatten())
            .chain([project])
            .flat_map(|settings| names(settings, "disabledMcpjsonServers"))
            .collect();
        let approved = |name: &String| {
            names(project, "enabledMcpjsonServers").contains(name)
                || approving.iter().any(|settings| {
                    settings.get("enableAllProjectMcpServers") == Some(&Value::Bool(true))
                        || names(settings, "enabledMcpjsonServers").contains(name)
                })
        };
        let mut shared = Map::new();
        let folders: Vec<&Path> = place.ancestors().collect();
        for folder in folders.into_iter().rev() {
            if let Some(config) = read(&folder.join(".mcp.json")) {
                shared.extend(servers_in(&config));
            }
        }
        shared.retain(|name, _| approved(name) && !rejected.contains(name));
        scopes.extend([servers_in(project), shared]);
    }
    scopes.push(servers_in(&config));
    let mut seen = Vec::new();
    let mut servers = Vec::new();
    for (name, entry) in scopes.into_iter().flatten() {
        if seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());
        // An `sdk` server only an SDK host registers; Claude Code skips it.
        if name == crate::STEERY
            || off.contains(&name)
            || entry.get("type").and_then(Value::as_str) == Some("sdk")
        {
            continue;
        }
        servers.push(match entry.get("url").and_then(Value::as_str) {
            Some(url) => McpServer::remote(name, url),
            None => McpServer::local(name),
        });
    }
    Ok(McpServers { servers })
}

/// The project Claude Code keys a folder's state on: the root of the git repository the
/// folder is in, a worktree's being its main checkout's; else the folder itself.
fn project(place: &Path) -> PathBuf {
    for folder in place.ancestors() {
        let git = folder.join(".git");
        if git.is_dir() {
            return folder.to_owned();
        }
        if git.is_file() {
            return main_checkout(&git).unwrap_or_else(|| folder.to_owned());
        }
    }
    place.to_owned()
}

/// A worktree's main checkout, from its `.git` file: `gitdir:` names the worktree's git
/// folder, whose `commondir` names the repository's. A submodule's has none.
fn main_checkout(git: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(git).ok()?;
    let gitdir = git.parent()?.join(text.trim().strip_prefix("gitdir:")?.trim());
    let common = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    let common = std::fs::canonicalize(gitdir.join(common.trim())).ok()?;
    common.parent().map(Path::to_owned)
}

fn names(settings: &Value, key: &str) -> Vec<String> {
    settings
        .get(key)
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

fn servers_in(config: &Value) -> Map<String, Value> {
    match config.get("mcpServers") {
        Some(Value::Object(servers)) => servers.clone(),
        _ => Map::new(),
    }
}

fn read(file: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(file).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A person's servers and a folder's, as Claude Code loads them: a remote server is its
    /// name and its URL without the token in its query, its user or its headers; a stdio
    /// server its name alone; Steery's own entry is absent, and so is a project's server
    /// nobody approved, one rejected, and one approved only by an untrusted repository's own
    /// settings.
    #[test]
    fn claude_code_names_the_servers_it_loads_and_no_secret() {
        let dir = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("claude-code-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        let root = crate::default_root(&home);
        let (trusted, untrusted) = (dir.join("trusted"), dir.join("untrusted"));
        write(
            &home.join(".claude.json"),
            &serde_json::json!({
                "mcpServers": {
                    "docs": {"type": "http", "url": "https://person:pw@mcp.example.com:8443/mcp?token=SECRET#top",
                             "headers": {"Authorization": "Bearer SECRET"}},
                    "files": {"command": "npx", "args": ["--key", "SECRET"], "env": {"KEY": "SECRET"}},
                    "steery": {"type": "http", "url": "http://127.0.0.1:7777/mcp"}
                },
                "projects": {
                    trusted.to_str().unwrap(): {
                        "hasTrustDialogAccepted": true,
                        "mcpServers": {"mine": {"command": "local", "env": {"KEY": "SECRET"}}},
                        "enabledMcpjsonServers": ["approved"],
                        "disabledMcpServers": ["files"]
                    }
                }
            })
            .to_string(),
        );
        write(
            &root.join("settings.json"),
            r#"{"enabledMcpjsonServers": ["byperson", "rejected"]}"#,
        );
        let shared = r#"{"mcpServers": {
            "approved": {"type": "http", "url": "https://approved.example.com/mcp?key=SECRET"},
            "byperson": {"command": "a"},
            "byrepo": {"command": "b"},
            "pending": {"command": "c"},
            "rejected": {"command": "d"}
        }}"#;
        for repo in [&trusted, &untrusted] {
            std::fs::create_dir_all(repo.join(".git")).unwrap();
            write(&repo.join(".mcp.json"), shared);
            write(
                &repo.join("sub/.claude/settings.json"),
                r#"{"enabledMcpjsonServers": ["byrepo"], "disabledMcpjsonServers": ["rejected"]}"#,
            );
        }
        write(&dir.join(".mcp.json"), r#"{"mcpServers": {"above": {"command": "e"}}}"#);

        let ask = |place: Option<PathBuf>| list(&home, &root, McpList { place }).unwrap().servers;
        let named = |name: &str, url: Option<&str>| McpServer {
            name: name.into(),
            url: url.map(str::to_owned),
        };
        let (docs, files) = (
            named("docs", Some("https://mcp.example.com:8443/mcp")),
            named("files", None),
        );
        assert_eq!(ask(None), [docs.clone(), files.clone()]);
        let listed = ask(Some(trusted.join("sub")));
        assert_eq!(
            listed,
            [
                named("mine", None),
                named("approved", Some("https://approved.example.com/mcp")),
                named("byperson", None),
                named("byrepo", None),
                docs.clone(),
            ]
        );
        assert_eq!(
            ask(Some(untrusted.join("sub"))),
            [named("byperson", None), docs, files]
        );
        let said = serde_json::to_string(&listed).unwrap();
        assert!(!said.contains("SECRET") && !said.contains("person:"), "{said}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
