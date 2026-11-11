//! `setup/plan`: what sets Antigravity CLI up for Steery, each an entry the runtime writes
//! and records, all in `~/.gemini/config/`, which Antigravity's app and IDE read too:
//! Steery's MCP server under `/mcpServers/steery` of `mcp_config.json`; Steery's hooks as
//! one named group, `/steery` of `hooks.json`, one hook per event Steery reads
//! ([`super::hooks::EVENTS`]); Steery's skill in `skills/steery/SKILL.md`. And the folders
//! the runtime names, in `trustedWorkspaces` of `<root>/settings.json`.
//!
//! Antigravity runs the hooks and MCP servers of these files in any folder, trusted or not
//! (seen in runs with `-p` in folders never trusted); its terminal asks once whether to
//! trust a folder before it opens a conversation there.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use steery_driver_common::program::{lacks, release_within, version};
use steery_sdk::wire::harness::{Entry, Format, Found, Install, Part, Planned, Setup};

/// The plan for this machine, with the person's home `home` and Antigravity's root `root`.
/// Antigravity has no offline check of its sign-in, so the plan asks for none.
pub async fn plan(home: &Path, root: &Path, setup: Setup) -> Planned {
    let Some(binary) = super::binary(home) else {
        let (install, refused) = match install() {
            Ok(install) => (Some(install), None),
            Err(why) => (None, Some(why)),
        };
        return Planned {
            found: None,
            refused,
            entries: Vec::new(),
            notes: vec![
                "Not installed here. The driver found no executable. A configuration directory \
                 does not prove that an executable is installed."
                    .into(),
            ],
            install,
            needs: Vec::new(),
        };
    };
    found(binary, home, root, setup).await
}

/// The plan where `binary` is the `agy` Steery runs.
async fn found(binary: PathBuf, home: &Path, root: &Path, setup: Setup) -> Planned {
    let release = version(&binary).await.as_deref().and_then(release_within);
    let mut notes = Vec::new();
    let mut refused = None;
    let mut entries = Vec::new();
    let least = release_within(super::MINIMUM).unwrap_or_default();
    match release {
        None => notes.push(
            "`--version` named no release, so the driver says nothing about this build.".into(),
        ),
        Some((major, minor, patch)) if (major, minor, patch) < least => {
            refused = Some(format!(
                "Antigravity CLI {major}.{minor}.{patch} is older than {}, the oldest that \
                 Steery reads. Run `agy update`.",
                super::MINIMUM,
            ));
        }
        Some(_) => {}
    }
    if refused.is_none() {
        entries = self::entries(&super::config(home), root, &setup);
        notes.push(
            "Antigravity's app and IDE read the same MCP servers, hooks and skills as its CLI."
                .into(),
        );
    }
    Planned {
        found: Some(Found {
            binary,
            release: release.map_or_else(String::new, |(major, minor, patch)| {
                format!("{major}.{minor}.{patch}")
            }),
            root: root.to_path_buf(),
        }),
        refused,
        entries,
        notes,
        install: None,
        needs: Vec::new(),
    }
}

fn entries(config: &Path, root: &Path, setup: &Setup) -> Vec<Entry> {
    // `serverUrl` is the streamable HTTP transport, the only remote one Antigravity speaks.
    // Its MCP client serves every conversation, so the header names the harness alone.
    // `timeoutSeconds` bounds the connection and each call.
    let mut entries = vec![
        Entry {
            path: config.join("mcp_config.json"),
            format: Format::Json,
            at: Some("/mcpServers/steery".into()),
            value: json!({
                "serverUrl": setup.mcp_url,
                "headers": {"Steery-Harness": "antigravity"},
                "timeoutSeconds": setup.timeout,
            }),
            carries: vec![Part::Mcp],
        },
        Entry {
            path: config.join("skills/steery/SKILL.md"),
            format: Format::Text,
            at: None,
            value: json!(setup.skill),
            carries: vec![Part::Skill],
        },
    ];
    // One group of Steery's own beside the person's groups, so the person can switch it off
    // with its `enabled`. A tool's event matches every tool (`*`). Antigravity hands the
    // command to a shell, so a hook whose command cannot run (a machine mid-upgrade, a filled
    // disk) exits 0 and says nothing, as an answer of nothing does. Its `timeout` is in
    // seconds.
    let group: Map<String, Value> = super::hooks::EVENTS
        .iter()
        .map(|event| {
            let hook = json!({
                "type": "command",
                "command": format!("{} {} || true", setup.hook, event.name),
                "timeout": event.waits,
            });
            let handlers = if event.matched {
                json!([{"matcher": "*", "hooks": [hook]}])
            } else {
                json!([hook])
            };
            (event.name.to_owned(), handlers)
        })
        .collect();
    entries.push(Entry {
        path: config.join("hooks.json"),
        format: Format::Json,
        at: Some("/steery".into()),
        value: Value::Object(group),
        carries: vec![Part::Hooks],
    });
    // Antigravity's terminal asks whether to trust a folder before it opens a conversation
    // there; on a host made for its conversations nobody answers, so the folders the runtime
    // names are trusted as their person would, by the real path it writes.
    let settings = root.join("settings.json");
    entries.extend(setup.trusts.iter().map(|folder| Entry {
        path: settings.clone(),
        format: Format::Json,
        at: Some("/trustedWorkspaces/-".into()),
        value: json!(folder.canonicalize().unwrap_or_else(|_| folder.clone())),
        carries: Vec::new(),
    }));
    entries
}

/// Antigravity's own installer, where `bash` and `curl` are here: it puts `agy` in
/// `~/.local/bin` and adds that folder to the shell's profile. Where not, why, in one line.
fn install() -> Result<Install, String> {
    if let Some(why) = lacks(&["bash", "curl"]) {
        return Err(format!("Antigravity's installer {why}"));
    }
    let line = "curl -fsSL https://antigravity.google/cli/install.sh | bash";
    Ok(Install {
        argv: vec!["bash".into(), "-c".into(), line.into()],
        about: format!(
            "Antigravity CLI's own installer. It installs agy into ~/.local/bin: {line}"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Antigravity CLI 1.5.5 as its installer put it, on the machine that recorded the corpus.
    fn agy() -> Option<PathBuf> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        steery_driver_common::program::executable(home.join(".local/bin/agy"))
    }

    #[tokio::test]
    async fn antigravity_is_pointed_at_steery_in_its_own_spelling() {
        let Some(agy) = agy() else {
            eprintln!("skipped: no ~/.local/bin/agy here");
            return;
        };
        let home = std::env::temp_dir().join("steery-antigravity-plan");
        let root = super::super::root(&home);
        let config = super::super::config(&home);
        let setup = Setup {
            mcp_url: "http://127.0.0.1:7777/mcp".into(),
            skill: "# Steery".into(),
            hook: "/opt/steery/hook".into(),
            door: "http://127.0.0.1:7777/door".into(),
            release: "1.0.0".into(),
            timeout: 600,
            trusts: vec!["/home/daytona/work".into()],
        };
        let planned = found(agy, &home, &root, setup).await;
        assert_eq!(planned.found.unwrap().release, "1.5.5");
        assert_eq!((planned.refused, planned.install), (None, None));
        let mut entries = planned.entries.into_iter();
        let mcp = entries.next().unwrap();
        assert_eq!(
            (mcp.path, mcp.at.as_deref()),
            (config.join("mcp_config.json"), Some("/mcpServers/steery"))
        );
        assert_eq!(
            mcp.value,
            json!({"serverUrl": "http://127.0.0.1:7777/mcp",
                   "headers": {"Steery-Harness": "antigravity"}, "timeoutSeconds": 600})
        );
        let skill = entries.next().unwrap();
        assert_eq!(skill.path, config.join("skills/steery/SKILL.md"));
        // What setup writes is what the hooks read: one hook per declared event.
        let hooks = entries.next().unwrap();
        assert_eq!(
            (hooks.path, hooks.at.as_deref()),
            (config.join("hooks.json"), Some("/steery"))
        );
        for hook in super::super::hooks::table() {
            let command = json!({"type": "command",
                "command": format!("/opt/steery/hook {} || true", hook.event),
                "timeout": hook.waits.unwrap()});
            let handlers = &hooks.value[&hook.event];
            assert!(
                handlers == &json!([command])
                    || handlers == &json!([{"matcher": "*", "hooks": [command]}]),
                "{handlers}"
            );
        }
        assert_eq!(
            hooks.value.as_object().unwrap().len(),
            super::super::hooks::EVENTS.len()
        );
        let trust = entries.next().unwrap();
        assert_eq!(
            (trust.path, trust.at.as_deref(), trust.value),
            (
                root.join("settings.json"),
                Some("/trustedWorkspaces/-"),
                json!("/home/daytona/work")
            )
        );
        assert!(entries.next().is_none());
    }
}
