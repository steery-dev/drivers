//! `setup/plan`: what sets Crush up for Steery, each an entry the runtime writes and records:
//! Steery's MCP server in the person's `crush.json` (`mcp.steery`), and Steery's skill in
//! the skills folder that Crush reads first.

use std::path::Path;

use serde_json::json;
use steery_driver_common::program::{release_within, version};
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

use crate::home;

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
    let entries = vec![
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
