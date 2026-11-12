//! `mcp/list`: the MCP servers that a conversation of Qwen Code starts, read from the files
//! that Qwen merges, as 0.27.2's `cli/src/config/settings.ts`, `assembleMcpServers` and
//! `core/src/config/mcp-server-config.ts` read them.
//!
//! - Settings, JSON with comments, under `mcpServers` by name: the system defaults, the
//!   person's `<root>/settings.json`, the project's `<cwd>/.qwen/settings.json` where the
//!   folder is trusted, then the system settings. A later file replaces a server of the same
//!   name whole. `mcp.serverCommand` adds a server named `mcp`.
//! - `<cwd>/.mcp.json`, the project's servers: they replace the person's and the system
//!   defaults' servers of a name, and the project's settings and the system settings replace
//!   them.
//! - Each active extension, `<root>/extensions/*/qwen-extension.json` (its `.mcp.json` where
//!   the manifest has no `mcpServers`): its servers join them; settings win a name.
//! - Then `mcp.allowed` (the patterns of every scope's list; a list, even empty, lets only
//!   what it matches) and `mcp.excluded` (which wins), with `*` and `?` as globs.
//! - A server of the project's `.mcp.json` or of its settings waits for the person's approval:
//!   Qwen starts it only where `mcpApprovals.json` approves it for the folder, under the hash
//!   of its fields that matter. Qwen skips the approval in YOLO mode; a mode named only on
//!   the command line is not seen here.
//!
//! The list is never complete: servers named on Qwen's command line, or added in a running
//! conversation, are in no file.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use steery_sdk::Error;
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};

use crate::jsonc::jsonc;

/// The servers a conversation of Qwen, whose root is `root`, starts in `asked.cwd`; asked
/// of no folder, the person's own servers, as no project and no trust limit them.
pub fn list(_home: &Path, root: &Path, asked: McpList) -> Result<McpServers, Error> {
    Ok(servers(root, asked.cwd.as_deref(), &Places::here(root)))
}

/// The files outside Qwen's settings that Qwen reads its settings, trust and approvals from.
struct Places {
    /// The system settings, which override all others.
    system: PathBuf,
    /// The system defaults, which all others override.
    defaults: PathBuf,
    /// `trustedFolders.json`.
    trusted: PathBuf,
    /// `mcpApprovals.json`.
    approvals: PathBuf,
}

impl Places {
    /// As Qwen finds them in this environment (`storage-paths-lite.ts`, `trustedFolders.ts`,
    /// `mcp-approvals.ts`).
    fn here(root: &Path) -> Self {
        let system = var_path("QWEN_CODE_SYSTEM_SETTINGS_PATH").unwrap_or_else(|| {
            PathBuf::from(if cfg!(target_os = "macos") {
                "/Library/Application Support/QwenCode/settings.json"
            } else {
                "/etc/qwen-code/settings.json"
            })
        });
        let defaults = var_path("QWEN_CODE_SYSTEM_DEFAULTS_PATH").unwrap_or_else(|| {
            system
                .parent()
                .unwrap_or(Path::new("/"))
                .join("system-defaults.json")
        });
        Self {
            system,
            defaults,
            trusted: var_path("QWEN_CODE_TRUSTED_FOLDERS_PATH")
                .unwrap_or_else(|| root.join("trustedFolders.json")),
            approvals: var_path("QWEN_CODE_MCP_APPROVALS_PATH")
                .unwrap_or_else(|| root.join("mcpApprovals.json")),
        }
    }
}

/// The path in the environment variable `name`, where it is set and not empty.
fn var_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// A settings file as Qwen reads it; an empty object where it is missing or unreadable.
fn settings(path: &Path) -> Value {
    jsonc(path)
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Map::new()))
}

/// The value at `pointer` in the last of `scopes`, in merge order, that holds one.
fn last<'a>(scopes: &[&'a Value], pointer: &str) -> Option<&'a Value> {
    scopes.iter().rev().find_map(|scope| scope.pointer(pointer))
}

/// Where a server was configured, which says whether it waits for approval.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// The person's settings or the system defaults.
    Person,
    /// The project's `.mcp.json`.
    Project,
    /// The project's `.qwen/settings.json`.
    Workspace,
    /// The system settings.
    System,
    /// An extension.
    Extension,
}

/// The settings Qwen loads in `cwd`, in merge order: the system defaults, the person's,
/// the project's, the system settings.
pub(super) fn scopes(root: &Path, cwd: &Path) -> [Value; 4] {
    loaded(root, Some(cwd), &Places::here(root))
}

/// The settings Qwen loads in `cwd` with its files at `places`. The project's
/// `<cwd>/.qwen/settings.json` counts where folder trust is off or trusts the folder, and
/// not in Qwen's home folder, where it is the person's own.
fn loaded(root: &Path, cwd: Option<&Path>, places: &Places) -> [Value; 4] {
    let user_path = root.join("settings.json");
    let system = settings(&places.system);
    let defaults = settings(&places.defaults);
    let user = settings(&user_path);
    let mut workspace = Value::Object(Map::new());
    if let Some(cwd) = cwd {
        let path = cwd.join(".qwen/settings.json");
        let checks = last(
            &[&defaults, &user, &system],
            "/security/folderTrust/enabled",
        )
        .and_then(Value::as_bool)
        .unwrap_or(false);
        if (!checks || trusts(&places.trusted, cwd)) && real(&path) != real(&user_path) {
            workspace = settings(&path);
        }
    }
    [defaults, user, workspace, system]
}

fn servers(root: &Path, cwd: Option<&Path>, places: &Places) -> McpServers {
    let user_path = root.join("settings.json");
    let extensions = root.join("extensions");
    let mut watch = vec![
        places.system.clone(),
        places.defaults.clone(),
        user_path.clone(),
        places.trusted.clone(),
        places.approvals.clone(),
        extensions.clone(),
        extensions.join("extension-enablement.json"),
    ];
    let mut project = Map::new();
    if let Some(cwd) = cwd {
        watch.push(cwd.join(".qwen/settings.json"));
        let file = cwd.join(".mcp.json");
        watch.push(file.clone());
        if let Some(Value::Object(servers)) =
            jsonc(&file).and_then(|f| f.get("mcpServers").cloned())
        {
            project = servers;
        }
    }
    let scopes = loaded(root, cwd, places);
    let [defaults, user, workspace, system] = &scopes;
    let scopes = [defaults, user, workspace, system];

    let mut configured: BTreeMap<String, (Map<String, Value>, Scope)> = BTreeMap::new();
    let mut put = |entries: Option<&Map<String, Value>>, scope: Scope| {
        for (name, entry) in entries.into_iter().flatten() {
            if let Some(entry) = entry.as_object() {
                configured.insert(name.clone(), (entry.clone(), scope));
            }
        }
    };
    let entries = |scope: &Value| scope.get("mcpServers").and_then(Value::as_object).cloned();
    put(entries(defaults).as_ref(), Scope::Person);
    put(entries(user).as_ref(), Scope::Person);
    let project: Map<String, Value> = project
        .into_iter()
        .filter(|(_, entry)| entry.is_object())
        .map(|(name, entry)| (name, claude(entry)))
        .collect();
    put(Some(&project), Scope::Project);
    put(entries(workspace).as_ref(), Scope::Workspace);
    put(entries(system).as_ref(), Scope::System);
    if let Some(command) = last(&scopes, "/mcp/serverCommand")
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty())
    {
        let entry = Map::from_iter([("command".to_owned(), Value::from(command))]);
        configured.insert("mcp".to_owned(), (entry, Scope::Person));
    }

    let rules = steery_driver_common::mcp::json(&extensions.join("extension-enablement.json"));
    let at = slashed(&cwd.unwrap_or(Path::new("/")).to_string_lossy());
    for extension in active(&extensions, rules.as_ref(), &at, &mut watch) {
        for (name, entry) in extension
            .get("mcpServers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if let Some(entry) = entry.as_object() {
                configured
                    .entry(name.clone())
                    .or_insert_with(|| (entry.clone(), Scope::Extension));
            }
        }
    }

    let allowed: Option<Vec<&str>> = scopes
        .iter()
        .any(|scope| scope.pointer("/mcp/allowed").is_some_and(Value::is_array))
        .then(|| {
            scopes
                .iter()
                .flat_map(|scope| strings(scope, "/mcp/allowed"))
                .collect()
        });
    let excluded: Vec<&str> = scopes
        .iter()
        .flat_map(|scope| strings(scope, "/mcp/excluded"))
        .collect();
    let yolo = last(&scopes, "/tools/approvalMode").and_then(Value::as_str) == Some("yolo");
    let approvals = jsonc(&places.approvals);
    let approved = |name: &str, fields: &Map<String, Value>| {
        let Some(cwd) = cwd else { return true };
        let record = approvals
            .as_ref()
            .and_then(|a| a.get(&*resolved(cwd).to_string_lossy()))
            .and_then(|project| project.get(name));
        record.and_then(|r| r.get("status")).and_then(Value::as_str) == Some("approved")
            && record.and_then(|r| r.get("hash")).and_then(Value::as_str)
                == Some(hash(fields).as_str())
    };
    let servers = configured
        .into_iter()
        .filter(|(_, (fields, _))| {
            ["command", "url", "httpUrl", "tcp"]
                .iter()
                .any(|key| truthy(fields.get(*key)))
        })
        .filter(|(name, _)| {
            allowed
                .as_ref()
                .is_none_or(|allowed| allowed.iter().any(|p| glob(p, name)))
        })
        .filter(|(name, _)| !excluded.iter().any(|p| glob(p, name)))
        .filter(|(name, (fields, scope))| {
            yolo || !matches!(scope, Scope::Project | Scope::Workspace) || approved(name, fields)
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

/// A `.mcp.json` server in Qwen's own fields (`normalizeClaudeMcpServer`): Claude's `type`
/// goes, and a `url` of `type: "http"` is an `httpUrl`.
fn claude(entry: Value) -> Value {
    let Value::Object(mut server) = entry else {
        return entry;
    };
    let sdk = server.get("type").and_then(Value::as_str) == Some("sdk");
    if ["command", "httpUrl", "tcp"]
        .iter()
        .any(|key| truthy(server.get(*key)))
    {
        if !sdk {
            server.remove("type");
        }
    } else if let Some(Value::String(url)) = server.remove("url") {
        let http = server.get("type").and_then(Value::as_str) == Some("http");
        if !sdk {
            server.remove("type");
        }
        server.insert(if http { "httpUrl" } else { "url" }.to_owned(), url.into());
    }
    Value::Object(server)
}

/// `hashMcpServerConfig`: SHA-256, in hex, of the server's fields but `scope`,
/// `extensionName` and `description`, as JavaScript's `JSON.stringify` writes them with each
/// object's keys sorted.
fn hash(fields: &Map<String, Value>) -> String {
    let mut text = String::new();
    let behavioral: Map<String, Value> = fields
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "scope" | "extensionName" | "description"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    stringify(&Value::Object(behavioral), &mut text);
    Sha256::digest(text.as_bytes())
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// `value` as `JSON.stringify` writes it, each object's keys sorted by UTF-16 code unit.
fn stringify(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::from(key.as_str()).to_string());
                out.push(':');
                stringify(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                stringify(item, out);
            }
            out.push(']');
        }
        // JavaScript writes a whole number without its fraction.
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 1e21 => {
                let _ = write!(out, "{}", f as i64);
            }
            _ => out.push_str(&n.to_string()),
        },
        other => out.push_str(&other.to_string()),
    }
}

/// `matchesServerPattern`: `*` is any run of characters, `?` one; else the exact name.
fn glob(pattern: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pattern.chars().collect(), name.chars().collect());
    let (mut pi, mut ni, mut star) = (0, 0, None);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
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

/// The strings of the array at `pointer` in `value`, but empty ones.
fn strings<'a>(value: &'a Value, pointer: &str) -> impl Iterator<Item = &'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| !name.is_empty())
}

/// The manifest of each extension in `dir` that is active in the folder `at` (slashed at
/// both ends), in the order of their folders' names, with the servers of its `.mcp.json`
/// where the manifest names none. Each file read joins `watch`.
fn active(dir: &Path, rules: Option<&Value>, at: &str, watch: &mut Vec<PathBuf>) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut folders: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    folders.sort();
    folders
        .into_iter()
        .filter_map(|folder| {
            let install = folder.join(".qwen-extension-install.json");
            watch.push(install.clone());
            let install = steery_driver_common::mcp::json(&install);
            let home = match (
                install
                    .as_ref()
                    .and_then(|i| i.get("type"))
                    .and_then(Value::as_str),
                install
                    .as_ref()
                    .and_then(|i| i.get("source"))
                    .and_then(Value::as_str),
            ) {
                (Some("link"), Some(source)) => PathBuf::from(source),
                _ => folder,
            };
            let file = home.join("qwen-extension.json");
            watch.push(file.clone());
            let mut config = steery_driver_common::mcp::json(&file)?;
            let name = config
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())?
                .to_owned();
            if !enabled(rules, &name, at) {
                return None;
            }
            if config.get("mcpServers").is_none_or(|s| !truthy(Some(s))) {
                let file = home.join(".mcp.json");
                watch.push(file.clone());
                if let Some(servers) = steery_driver_common::mcp::json(&file)
                    .and_then(|f| f.get("mcpServers").cloned())
                    .filter(Value::is_object)
                {
                    config["mcpServers"] = servers;
                }
            }
            Some(config)
        })
        .collect()
}

/// Whether `extension-enablement.json` leaves the extension `name` on in the folder `at`:
/// the last of its rules that matches decides; on by default.
fn enabled(rules: Option<&Value>, name: &str, at: &str) -> bool {
    let mut on = true;
    for rule in rules
        .and_then(|rules| rules.get(name))
        .and_then(|rules| rules.get("overrides"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let (off, base) = rule
            .strip_prefix('!')
            .map_or((false, rule), |base| (true, base));
        let matches = match base.strip_suffix('*') {
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

/// `path` with forward slashes and one at each end, as Qwen compares folders.
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

/// Whether `trustedFolders.json` at `file` trusts `folder`: the deepest rule whose folder
/// holds it decides, a rule that distrusts winning a tie; a level Qwen does not know is
/// skipped; no rule trusts nothing.
fn trusts(file: &Path, folder: &Path) -> bool {
    let Some(Value::Object(rules)) = jsonc(file) else {
        return false;
    };
    let folder = real(folder);
    let mut deepest: Option<(usize, bool)> = None;
    for (rule, level) in &rules {
        let path = Path::new(rule);
        let (trusted, holder) = match level.as_str() {
            Some("TRUST_FOLDER") => (true, path),
            Some("TRUST_PARENT") => (true, path.parent().unwrap_or(path)),
            Some("DO_NOT_TRUST") => (false, path),
            _ => continue,
        };
        let holder = real(holder);
        if !folder.starts_with(&holder) {
            continue;
        }
        let depth = holder.components().count();
        if deepest.is_none_or(|(d, t)| depth > d || depth == d && !trusted && t) {
            deepest = Some((depth, trusted));
        }
    }
    deepest.is_some_and(|(_, trusted)| trusted)
}

/// `path` made absolute without following links, as Node's `path.resolve` does.
fn resolved(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `path` resolved where it exists, in lower case on macOS, as Qwen compares folders.
fn real(path: &Path) -> PathBuf {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if cfg!(target_os = "macos") {
        PathBuf::from(path.to_string_lossy().to_lowercase())
    } else {
        path
    }
}
