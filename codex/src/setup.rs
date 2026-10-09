//! `setup/plan`: what sets Codex up for Steery, each an entry the runtime writes and
//! records: Steery's MCP server in `config.toml`, Steery's skill, one hook group per
//! moment in `hooks.json` beside the person's own, and the trust a Steery hook its person
//! already trusted lost by moving.
//!
//! Codex runs a hook only while `config.toml` holds, under `[hooks.state."<file>:<event>:
//! <group>:<hook>"]`, a `trusted_hash` equal to the hook's current hash, and the key is a
//! position: another tool's group put ahead of Steery's moves Steery's down one place, not
//! a byte of it changed, and Codex reads it as modified and runs none of it (`dcg`, on
//! `PreToolUse`, 2026-09-24). The person's decision was about the content, so a Steery
//! hook whose current hash a record on the same file and event already trusts has that
//! trust written again where it now stands. Nothing is trusted that was not before, and
//! Codex is asked (`hooks/list`), never imitated.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use steery_driver_common::program::{release_within, version};
use steery_sdk::Error;
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

use crate::appserver::AppServer;
use crate::hooks;

/// How long Codex's app server has to list its hooks.
const LIST_WITHIN: Duration = Duration::from_secs(15);

/// The plan for this machine.
pub async fn plan(home: &Path, setup: Setup) -> Result<Planned, Error> {
    let root = match crate::home::root(home) {
        Ok(root) => root,
        Err(why) => {
            return Ok(Planned {
                found: None,
                refused: Some(why),
                entries: Vec::new(),
                notes: Vec::new(),
                install: None,
            });
        }
    };
    let Some(binary) = crate::home::binary() else {
        return Ok(Planned {
            found: None,
            refused: None,
            entries: Vec::new(),
            notes: vec![
                "Not installed here: no executable found, and a configuration directory is not \
                 evidence of one."
                    .into(),
            ],
            install: crate::home::install(),
        });
    };
    let mut notes = Vec::new();
    let release = release(&binary).await;
    if release.is_none() {
        notes.push("`--version` named no release, so nothing about this build is claimed.".into());
    }
    let hooks_file = root.join("hooks.json");
    let config = root.join("config.toml");
    let mut entries = vec![
        // Codex ends a tool call at 300 seconds unless the server's entry says otherwise,
        // and a wait for a person is longer. A Codex table is a URL and has nowhere to put
        // a transport.
        Entry {
            path: config.clone(),
            format: Format::Toml,
            at: Some("mcp_servers.steery".into()),
            value: json!({"url": setup.mcp_url, "tool_timeout_sec": setup.timeout}),
            carries: vec![Part::Mcp],
        },
        Entry {
            path: root.join("skills/steery/SKILL.md"),
            format: Format::Text,
            at: None,
            value: json!(setup.skill),
            carries: vec![Part::Skill],
        },
    ];
    // Codex reads a `SessionStart` matcher against how the session started, so a group
    // for every start carries none at all rather than an empty one.
    entries.extend(hooks::events().map(|event| Entry {
        path: hooks_file.clone(),
        format: Format::Json,
        at: Some(format!("/hooks/{event}/-")),
        value: json!({"hooks": [{
            "type": "command",
            "command": command(&setup.hook, event),
            "timeout": hooks::timeout(event),
        }]}),
        carries: vec![Part::Hooks],
    }));
    match trust(&binary, &root, &setup.hook).await {
        Ok(carried) => entries.extend(carried.into_iter().map(|(key, hash)| Entry {
            path: config.clone(),
            format: Format::Toml,
            at: Some(format!("hooks.state.{}.trusted_hash", quoted(&key))),
            value: json!(hash),
            carries: vec![Part::Hooks],
        })),
        Err(why) => notes.push(format!(
            "Codex did not list its hooks ({why}), so no trust a moved Steery hook lost is put back."
        )),
    }
    notes.push(
        "Codex runs no hook a person has not trusted: open `/hooks` inside Codex once and trust \
         the Steery entries. Until then its conversations are seen from outside and it hands \
         nothing over."
            .into(),
    );
    Ok(Planned {
        found: Some(Found {
            binary,
            release: release.unwrap_or_default(),
            root,
        }),
        refused: None,
        entries,
        notes,
        install: None,
    })
}

/// The command one hook runs: the runtime's hook line with the event added, which cannot
/// fail as far as Codex can tell. Codex hands the string to a shell, and a missing
/// `steery` (mid-upgrade, uninstalled) is exit 127, which Codex reads as a hook that
/// failed; `|| true` makes it a hook that said nothing.
fn command(hook: &str, event: &str) -> String {
    format!("{hook} {event} || true")
}

/// A key quoted as a TOML basic string.
fn quoted(key: &str) -> String {
    let mut out = String::with_capacity(key.len() + 2);
    out.push('"');
    for c in key.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The release `codex --version` names (`codex-cli 0.162.0`), or none where it did not
/// answer in time.
async fn release(binary: &Path) -> Option<String> {
    let (major, minor, patch) = release_within(&version(binary).await?)?;
    Some(format!("{major}.{minor}.{patch}"))
}

/// One hook as `hooks/list` names it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Listed {
    key: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    current_hash: Option<String>,
    #[serde(default)]
    trust_status: Option<String>,
}

/// The trust records to write again, asked of Codex's own app server in Codex's home.
async fn trust(binary: &Path, root: &Path, hook: &str) -> Result<Vec<(String, String)>, String> {
    let bytes = match std::fs::read(root.join("config.toml")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let trusted = trusted(&bytes)?;
    if trusted.is_empty() {
        return Ok(Vec::new());
    }
    let cwd = root.parent().unwrap_or(root).to_owned();
    let listed = tokio::time::timeout(LIST_WITHIN, async {
        let mut command = steery_driver_common::program::helper(binary);
        command.env("CODEX_HOME", root);
        let mut server = AppServer::open(command, false)
            .await
            .map_err(|e| e.to_string())?;
        let result = server
            .call("hooks/list", json!({"cwds": [cwd]}))
            .await
            .map_err(|e| e.to_string());
        server.close().await;
        result
    })
    .await
    .map_err(|_| "it did not answer in time".to_owned())??;
    let mut hooks = Vec::new();
    for place in result_places(&listed) {
        hooks.extend(
            serde_json::from_value::<Vec<Listed>>(
                place.get("hooks").cloned().unwrap_or(Value::Null),
            )
            .unwrap_or_default(),
        );
    }
    Ok(carried(&hooks, &trusted, hook))
}

fn result_places(listed: &Value) -> impl Iterator<Item = &Value> {
    listed
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// The file and the event a trust key is about: everything before its last two places.
fn scope(key: &str) -> Option<(&str, &str)> {
    let mut parts = key.rsplitn(4, ':');
    let (_hook, _group, event, file) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    Some((file, event))
}

/// Whether a hook's command is one Steery wrote: the runtime's hook line, on any event.
fn steery(command: &str, hook: &str) -> bool {
    command
        .strip_prefix(hook)
        .and_then(|rest| rest.strip_prefix(' '))
        .and_then(|rest| rest.strip_suffix(" || true"))
        .is_some_and(|event| hooks::events().any(|known| known == event))
}

/// For each Steery hook Codex is not running, its key and hash, where a record on the same
/// file and event already trusts that exact hash.
fn carried(
    listed: &[Listed],
    trusted: &BTreeMap<String, String>,
    hook: &str,
) -> Vec<(String, String)> {
    listed
        .iter()
        .filter(|listed| listed.trust_status.as_deref() != Some("trusted"))
        .filter(|listed| listed.command.as_deref().is_some_and(|c| steery(c, hook)))
        .filter_map(|listed| {
            let hash = listed.current_hash.as_deref()?;
            let here = scope(&listed.key)?;
            trusted
                .iter()
                .any(|(key, was)| was == hash && scope(key) == Some(here))
                .then(|| (listed.key.clone(), hash.to_owned()))
        })
        .collect()
}

/// Every `trusted_hash` in a Codex `config.toml`, by key.
fn trusted(bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "config.toml is not UTF-8".to_owned())?;
    let document: toml::Table = toml::from_str(text)
        .map_err(|e: toml::de::Error| format!("config.toml does not parse: {}", e.message()))?;
    let Some(state) = document
        .get("hooks")
        .and_then(|hooks| hooks.get("state"))
        .and_then(toml::Value::as_table)
    else {
        return Ok(BTreeMap::new());
    };
    Ok(state
        .iter()
        .filter_map(|(key, entry)| {
            let hash = entry.get("trusted_hash")?.as_str()?;
            Some((key.clone(), hash.to_owned()))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "/Users/a/.codex/hooks.json";
    const HOOK: &str = "/Users/a/.local/bin/steery hook codex";

    fn hook(key: &str, command: &str, hash: &str, status: &str) -> Listed {
        Listed {
            key: format!("{FILE}:{key}"),
            command: Some(command.to_owned()),
            current_hash: Some(hash.to_owned()),
            trust_status: Some(status.to_owned()),
        }
    }

    /// The layout of 2026-09-24 20:12: `dcg` put in front on `pre_tool_use`, and
    /// Steery's unchanged hook read as modified one place further down.
    #[test]
    fn trust_follows_a_steery_hook_that_only_moved() {
        let steery = format!("{HOOK} PreToolUse || true");
        let listed = [
            hook(
                "pre_tool_use:0:0",
                "/usr/local/bin/dcg",
                "sha256:c27f",
                "modified",
            ),
            hook("pre_tool_use:1:0", &steery, "sha256:96b3", "modified"),
            hook(
                "pre_tool_use:2:0",
                "paseo hooks",
                "sha256:e871",
                "untrusted",
            ),
            hook(
                "stop:0:0",
                &format!("{HOOK} Stop || true"),
                "sha256:aaaa",
                "trusted",
            ),
        ];
        let trusted = BTreeMap::from([
            (format!("{FILE}:pre_tool_use:0:0"), "sha256:96b3".to_owned()),
            (format!("{FILE}:pre_tool_use:2:0"), "sha256:e871".to_owned()),
            (format!("{FILE}:stop:0:0"), "sha256:aaaa".to_owned()),
        ]);
        assert_eq!(
            carried(&listed, &trusted, HOOK),
            [(format!("{FILE}:pre_tool_use:1:0"), "sha256:96b3".to_owned())],
            "only Steery's own hooks, and neither dcg nor paseo"
        );
    }

    /// A hash nobody trusted, or one trusted on another event, is its person's to trust.
    #[test]
    fn nothing_is_trusted_that_was_not_trusted_there_before() {
        let listed = [hook(
            "stop:1:0",
            &format!("{HOOK} Stop || true"),
            "sha256:b",
            "modified",
        )];
        let elsewhere =
            BTreeMap::from([(format!("{FILE}:pre_tool_use:0:0"), "sha256:b".to_owned())]);
        assert!(carried(&listed, &elsewhere, HOOK).is_empty());
        let other = BTreeMap::from([(format!("{FILE}:stop:0:0"), "sha256:c".to_owned())]);
        assert!(carried(&listed, &other, HOOK).is_empty());
    }

    #[test]
    fn trust_records_are_read_by_key_and_keys_are_quoted() {
        let config = format!(
            "model = \"gpt-5\"\n[hooks.state.\"{FILE}:stop:0:0\"]\ntrusted_hash = \"sha256:a\"\n"
        );
        let read = trusted(config.as_bytes()).unwrap();
        assert_eq!(read[&format!("{FILE}:stop:0:0")], "sha256:a");
        assert_eq!(quoted("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert!(trusted(b"not [toml").is_err());
    }

    #[test]
    fn a_release_is_read_off_codex_s_version_line() {
        assert_eq!(release_within("codex-cli 0.162.0\n"), Some((0, 162, 0)));
        assert_eq!(release_within("codex-cli dev"), None);
        assert!(steery(&format!("{HOOK} SessionStart || true"), HOOK));
        assert!(!steery(&format!("{HOOK} Other || true"), HOOK));
        assert!(!steery(
            "/usr/local/bin/notify hook codex Stop || true",
            HOOK
        ));
    }
}
