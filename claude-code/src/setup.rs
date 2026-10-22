//! `setup/plan`: what sets Claude Code up for Steery. Steery's MCP server among Claude
//! Code's user-scope servers; Steery's skill in its skills folder; and one hook per event
//! Steery reads ([`crate::hooks::EVENTS`]) in its `settings.json`, each added beside the
//! person's own hooks on that event.

use std::path::Path;

use serde_json::json;
use steery_driver_common::program::{release_within, version};
use steery_sdk::Error;
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

/// The plan for this machine.
pub async fn plan(home: &Path, root: &Path, setup: Setup) -> Result<Planned, Error> {
    let Some(binary) = crate::binary() else {
        return Ok(Planned {
            found: None,
            refused: None,
            entries: Vec::new(),
            notes: vec![
                "Not installed here: no executable found, and a configuration directory is not \
                 evidence of one."
                    .into(),
            ],
            install: crate::install(),
        });
    };
    let release = named(version(&binary).await.as_deref().and_then(release_within));
    let mut notes = Vec::new();
    if release.is_none() {
        notes.push("`--version` did not answer, so nothing about this build is claimed.".into());
    }
    Ok(Planned {
        found: Some(Found {
            binary,
            release: release.unwrap_or_default(),
            root: root.to_owned(),
        }),
        refused: None,
        entries: entries(home, root, &setup),
        notes,
        install: None,
    })
}

/// Every entry, in the order Claude Code reads them.
fn entries(home: &Path, root: &Path, setup: &Setup) -> Vec<Entry> {
    let servers = crate::claude_json(home, root);
    let mut entries = vec![
        // A per-server `timeout`, in milliseconds, is at once the bound on one call and the
        // floor of Claude Code's idle watchdog on an `http` server. Claude Code reads no
        // per-conversation headers from this file.
        Entry {
            path: servers,
            format: Format::Json,
            at: Some(format!("/mcpServers/{}", crate::STEERY)),
            value: json!({
                "type": "http",
                "url": setup.mcp_url,
                "timeout": u64::from(setup.timeout) * 1_000,
            }),
            carries: vec![Part::Mcp],
        },
        Entry {
            path: root.join("skills").join(crate::STEERY).join("SKILL.md"),
            format: Format::Text,
            at: None,
            value: json!(setup.skill),
            carries: vec![Part::Skill],
        },
    ];
    // One group per event, matching every tool, beside whatever the person hooked there.
    // Claude Code hands the command to a shell, and reads a non-zero exit as a hook that
    // failed — some events by refusing the call about to run — so a hook whose command
    // cannot run (a machine mid-upgrade, a filled disk) says nothing instead.
    let settings = root.join("settings.json");
    entries.extend(crate::hooks::EVENTS.iter().map(|event| Entry {
        path: settings.clone(),
        format: Format::Json,
        at: Some(format!("/hooks/{}/-", event.name)),
        value: json!({
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": format!("{} {} || true", setup.hook, event.name),
                "timeout": event.waits,
            }],
        }),
        carries: vec![Part::Hooks],
    }));
    entries
}

/// A release as `major.minor.patch`.
fn named(release: Option<(u64, u64, u64)>) -> Option<String> {
    release.map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Setup {
        Setup {
            mcp_url: "http://127.0.0.1:7777/mcp".into(),
            skill: "---\nname: steery\n---\n".into(),
            hook: "/usr/local/bin/steery hook claude-code".into(),
            door: "ws://127.0.0.1:7777/door".into(),
            release: "1.0.0".into(),
            timeout: 120,
        }
    }

    #[test]
    fn a_version_is_the_first_release_the_probe_names() {
        assert_eq!(
            named(release_within("2.1.0 (Claude Code)")).as_deref(),
            Some("2.1.0")
        );
        assert_eq!(release_within("Claude Code"), None);
    }

    #[test]
    fn claude_code_is_pointed_at_the_local_server_in_its_own_spelling() {
        let home = Path::new("/home/a");
        let entries = entries(home, &home.join(".claude"), &setup());
        let mcp = &entries[0];
        assert_eq!(mcp.path, home.join(".claude.json"));
        assert_eq!(mcp.at.as_deref(), Some("/mcpServers/steery"));
        assert_eq!(
            mcp.value,
            json!({"type": "http", "url": "http://127.0.0.1:7777/mcp", "timeout": 120_000})
        );
        let moved = super::entries(home, Path::new("/srv/claude"), &setup());
        assert_eq!(moved[0].path, Path::new("/srv/claude/.claude.json"));
        assert_eq!(
            moved[1].path,
            Path::new("/srv/claude/skills/steery/SKILL.md")
        );
    }

    /// Every event Steery reads is hooked once, running the command the runtime named
    /// with the event added, within the bound the runtime is told it has, and unable to
    /// fail as far as Claude Code can tell.
    #[test]
    fn what_setup_writes_is_what_the_command_reads() {
        let root = Path::new("/home/a/.claude");
        let entries = entries(Path::new("/home/a"), root, &setup());
        let hooked: Vec<&Entry> = entries
            .iter()
            .filter(|entry| entry.carries == [Part::Hooks])
            .collect();
        let table = crate::hooks::table();
        assert_eq!(hooked.len(), table.len());
        for (entry, hook) in hooked.iter().zip(&table) {
            assert_eq!(entry.path, root.join("settings.json"));
            assert_eq!(entry.at, Some(format!("/hooks/{}/-", hook.event)));
            assert_eq!(entry.value["matcher"], "");
            let command = &entry.value["hooks"][0];
            assert_eq!(
                command["command"],
                format!(
                    "/usr/local/bin/steery hook claude-code {} || true",
                    hook.event
                )
            );
            assert_eq!(command["timeout"], json!(hook.waits));
        }
    }
}
