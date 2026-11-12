//! `setup/plan`: what sets Crush up for Steery, each an entry the runtime writes and records:
//! Steery's MCP server in the person's `crush.json` (`mcp.steery`), Steery's skill in the
//! skills folder that Crush reads first, and Steery's hooks, appended to `hooks.<event>` in
//! the same `crush.json`. Crush joins the hook lists of all its configuration files, so a
//! project's own hooks run beside Steery's and cannot drop them.

use std::path::Path;

use serde_json::{Value, json};
use steery_driver_common::program::{release_within, version};
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

use crate::{home, hooks};

/// The plan for this host.
pub async fn plan(home: &Path, setup: Setup) -> Planned {
    let Some(binary) = home::binary() else {
        let (install, refused) = match home::install() {
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
    let release = release(&binary).await;
    let mut notes = Vec::new();
    if release.is_none() {
        notes.push(
            "`--version` named no release, so the driver says nothing about this build.".into(),
        );
    }
    notes.extend(home::others_note(&binary));
    let mut entries = vec![
        // One MCP client serves every session, and Crush sends no `_meta` with a call, so
        // the entry sends no `Steery-*` header. `timeout` bounds only Crush's connect and
        // ping, never a call, so the entry leaves Crush's own.
        Entry {
            path: home::settings(home),
            format: Format::Json,
            at: Some("/mcp/steery".into()),
            value: json!({"type": "http", "url": setup.mcp_url}),
            carries: vec![Part::Mcp],
        },
        // Crush's own skills folder. Crush sorts skills by path and keeps the last of a
        // name, and `~/.config/crush` sorts after `~/.agents`, `~/.claude` and
        // `~/.config/agents`, so a `steery` skill there does not replace this one.
        Entry {
            path: home::skills(home).join("steery/SKILL.md"),
            format: Format::Text,
            at: None,
            value: json!(setup.skill),
            carries: vec![Part::Skill],
        },
    ];
    // No `matcher`: every tool. Crush shows `name` in its window beside the hook's verdict.
    entries.extend(hooks::EVENTS.iter().map(|event| Entry {
        path: home::settings(home),
        format: Format::Json,
        at: Some(format!("/hooks/{}/-", event.name)),
        value: json!({
            "name": "Steery",
            "command": hooks::command(&setup.hook, event.name),
            "timeout": event.waits,
        }),
        carries: vec![Part::Hooks],
    }));
    Planned {
        found: Some(Found {
            binary,
            release: release.unwrap_or_default(),
            root: home::config(home),
        }),
        refused: None,
        entries,
        notes,
        install: None,
        needs: Vec::new(),
    }
}

/// The release `crush --version` names (`crush version v0.100.0`), or none where it did not
/// answer in time.
async fn release(binary: &Path) -> Option<String> {
    let (major, minor, patch) = release_within(&version(binary).await?)?;
    Some(format!("{major}.{minor}.{patch}"))
}

/// Whether Steery's `PreToolUse` hook is in the person's `crush.json`, on every tool: an
/// entry with no `matcher` that runs the command setup writes ([`hooks::guards`]). `crush
/// run` approves every call of its session itself, so this hook is the only gate of a turn
/// Steery starts.
pub(crate) fn gated(home: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(home::settings(home)) else {
        return false;
    };
    let Ok(document) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    document["hooks"][hooks::BEFORE_CALL]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|hook| {
            hook.get("matcher")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
        })
        .any(|hook| hook["command"].as_str().is_some_and(hooks::guards))
}
