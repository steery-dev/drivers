//! Kilo's configuration: JSON with comments, merged from its global folder's `config.json`,
//! `opencode.json(c)` and `kilo.json(c)`, and a project's own files of those names, in the
//! project's folders and their `.kilo` and `.opencode` folders. Its MCP servers are under
//! `mcp`, by name: `{type: "remote", url}` or `{type: "local", command}`, off with
//! `enabled: false`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use steery_driver_common::mcp::Servers;
use steery_driver_common::program;
use steery_sdk::wire::harness::{McpList, McpServers};
use steery_sdk::{Error, refusal};

/// The names of a configuration file, in the order Kilo merges them: a later one wins.
const FILES: [&str; 5] = [
    "config.json",
    "opencode.json",
    "opencode.jsonc",
    "kilo.json",
    "kilo.jsonc",
];

/// How long Kilo has to print the configuration it resolved.
const RESOLVE: Duration = Duration::from_secs(20);

/// The configuration files that can name what a conversation in `cwd` loads: the global
/// ones, then a project's, from `cwd` up.
pub fn files(global: &Path, cwd: Option<&Path>) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = FILES.iter().map(|file| global.join(file)).collect();
    for dir in cwd.into_iter().flat_map(Path::ancestors) {
        for folder in [dir.to_path_buf(), dir.join(".kilo"), dir.join(".opencode")] {
            files.extend(FILES.iter().skip(1).map(|file| folder.join(file)));
        }
    }
    files
}

/// The names of the MCP servers the configuration files that reach `cwd` name, on or off:
/// where a tool's name ends its server.
pub fn servers(global: &Path, cwd: Option<&Path>) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for config in files(global, cwd)
        .iter()
        .filter_map(|file| crate::jsonc::jsonc(file))
    {
        for name in config
            .get("mcp")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|servers| servers.keys())
        {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    names
}

/// `mcp/list`: the servers Kilo itself resolves for a conversation in `asked.cwd`, or in the
/// person's home, from `kilo debug config` run there. Plugins can still add servers as a
/// conversation starts, so the list is never complete.
pub async fn list(
    binary: Option<&Path>,
    home: &Path,
    global: &Path,
    asked: McpList,
) -> Result<McpServers, Error> {
    let binary =
        binary.ok_or_else(|| Error::refused(refusal::NO_BINARY, "Kilo is not installed here"))?;
    let cwd = asked.cwd.as_deref().unwrap_or(home);
    let output = tokio::time::timeout(
        RESOLVE,
        program::helper(binary)
            .args(["--log-level", "ERROR", "debug", "config"])
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| {
        Error::refused(
            refusal::REFUSED,
            format!(
                "`kilo debug config` did not answer within {} s",
                RESOLVE.as_secs()
            ),
        )
    })?
    .map_err(|error| {
        Error::refused(
            refusal::REFUSED,
            format!("`kilo debug config` did not start: {error}"),
        )
    })?;
    let said = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or_default()
            .to_owned()
    };
    if !output.status.success() {
        return Err(Error::refused(refusal::REFUSED, said(&output.stderr)));
    }
    let config: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        Error::refused(
            refusal::REFUSED,
            format!(
                "`kilo debug config` printed no JSON: {}",
                said(&output.stdout)
            ),
        )
    })?;
    let mut servers = Servers::default();
    for (name, entry) in config
        .get("mcp")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        // Kilo reads an entry without a `type` as one that overrides only `enabled`.
        if entry.get("type").is_none() {
            continue;
        }
        servers.add(
            name,
            entry,
            entry.get("enabled").and_then(Value::as_bool) != Some(false),
        );
    }
    Ok(McpServers {
        servers: servers.on(),
        complete: false,
        watch: files(global, asked.cwd.as_deref()),
    })
}
