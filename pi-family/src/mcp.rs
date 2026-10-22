//! `mcp/list`: the MCP servers omp or pi loads, read from the harness's own `mcp.json`
//! files as the harness reads them, each as its name and a remote one's URL without a
//! secret (PROTOCOL.md §4.1).
//!
//! omp reads, first definition of a name winning: a folder's `.omp/mcp.json` and
//! `.omp/.mcp.json`, its agent folder's `mcp.json` and `.mcp.json`, then the folder's own
//! `mcp.json` and `.mcp.json`. It skips an entry `enabled: false` unless the agent folder's
//! `enabledServers` names it, and every one its `disabledServers` names; with
//! `mcp.enableProjectConfig` off it reads no folder's file. Servers omp imports from
//! other tools' files and from plugins are not read here.
//!
//! pi reads its agent folder's `mcp.json`, then a folder's `.pi/mcp.json` where its
//! `trust.json` trusts the folder, a project entry replacing a same-named one. It skips an
//! entry `enabled: false`, one it cannot load, a name another one's spells alike but for
//! `-` and `_`, and a project entry that would send a sign-in's token (`auth`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};
use steery_driver_common::program::helper;
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};
use steery_sdk::{Error, refusal};

use crate::Harness;

/// Steery's own server, as setup names it in omp and the plugin registers it in pi.
const STEERY: &str = "steery";

/// How long omp has to say whether it reads a folder's servers.
const ASKED: Duration = Duration::from_secs(5);

/// The servers `harness`, keeping everything in `root`, loads where `asked` says; `binary`
/// is the harness's executable, which omp is asked whether it reads a folder's servers.
pub async fn list(
    harness: Harness,
    root: &Path,
    binary: Option<PathBuf>,
    asked: McpList,
) -> Result<McpServers, Error> {
    if asked.place.as_deref().is_some_and(|place| !place.is_absolute()) {
        return Err(Error::invalid("`place` is no absolute folder"));
    }
    let place = asked.place.as_deref();
    let servers = match harness {
        Harness::Omp => omp(root, place, binary).await?,
        Harness::Pi => pi(root, place),
    };
    Ok(McpServers {
        servers: servers
            .into_iter()
            .filter(|(name, _)| name.replace('-', "_") != STEERY)
            .map(|(name, entry)| server(name, &entry))
            .collect(),
    })
}

async fn omp(
    root: &Path,
    place: Option<&Path>,
    binary: Option<PathBuf>,
) -> Result<Vec<(String, Value)>, Error> {
    let user = read(&root.join("mcp.json"));
    let named = |key: &str| -> Vec<String> {
        user.as_ref()
            .and_then(|config| config.get(key))
            .and_then(Value::as_array)
            .map(|names| names.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default()
    };
    let (disabled, enabled) = (named("disabledServers"), named("enabledServers"));
    let project = match place {
        Some(place) => {
            let files = [".omp/mcp.json", ".omp/.mcp.json", "mcp.json", ".mcp.json"]
                .map(|file| place.join(file));
            if files.iter().any(|file| file.exists()) && omp_reads_projects(place, binary).await? {
                Some(files)
            } else {
                None
            }
        }
        None => None,
    };
    let mut files = Vec::new();
    if let Some([omp, omp_hidden, _, _]) = &project {
        files.extend([omp.clone(), omp_hidden.clone()]);
    }
    files.extend([root.join("mcp.json"), root.join(".mcp.json")]);
    if let Some([_, _, fallback, fallback_hidden]) = project {
        files.extend([fallback, fallback_hidden]);
    }
    let mut seen = Vec::new();
    let mut servers = Vec::new();
    for (name, entry) in files.iter().flat_map(|file| servers_in(file)) {
        if seen.contains(&name) {
            continue;
        }
        seen.push(name.clone());
        let off = entry.get("enabled") == Some(&Value::Bool(false)) && !enabled.contains(&name);
        if !off && !disabled.contains(&name) {
            servers.push((name, entry));
        }
    }
    Ok(servers)
}

/// Whether omp reads a folder's servers in `place`: its own `mcp.enableProjectConfig`, as
/// `omp config get` says it there, where its settings files and a folder's are merged.
async fn omp_reads_projects(place: &Path, binary: Option<PathBuf>) -> Result<bool, Error> {
    let omp = binary.ok_or_else(|| Error::refused(refusal::NO_BINARY, "omp is not installed here"))?;
    let output = tokio::time::timeout(
        ASKED,
        helper(&omp)
            .args(["config", "get", "mcp.enableProjectConfig", "--json"])
            .current_dir(place)
            .stdout(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| {
        Error::refused(
            refusal::NO_ANSWER,
            "`omp config get mcp.enableProjectConfig` did not answer in time",
        )
    })?
    .map_err(|e| Error::refused(refusal::NO_BINARY, format!("omp would not start: {e}")))?;
    serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .filter(|_| output.status.success())
        .and_then(|said| said.get("value").and_then(Value::as_bool))
        .ok_or_else(|| {
            Error::refused(
                refusal::REFUSED,
                "`omp config get mcp.enableProjectConfig` said no value",
            )
        })
}

fn pi(root: &Path, place: Option<&Path>) -> Vec<(String, Value)> {
    let mut servers: Vec<(String, Value)> = Vec::new();
    let mut add = |name: String, entry: Value, project: bool| {
        let namespace = name.replace('-', "_");
        let valid = !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            && entry.get("type").and_then(Value::as_str) != Some("sse")
            && (entry.get("url").is_some_and(Value::is_string)
                || entry.get("command").is_some_and(Value::is_string));
        let clash = servers
            .iter()
            .any(|(other, _)| *other != name && other.replace('-', "_") == namespace);
        if !valid || clash || (project && entry.get("url").is_some() && entry.get("auth").is_some()) {
            return;
        }
        match servers.iter_mut().find(|(other, _)| *other == name) {
            Some(held) => held.1 = entry,
            None => servers.push((name, entry)),
        }
    };
    for (name, entry) in servers_in(&root.join("mcp.json")) {
        add(name, entry, false);
    }
    if let Some(place) = place.filter(|place| pi_trusts(root, place)) {
        for (name, entry) in servers_in(&place.join(".pi/mcp.json")) {
            add(name, entry, true);
        }
    }
    servers.retain(|(_, entry)| entry.get("enabled") != Some(&Value::Bool(false)));
    servers
}

/// Whether pi trusts `place`: the decision `trust.json` holds for the folder, as its real
/// path, or for the nearest folder above it that has one.
fn pi_trusts(root: &Path, place: &Path) -> bool {
    let Some(Value::Object(decisions)) = read(&root.join("trust.json")) else {
        return false;
    };
    let place = std::fs::canonicalize(place).unwrap_or_else(|_| place.to_owned());
    place
        .ancestors()
        .find_map(|folder| decisions.get(folder.to_str()?)?.as_bool())
        .unwrap_or(false)
}

/// One server as the protocol names it: its URL, where it has one, is a remote server's.
fn server(name: String, entry: &Value) -> McpServer {
    match entry.get("url").and_then(Value::as_str) {
        Some(url) => McpServer::remote(name, url),
        None => McpServer::local(name),
    }
}

/// The `mcpServers` a file names, none where it is missing or not JSON, as the harness
/// skips such a file.
fn servers_in(file: &Path) -> Map<String, Value> {
    match read(file).and_then(|mut config| config.get_mut("mcpServers").map(Value::take)) {
        Some(Value::Object(servers)) => servers,
        _ => Map::new(),
    }
}

fn read(file: &Path) -> Option<Value> {
    let bytes = std::fs::read(file).ok()?;
    serde_json::from_slice(bytes.strip_prefix("\u{feff}".as_bytes()).unwrap_or(&bytes)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn listed(harness: Harness, root: &Path, binary: Option<PathBuf>, place: Option<&Path>) -> Vec<McpServer> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(list(
                harness,
                root,
                binary,
                McpList {
                    place: place.map(Path::to_owned),
                },
            ))
            .unwrap()
            .servers
    }

    fn named(name: &str, url: Option<&str>) -> McpServer {
        McpServer {
            name: name.into(),
            url: url.map(str::to_owned),
        }
    }

    /// A person's own servers and a folder's, as omp and pi load them: a remote server is
    /// its name and its URL without the token in its query, its user or its headers; a
    /// stdio server its name alone; Steery's own entry and a folder's server the harness
    /// would not load are absent, and nothing the files hold beyond that leaves.
    #[test]
    fn each_harness_names_the_servers_it_loads_and_no_secret() {
        let dir = std::env::temp_dir().join(format!("pi-family-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (root, project, untrusted) = (dir.join("agent"), dir.join("project"), dir.join("elsewhere"));
        let user = r#"{
            "mcpServers": {
                "docs": {"type": "http", "url": "https://person:pw@mcp.example.com:8443/mcp?token=SECRET#top",
                         "headers": {"Authorization": "Bearer SECRET"}},
                "files": {"command": "npx", "args": ["-y", "fs", "--key", "SECRET"], "env": {"KEY": "SECRET"}},
                "steery": {"type": "http", "url": "http://127.0.0.1:7777/mcp"},
                "off": {"command": "x", "enabled": false}
            },
            "disabledServers": ["hidden"]
        }"#;
        write(&root.join("mcp.json"), user);
        let them = [
            named("docs", Some("https://mcp.example.com:8443/mcp")),
            named("files", None),
        ];

        // omp: the folder's own file wins a name, `enabled: false` there turns the user's
        // server off, and `disabledServers` hides one from any file.
        let omp_project = r#"{"mcpServers": {
            "proj": {"type": "http", "url": "https://proj.example.com/mcp?k=SECRET"},
            "files": {"command": "other", "enabled": false},
            "hidden": {"command": "y"}
        }}"#;
        write(&project.join(".omp/mcp.json"), omp_project);
        write(&project.join(".mcp.json"), r#"{"mcpServers": {"fallback": {"command": "z"}}}"#);
        let omp = |answer: &str| {
            let fake = dir.join(format!("omp-{answer}"));
            write(
                &fake,
                &format!("#!/bin/sh\necho '{{\"key\": \"mcp.enableProjectConfig\", \"value\": {answer}}}'\n"),
            );
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            Some(fake)
        };
        assert_eq!(listed(Harness::Omp, &root, None, None), them);
        assert_eq!(
            listed(Harness::Omp, &root, omp("true"), Some(&project)),
            [
                named("proj", Some("https://proj.example.com/mcp")),
                them[0].clone(),
                named("fallback", None),
            ]
        );
        assert_eq!(
            listed(Harness::Omp, &root, omp("false"), Some(&project)),
            them,
            "with project config off, no folder's server"
        );

        // pi: a folder's `.pi/mcp.json` only where `trust.json` trusts it or a folder
        // above; its entry replaces the user's, and one sending a sign-in's token is
        // skipped.
        let pi_project = r#"{"mcpServers": {
            "proj": {"url": "https://proj.example.com/mcp?k=SECRET"},
            "files": {"url": "https://files.example.com/v1?key=SECRET"},
            "signed": {"url": "https://signed.example.com/mcp", "auth": {"provider": "x"}},
            "fs-tool": {"command": "a"},
            "fs_tool": {"command": "b"}
        }}"#;
        for folder in [&project, &untrusted] {
            write(&folder.join("inner/.pi/mcp.json"), pi_project);
        }
        write(
            &root.join("trust.json"),
            &format!(
                "{{\"{}\": true, \"{}\": false}}",
                std::fs::canonicalize(&project).unwrap().display(),
                std::fs::canonicalize(&untrusted).unwrap().display()
            ),
        );
        assert_eq!(listed(Harness::Pi, &root, None, None), them);
        assert_eq!(
            listed(Harness::Pi, &root, None, Some(&project.join("inner"))),
            [
                them[0].clone(),
                named("files", Some("https://files.example.com/v1")),
                named("fs-tool", None),
                named("proj", Some("https://proj.example.com/mcp")),
            ]
        );
        assert_eq!(listed(Harness::Pi, &root, None, Some(&untrusted.join("inner"))), them);

        for harness in [Harness::Omp, Harness::Pi] {
            let said = serde_json::to_string(
                &listed(harness, &root, omp("true"), Some(&project.join("inner"))),
            )
            .unwrap();
            assert!(!said.contains("SECRET") && !said.contains("person"), "{said}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
