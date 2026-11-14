//! `setup/plan`: what sets OpenHands up for Steery, each an entry the runtime writes and
//! records: Steery's MCP server in the person's `mcp.json`, Steery's skill among the
//! person's skills, and Steery's hook command line, in a file of its own that the turns this
//! driver starts read. A turn is OpenHands' ACP client, which OpenHands asks before each call
//! it runs.

use std::path::PathBuf;

use serde_json::json;
use steery_driver_common::program::{others_note, release_within, version};
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

use super::events::STEERY;
use super::place::{self, Place};

/// Steery's hook command line, which a turn runs at a permission OpenHands asks for.
/// OpenHands reads nothing under its folder's `steery`.
pub(crate) fn hook_file(place: &Place) -> PathBuf {
    place.root.join(STEERY).join("hook")
}

/// The plan for this machine. Whether the release found is one Steery speaks to is the
/// runtime's to say, from `initialize`'s minimum.
pub(crate) async fn plan(place: &Place, setup: Setup) -> Planned {
    let Some(binary) = place.binary() else {
        let (install, refused) = match place::install() {
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
    let release = version(&binary).await.as_deref().and_then(release_within);
    let mut notes: Vec<String> = others_note(
        "OpenHands",
        place::EXECUTABLE,
        place.installed().as_deref(),
        &binary,
    )
    .into_iter()
    .collect();
    if release.is_none() {
        notes.push(
            "`--version` did not answer, so the driver says nothing about this build.".into(),
        );
    }
    let entries = vec![
        Entry {
            path: place.mcp(),
            format: Format::Json,
            at: Some(format!("/mcpServers/{STEERY}")),
            // OpenHands waits 300 s for any MCP call, whatever its entry says
            // (`sdk/mcp/tool.py`, `MCP_TOOL_TIMEOUT_SECONDS`).
            value: json!({
                "url": setup.mcp_url,
                "transport": "http",
                "headers": {"Steery-Harness": "openhands"},
                "enabled": true,
            }),
            carries: vec![Part::Mcp],
        },
        Entry {
            path: hook_file(place),
            format: Format::Text,
            at: None,
            value: json!(setup.hook),
            carries: vec![Part::Hooks],
        },
        Entry {
            // OpenHands reads its person's skills from the home's folder, whatever
            // `OPENHANDS_PERSISTENCE_DIR` says (`sdk/skills/skill.py`, `USER_SKILLS_DIRS`).
            path: place
                .home
                .join(".openhands/skills")
                .join(STEERY)
                .join("SKILL.md"),
            format: Format::Text,
            at: None,
            value: json!(setup.skill),
            carries: vec![Part::Skill],
        },
    ];
    Planned {
        found: Some(Found {
            binary,
            release: release
                .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
                .unwrap_or_default(),
            root: place.root.clone(),
        }),
        refused: None,
        entries,
        notes,
        install: None,
        needs: Vec::new(),
    }
}
