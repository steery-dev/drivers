//! OpenCode's configuration files, as the MCP servers they load. OpenCode reads
//! `opencode.json` and `opencode.jsonc` (JSON with comments and trailing commas) from its
//! system folder, its global folder, and each folder from the project's root down to the
//! working folder, each folder's `.opencode/` too. A later file overrides an earlier one, a
//! server at a time.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};

use super::place::Place;

const NAMES: [&str; 2] = ["opencode.json", "opencode.jsonc"];

/// OpenCode's system folder on this platform.
fn system() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/opencode")
    } else {
        PathBuf::from("/etc/opencode")
    }
}

/// The files OpenCode reads for `cwd`, lowest priority first; for the person alone where
/// `cwd` is absent. `OPENCODE_CONFIG` names one more file, after the global folder's.
pub(crate) fn files(place: &Place, cwd: Option<&Path>) -> Vec<PathBuf> {
    let globals = [system(), place.config.clone()];
    let mut found: Vec<PathBuf> = globals
        .iter()
        .flat_map(|folder| NAMES.iter().map(move |name| folder.join(name)))
        .collect();
    found.extend(
        std::env::var_os("OPENCODE_CONFIG")
            .filter(|file| !file.is_empty())
            .map(PathBuf::from),
    );
    // A conversation in a global folder reads no project files.
    if let Some(cwd) = cwd.filter(|cwd| !globals.iter().any(|global| global == cwd)) {
        let project: Vec<&Path> = cwd
            .ancestors()
            .filter(|folder| !globals.iter().any(|global| global == folder))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let named = |folder: &Path| NAMES.map(|name| folder.join(name));
        found.extend(project.iter().flat_map(|folder| named(folder)));
        found.extend(
            project
                .iter()
                .flat_map(|folder| named(&folder.join(".opencode"))),
        );
    }
    found
}

/// A JSONC text as JSON: its comments and trailing commas dropped.
pub(crate) fn parse(text: &str) -> Result<Value, serde_json::Error> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut string = false;
    while let Some(c) = chars.next() {
        if string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                out.push(' ');
            }
            ('}' | ']', _) => {
                // A comma right before a closer is a trailing one; comments already left.
                let kept = out.trim_end_matches(char::is_whitespace).len();
                if out[..kept].ends_with(',') {
                    out.truncate(kept - 1);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    serde_json::from_str(&out)
}

/// The servers of one file's `mcp`, as OpenCode 2 normalizes it: each key of `mcp` with a
/// `type` is a server of OpenCode 1's shape; `mcp.servers` holds OpenCode 2's, which win.
/// An entry with only `enabled` is one OpenCode 2 drops.
fn servers(document: &Value) -> Vec<(&String, &Map<String, Value>)> {
    fn typed(server: &Value) -> Option<&Map<String, Value>> {
        server
            .as_object()
            .filter(|server| server.contains_key("type"))
    }
    let Some(mcp) = document.get("mcp").and_then(Value::as_object) else {
        return Vec::new();
    };
    // A `servers` or `timeout` key with a `type` is an OpenCode 1 server of that name.
    let mut found: Vec<(&String, &Map<String, Value>)> = mcp
        .iter()
        .filter_map(|(name, server)| Some((name, typed(server)?)))
        .collect();
    let nested = mcp
        .get("servers")
        .filter(|servers| typed(servers).is_none())
        .and_then(Value::as_object);
    for (name, server) in nested.into_iter().flatten() {
        if let Some(server) = typed(server) {
            found.retain(|(seen, _)| *seen != name);
            found.push((name, server));
        }
    }
    found
}

/// `mcp/list`: the servers OpenCode loads, a later file's entry replacing an earlier one's.
pub(crate) fn mcp(place: &Place, asked: McpList) -> McpServers {
    let watch = files(place, asked.cwd.as_deref());
    // An administrator's managed preferences and a configuration given in the environment
    // add servers that no file here holds.
    let mut complete = managed().is_none()
        && std::env::var_os("OPENCODE_CONFIG_CONTENT").is_none_or(|content| content.is_empty());
    let mut loaded: Vec<(String, Option<String>, bool)> = Vec::new();
    for file in &watch {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let Ok(document) = parse(&text) else {
            complete = false;
            continue;
        };
        // A plugin's `config` hook can add servers too: OpenCode 1's `plugin` list, OpenCode
        // 2's `plugins`.
        let listed = |key: &str| {
            document[key]
                .as_array()
                .is_some_and(|list| !list.is_empty())
        };
        if listed("plugin") || listed("plugins") {
            complete = false;
        }
        for (name, server) in servers(&document) {
            let on = server.get("enabled").and_then(Value::as_bool) != Some(false)
                && server.get("disabled").and_then(Value::as_bool) != Some(true);
            let url = (server.get("type").and_then(Value::as_str) == Some("remote"))
                .then(|| server.get("url").and_then(Value::as_str))
                .flatten()
                .map(str::to_owned);
            loaded.retain(|(seen, _, _)| seen != name);
            loaded.push((name.clone(), url, on));
        }
    }
    // Or a module in a config folder's `plugin` or `plugins` folder.
    let mut folders: Vec<&Path> = watch.iter().filter_map(|file| file.parent()).collect();
    folders.dedup();
    if folders
        .into_iter()
        .filter(|folder| {
            *folder == place.config || *folder == system() || folder.ends_with(".opencode")
        })
        .any(holds_plugins)
    {
        complete = false;
    }
    McpServers {
        servers: loaded
            .into_iter()
            .filter(|(_, _, on)| *on)
            .map(|(server, url, _)| McpServer::new(server, url.as_deref()))
            .collect(),
        complete,
        watch,
    }
}

/// A config folder with a module OpenCode loads as a plugin: a `.ts` or `.js` file, or a
/// folder, in its `plugin` or `plugins` folder.
fn holds_plugins(folder: &Path) -> bool {
    ["plugin", "plugins"].iter().any(|name| {
        std::fs::read_dir(folder.join(name)).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                let path = entry.path();
                path.is_dir()
                    || path
                        .extension()
                        .is_some_and(|ext| ext == "ts" || ext == "js")
            })
        })
    })
}

/// The macOS managed preferences that OpenCode reads, where an administrator deployed them.
fn managed() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let user = std::env::var("USER").unwrap_or_default();
    [
        PathBuf::from(format!(
            "/Library/Managed Preferences/{user}/ai.opencode.managed.plist"
        )),
        PathBuf::from("/Library/Managed Preferences/ai.opencode.managed.plist"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

/// The names of the servers OpenCode loads for the person, to tell an MCP tool by its name.
pub(crate) fn server_names(place: &Place, cwd: Option<&Path>) -> Vec<String> {
    let mut names: Vec<String> = mcp(
        place,
        McpList {
            cwd: cwd.map(Path::to_path_buf),
        },
    )
    .servers
    .into_iter()
    .map(|server| server.server)
    .collect();
    if !names.iter().any(|name| name == "steery") {
        names.push("steery".into());
    }
    // The longest first, so `a_b` wins over `a` for a tool `a_b_c`.
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    names
}
