//! `mcp/list`: the MCP servers Codex loads, as `codex mcp list --json` names them run in
//! the folder (PROTOCOL.md §4.1). Codex merges its layers itself there: `config.toml`,
//! `/etc/codex`, managed config, plugins, and a project's `.codex/config.toml` only where
//! the project is trusted. Its listing carries each server's arguments, environment and
//! headers verbatim; of those only a name, and a remote server's reduced URL, leave.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use steery_sdk::wire::harness::{McpList, McpServer, McpServers};
use steery_sdk::{Error, refusal};

/// Steery's own server, as setup names it in `config.toml`.
const STEERY: &str = "steery";

/// How long `codex mcp list` has to answer.
const LISTED_WITHIN: Duration = Duration::from_secs(5);

/// The servers the Codex at `codex` loads where `asked` says: a person's own where no
/// folder is named, read from `/`, which no project holds.
pub async fn list(codex: Option<&Path>, asked: McpList) -> Result<McpServers, Error> {
    if asked.place.as_deref().is_some_and(|place| !place.is_absolute()) {
        return Err(Error::invalid("`place` is no absolute folder"));
    }
    let codex =
        codex.ok_or_else(|| Error::refused(refusal::NO_BINARY, "codex is not installed here"))?;
    let place = asked.place.as_deref().unwrap_or(Path::new("/"));
    let servers = listed(codex, place, &[])
        .await?
        .into_iter()
        .filter(|server| {
            server.get("enabled") == Some(&Value::Bool(true))
                && server.get("disabled_reason").is_none_or(Value::is_null)
        })
        .filter_map(|server| {
            let name = server.get("name")?.as_str()?;
            let transport = server.get("transport")?;
            (name != STEERY).then(|| match transport.get("type").and_then(Value::as_str) {
                Some("streamable_http") => McpServer::remote(
                    name,
                    transport.get("url").and_then(Value::as_str).unwrap_or_default(),
                ),
                _ => McpServer::local(name),
            })
        })
        .collect();
    Ok(McpServers { servers })
}

/// Each server `codex mcp list --json` names in `cwd` with the features `off` taken away,
/// as it prints them.
pub async fn listed(codex: &Path, cwd: &Path, off: &[&str]) -> Result<Vec<Value>, Error> {
    let output = tokio::time::timeout(
        LISTED_WITHIN,
        steery_driver_common::program::helper(codex)
            .args(off.iter().flat_map(|feature| ["--disable", *feature]))
            .args(["mcp", "list", "--json"])
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| {
        Error::refused(
            refusal::NO_ANSWER,
            "`codex mcp list` did not answer in time",
        )
    })?
    .map_err(|e| Error::refused(refusal::NO_BINARY, format!("codex would not start: {e}")))?;
    match serde_json::from_slice(&output.stdout) {
        Ok(Value::Array(servers)) if output.status.success() => Ok(servers),
        _ => Err(Error::refused(
            refusal::REFUSED,
            "`codex mcp list` did not list its servers",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What Codex 0.163.0 printed for `codex mcp list --json` given `fixtures/mcp-home.toml`
    /// as `config.toml`, in a trusted project whose `.codex/config.toml` names `proj` and in
    /// an untrusted one whose names `unapproved`: Codex leaves the untrusted project's
    /// servers out itself. A fake `codex` prints the one for the folder it runs in.
    #[tokio::test]
    async fn codex_names_the_servers_it_loads_and_no_secret() {
        let dir = std::env::temp_dir().join(format!("codex-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (trusted, untrusted) = (dir.join("trusted"), dir.join("untrusted"));
        for folder in [&trusted, &untrusted] {
            std::fs::create_dir_all(folder).unwrap();
        }
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let codex = dir.join("codex");
        std::fs::write(
            &codex,
            format!(
                "#!/bin/sh\n[ \"$*\" = 'mcp list --json' ] || exit 2\ncase \"$PWD\" in\n*/trusted) cat '{}' ;;\n*) cat '{}' ;;\nesac\n",
                fixtures.join("mcp-list-trusted.json").display(),
                fixtures.join("mcp-list-untrusted.json").display(),
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();

        let ask = |place: Option<&Path>| {
            list(
                Some(&codex),
                McpList {
                    place: place.map(Path::to_owned),
                },
            )
        };
        let named = |name: &str, url: Option<&str>| McpServer {
            name: name.into(),
            url: url.map(str::to_owned),
        };
        let user = [
            named("docs", Some("https://mcp.example.com:8443/mcp")),
            named("files", None),
        ];
        assert_eq!(ask(None).await.unwrap().servers, user);
        let listed = ask(Some(&trusted)).await.unwrap().servers;
        assert_eq!(
            listed,
            [
                user[0].clone(),
                user[1].clone(),
                named("proj", Some("https://proj.example.com/mcp")),
            ]
        );
        assert_eq!(ask(Some(&untrusted)).await.unwrap().servers, user);
        let said = serde_json::to_string(&listed).unwrap();
        assert!(!said.contains("SECRET") && !said.contains("person"), "{said}");

        let refused = |result: Result<McpServers, Error>| match result {
            Err(Error::Refused { word, .. }) => word,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            refused(list(None, McpList { place: None }).await),
            refusal::NO_BINARY
        );
        assert!(ask(Some(Path::new("relative"))).await.is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
