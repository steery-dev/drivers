//! `mcp/list`: the MCP servers that a conversation of Gemini CLI starts, read from the files
//! that Gemini merges, as `cli/src/config/settings.ts` and `core/src/tools/mcp-client-manager.ts`
//! of 0.67 read them.
//!
//! - Settings, JSON with comments, under `mcpServers` by name: the system defaults, the
//!   person's `<root>/settings.json`, the project's `<cwd>/.gemini/settings.json` where the
//!   folder is trusted, then the system settings. A later file replaces a server of the same
//!   name whole. `mcp.serverCommand` adds a server named `mcp`.
//! - Each active extension, `<root>/extensions/*/gemini-extension.json`: its servers join
//!   them; where settings name the same server, the fields of the settings win.
//! - Then `mcp.allowed` (where any file has it: the names in every list that has one) and
//!   `mcp.excluded` (the names in any), by exact name, and `<root>/mcp-server-enablement.json`,
//!   which turns a server off by its name in lower case.
//!
//! Gemini starts no MCP server at all in a folder it does not trust, so a folder that is not
//! trusted lists none. A server's URL is its `httpUrl`, else its `url`. The list is never
//! complete: an administrator's remote settings can allow, rewrite or add servers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use steery_sdk::Error;
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};

use crate::jsonc::jsonc;

/// The servers a conversation of Gemini, whose root is `root`, starts in `asked.cwd`; asked
/// of no folder, the person's own servers, as no project and no trust limit them.
pub fn list(_home: &Path, root: &Path, asked: McpList) -> Result<McpServers, Error> {
    Ok(servers(root, asked.cwd.as_deref(), &Places::here(root)))
}

/// The files outside Gemini's root that Gemini reads its settings and trust from, and what
/// the environment says of trust.
struct Places {
    /// The system settings, which override all others.
    system: PathBuf,
    /// The system defaults, which all others override.
    defaults: PathBuf,
    /// `trustedFolders.json`.
    trusted: PathBuf,
    /// Trust the environment decides: `GEMINI_RESTRICTED_MODE`, `GEMINI_CLI_TRUST_WORKSPACE`.
    trust: Option<bool>,
}

impl Places {
    /// As Gemini finds them in this environment (`storage.ts`, `settings.ts`, `trust.ts`).
    fn here(root: &Path) -> Self {
        let (system, defaults) = system();
        let env = |name: &str| std::env::var(name).ok();
        let workspace = env("GEMINI_CLI_TRUST_WORKSPACE");
        let trust = if env("GEMINI_RESTRICTED_MODE").as_deref() == Some("true")
            || workspace.as_deref() == Some("false")
        {
            Some(false)
        } else {
            (workspace.as_deref() == Some("true")).then_some(true)
        };
        Self {
            system,
            defaults,
            trusted: var_path("GEMINI_CLI_TRUSTED_FOLDERS_PATH")
                .unwrap_or_else(|| root.join("trustedFolders.json")),
            trust,
        }
    }
}

/// Gemini's system settings and system defaults files: `GEMINI_CLI_SYSTEM_SETTINGS_PATH`,
/// else the platform's, and `GEMINI_CLI_SYSTEM_DEFAULTS_PATH`, else `system-defaults.json`
/// beside the system settings.
pub(super) fn system() -> (PathBuf, PathBuf) {
    let system = var_path("GEMINI_CLI_SYSTEM_SETTINGS_PATH").unwrap_or_else(|| {
        PathBuf::from(if cfg!(target_os = "macos") {
            "/Library/Application Support/GeminiCli/settings.json"
        } else {
            "/etc/gemini-cli/settings.json"
        })
    });
    let defaults = var_path("GEMINI_CLI_SYSTEM_DEFAULTS_PATH").unwrap_or_else(|| {
        system
            .parent()
            .unwrap_or(Path::new("/"))
            .join("system-defaults.json")
    });
    (system, defaults)
}

/// The path in the environment variable `name`, where it is set and not empty.
fn var_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// A settings file as Gemini reads it; an empty object where it is missing or unreadable.
pub(super) fn settings(path: &Path) -> Value {
    jsonc(path)
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Map::new()))
}

/// The value at `pointer` in the last of `scopes`, in merge order, that holds one.
pub(super) fn last<'a>(scopes: &[&'a Value], pointer: &str) -> Option<&'a Value> {
    scopes.iter().rev().find_map(|scope| scope.pointer(pointer))
}

fn servers(root: &Path, cwd: Option<&Path>, places: &Places) -> McpServers {
    let user_path = root.join("settings.json");
    let enablement = root.join("mcp-server-enablement.json");
    let extensions = root.join("extensions");
    let extension_enablement = extensions.join("extension-enablement.json");
    let mut watch = vec![
        places.system.clone(),
        places.defaults.clone(),
        user_path.clone(),
        places.trusted.clone(),
        enablement.clone(),
        extensions.clone(),
        extension_enablement.clone(),
    ];
    let system = settings(&places.system);
    let defaults = settings(&places.defaults);
    let user = settings(&user_path);

    let mut workspace = Value::Object(Map::new());
    if let Some(cwd) = cwd {
        let project = cwd.join(".gemini/settings.json");
        watch.push(project.clone());
        let enabled = last(
            &[&defaults, &user, &system],
            "/security/folderTrust/enabled",
        )
        .and_then(Value::as_bool)
        .unwrap_or(true);
        let trusted = places
            .trust
            .unwrap_or_else(|| !enabled || trusts(&places.trusted, cwd));
        if !trusted {
            return McpServers {
                servers: Vec::new(),
                complete: false,
                watch,
            };
        }
        // Gemini's home folder is the parent of its root: there the project's file is the
        // person's own.
        if real(cwd) != real(root.parent().unwrap_or(root)) {
            workspace = settings(&project);
        }
    }
    let scopes = [&defaults, &user, &workspace, &system];

    // Each server by name, with whether an extension gave it.
    let mut configured: BTreeMap<String, (Map<String, Value>, bool)> = BTreeMap::new();
    for scope in scopes {
        for (name, entry) in scope
            .get("mcpServers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if let Some(entry) = entry.as_object() {
                configured.insert(name.clone(), (entry.clone(), false));
            }
        }
    }
    if let Some(command) = last(&scopes, "/mcp/serverCommand")
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty())
    {
        let entry = Map::from_iter([("command".to_owned(), Value::from(command))]);
        configured.insert("mcp".to_owned(), (entry, false));
    }

    let at = cwd.unwrap_or(Path::new("/"));
    let block_git =
        last(&scopes, "/security/blockGitExtensions").and_then(Value::as_bool) == Some(true);
    let allow_listed = last(&scopes, "/security/allowedExtensions")
        .and_then(Value::as_array)
        .is_some_and(|patterns| !patterns.is_empty());
    let rules = steery_driver_common::mcp::json(&extension_enablement);
    for extension in active(
        &extensions,
        rules.as_ref(),
        at,
        (allow_listed, block_git),
        &mut watch,
    ) {
        for (name, entry) in steery_driver_common::mcp::entries(&extension) {
            let Some(entry) = entry.as_object() else {
                continue;
            };
            match configured.get_mut(name) {
                None => {
                    configured.insert(name.to_owned(), (entry.clone(), true));
                }
                // The settings' fields win; a second extension's server of a name is skipped.
                Some((fields, false)) => {
                    for (key, value) in entry {
                        fields.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                    configured.get_mut(name).expect("present").1 = true;
                }
                Some((_, true)) => {}
            }
        }
    }

    let allowed = allowed(&[&system, &defaults, &user, &workspace]);
    let excluded: Vec<&str> = scopes
        .iter()
        .flat_map(|scope| strings(scope, "/mcp/excluded"))
        .collect();
    let off = disabled(&enablement);
    let servers = configured
        .into_iter()
        .filter(|(_, (fields, _))| {
            ["command", "url", "httpUrl"]
                .iter()
                .any(|key| truthy(fields.get(*key)))
        })
        .filter(|(name, _)| {
            allowed
                .as_ref()
                .is_none_or(|allowed| allowed.is_empty() || allowed.contains(name))
        })
        .filter(|(name, _)| !excluded.contains(&name.as_str()))
        .filter(|(name, _)| {
            off.as_ref()
                .is_some_and(|off| !off.contains(&name.trim().to_lowercase()))
        })
        .map(|(name, (fields, _))| {
            let url = ["httpUrl", "url"].iter().find_map(|key| {
                fields
                    .get(*key)
                    .and_then(Value::as_str)
                    .filter(|url| !url.is_empty())
            });
            McpServer::new(name, url)
        })
        .collect();
    McpServers {
        servers,
        complete: false,
        watch,
    }
}

/// Whether a JavaScript `||` takes `value`.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(on)) => *on,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        Some(_) => true,
    }
}

/// The strings of the array at `pointer` in `value`.
fn strings<'a>(value: &'a Value, pointer: &str) -> impl Iterator<Item = &'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

/// `mcp.allowed` as Gemini consolidates it: none where no scope has a list, else the names
/// of the first list that every other list holds, compared in lower case.
fn allowed(scopes: &[&Value]) -> Option<Vec<String>> {
    let mut lists = scopes.iter().filter_map(|scope| {
        scope
            .pointer("/mcp/allowed")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).collect::<Vec<_>>())
    });
    let first = lists.next()?;
    let others: Vec<Vec<String>> = lists
        .map(|list| list.iter().map(|name| name.trim().to_lowercase()).collect())
        .collect();
    Some(
        first
            .into_iter()
            .filter(|name| {
                let name = name.trim().to_lowercase();
                others.iter().all(|list| list.contains(&name))
            })
            .map(str::to_owned)
            .collect(),
    )
}

/// The names, in lower case, that `mcp-server-enablement.json` turns off; none where the file
/// exists but is no JSON object, which turns every server off.
fn disabled(path: &Path) -> Option<Vec<String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let config: Value = serde_json::from_str(&text).ok()?;
    Some(
        config
            .as_object()?
            .iter()
            .filter(|(_, state)| state.get("enabled") == Some(&Value::Bool(false)))
            .map(|(name, _)| name.trim().to_lowercase())
            .collect(),
    )
}

/// The `gemini-extension.json` of each extension in `dir` that is active in the folder `at`,
/// in the order of their folders' names. Each file read joins `watch`. The pair says
/// whether `security.allowedExtensions` lists any pattern (then an extension needs its
/// install source; the patterns themselves are not matched here) and whether
/// `security.blockGitExtensions` is on.
fn active(
    dir: &Path,
    rules: Option<&Value>,
    at: &Path,
    (allow_listed, block_git): (bool, bool),
    watch: &mut Vec<PathBuf>,
) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut folders: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| !path.to_string_lossy().ends_with(".lock") && path.is_dir())
        .collect();
    folders.sort();
    let at = slashed(&at.to_string_lossy());
    folders
        .into_iter()
        .filter_map(|folder| {
            let install = folder.join(".gemini-extension-install.json");
            watch.push(install.clone());
            let install = steery_driver_common::mcp::json(&install);
            let kind = install
                .as_ref()
                .and_then(|i| i.get("type"))
                .and_then(Value::as_str);
            let source = install
                .as_ref()
                .and_then(|i| i.get("source"))
                .and_then(Value::as_str);
            if allow_listed && source.is_none()
                || !allow_listed && block_git && matches!(kind, Some("git" | "github-release"))
            {
                return None;
            }
            let home = match (kind, source) {
                (Some("link"), Some(source)) => PathBuf::from(source),
                _ => folder,
            };
            let file = home.join("gemini-extension.json");
            watch.push(file.clone());
            let config = steery_driver_common::mcp::json(&file)?;
            let name = config
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())?;
            truthy(config.get("version")).then_some(())?;
            enabled(rules, name, &at).then_some(config)
        })
        .collect()
}

/// Whether `extension-enablement.json` leaves the extension `name` on in the folder `at`
/// (slashed at both ends): the last of its rules that matches decides; on by default.
fn enabled(rules: Option<&Value>, name: &str, at: &str) -> bool {
    let mut on = true;
    for rule in rules
        .and_then(|rules| {
            rules.pointer(&format!(
                "/{}/overrides",
                name.replace('~', "~0").replace('/', "~1")
            ))
        })
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let (off, base) = rule
            .strip_prefix('!')
            .map_or((false, rule), |base| (true, base));
        let matches = match base.strip_suffix('*') {
            // `<base>*` is Gemini's glob `^<base>(/.*)?$` where `base` ends in `/`, else
            // `^<base>.*$`.
            Some(base) => match base.strip_suffix('/') {
                Some(folder) => at == folder || at.starts_with(&format!("{folder}/")),
                None => at.starts_with(base),
            },
            None => at == base,
        };
        if matches {
            on = !off;
        }
    }
    on
}

/// `path` with forward slashes and one at each end, as Gemini compares folders.
fn slashed(path: &str) -> String {
    let mut path = path.replace('\\', "/");
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    if !path.ends_with('/') {
        path.push('/');
    }
    path
}

/// Whether `trustedFolders.json` at `file` trusts `folder`: the longest rule whose folder
/// holds it decides; no rule, or a file Gemini refuses, trusts nothing.
fn trusts(file: &Path, folder: &Path) -> bool {
    let Some(Value::Object(rules)) = jsonc(file) else {
        return false;
    };
    let folder = real(folder);
    let mut longest: Option<(usize, bool)> = None;
    for (rule, level) in &rules {
        let trusted = match level.as_str() {
            Some("TRUST_FOLDER" | "TRUST_PARENT") => true,
            Some("DO_NOT_TRUST") => false,
            // Gemini stops at a level it does not know.
            _ => return false,
        };
        let path = Path::new(rule);
        let holder = if level == "TRUST_PARENT" {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        if folder.starts_with(real(holder)) && longest.is_none_or(|(len, _)| rule.len() > len) {
            longest = Some((rule.len(), trusted));
        }
    }
    longest.is_some_and(|(_, trusted)| trusted)
}

/// `path` resolved where it exists, in lower case on macOS, as Gemini compares folders.
fn real(path: &Path) -> PathBuf {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if cfg!(target_os = "macos") {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder of the test's own, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("steery-gemini-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn put(&self, file: &str, text: &str) {
            let path = self.0.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn listed(servers: &McpServers) -> Vec<(&str, Option<&str>)> {
        servers
            .servers
            .iter()
            .map(|server| (server.server.as_str(), server.url.as_deref()))
            .collect()
    }

    #[test]
    fn merges_settings_trust_extensions_and_lists() {
        let scratch = Scratch::new("mcp");
        let root = scratch.0.join("home/.gemini");
        let project = scratch.0.join("project");
        let elsewhere = scratch.0.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        scratch.put(
            "home/.gemini/settings.json",
            r#"{
                // The person's own servers.
                "mcpServers": {
                    "a": {"httpUrl": "https://a.example/mcp?key=secret"},
                    "b": {"command": "b-server"},
                    "x": {"url": "https://x.example"},
                    "shared": {"command": "u", "timeout": 5000},
                    "off": {"command": "off-server"},
                    "t": {"command": "t-server"}, /* overridden by nothing */
                },
                "mcp": {
                    "excluded": ["x"],
                    "allowed": ["a", "b", "x", "p", "ext1", "shared", "off", "t", "t2"]
                }
            }"#,
        );
        scratch.put(
            "home/.gemini/trustedFolders.json",
            &format!(
                "{{\n  // trusted\n  {:?}: \"TRUST_FOLDER\"\n}}",
                project.display()
            ),
        );
        scratch.put(
            "project/.gemini/settings.json",
            r#"{"mcpServers": {"a": {"url": "https://a2.example"}, "p": {"url": "http://localhost:9/p"}, "q": {"command": "q"}}}"#,
        );
        scratch.put(
            "home/.gemini/extensions/one/gemini-extension.json",
            r#"{"name": "one", "version": "1.0.0", "mcpServers": {"ext1": {"httpUrl": "https://e.example/m"}, "shared": {"url": "https://shared.example"}}}"#,
        );
        scratch.put(
            "home/.gemini/extensions/two/gemini-extension.json",
            r#"{"name": "two", "version": "1.0.0", "mcpServers": {"t2": {"command": "t2"}}}"#,
        );
        scratch.put(
            "home/.gemini/extensions/extension-enablement.json",
            r#"{"two": {"overrides": ["!/*"]}}"#,
        );
        scratch.put(
            "home/.gemini/mcp-server-enablement.json",
            r#"{"off": {"enabled": false}}"#,
        );
        let places = Places {
            system: scratch.0.join("system/settings.json"),
            defaults: scratch.0.join("system/system-defaults.json"),
            trusted: root.join("trustedFolders.json"),
            trust: None,
        };

        let trusted = servers(&root, Some(&project), &places);
        assert_eq!(
            listed(&trusted),
            [
                ("a", Some("https://a2.example")),
                ("b", None),
                ("ext1", Some("https://e.example/m")),
                ("p", Some("http://localhost:9/p")),
                ("shared", Some("https://shared.example")),
                ("t", None),
            ]
        );
        assert!(!trusted.complete);
        assert!(
            trusted
                .watch
                .contains(&project.join(".gemini/settings.json"))
        );
        assert!(
            trusted
                .watch
                .contains(&root.join("extensions/one/gemini-extension.json"))
        );

        let own = servers(&root, None, &places);
        assert_eq!(
            listed(&own),
            [
                ("a", Some("https://a.example/mcp")),
                ("b", None),
                ("ext1", Some("https://e.example/m")),
                ("shared", Some("https://shared.example")),
                ("t", None),
            ]
        );

        assert!(servers(&root, Some(&elsewhere), &places).servers.is_empty());
    }
}
