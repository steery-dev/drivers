//! `setup/plan`: what sets Amp up for Steery, each an entry the runtime writes and records:
//! Steery's MCP server in Amp's settings (`amp.mcpServers`), Steery's skill in Amp's own
//! skills folder, and Steery's plugin in Amp's system plugins folder, which every `amp`
//! process loads and which dials the runtime's door. Where Amp is not signed in, its sign-in
//! is a need.

use std::path::Path;

use serde_json::json;
use steery_driver_common::program::{release_within, version};
use steery_sdk::Error;
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

use crate::home;

/// Steery's plugin for Amp, the door's address in a placeholder inside a string literal.
const PLUGIN: &str = include_str!("../plugin/steery.ts");

/// The plan for this host; where `needs` (protocol 5) and Amp is signed out, its sign-in
/// ([`crate::sign_in::need`]).
pub async fn plan(home: &Path, setup: Setup, needs: bool) -> Result<Planned, Error> {
    let Some(binary) = home::binary() else {
        let (install, refused) = match home::install() {
            Ok(install) => (Some(install), None),
            Err(why) => (None, Some(why)),
        };
        return Ok(Planned {
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
        });
    };
    let lacks = async { needs && crate::sign_in::signed_out(&binary).await };
    let (release, lacks) = tokio::join!(release(&binary), lacks);
    let mut notes = Vec::new();
    if release.is_none() {
        notes.push(
            "`--version` named no release, so the driver says nothing about this build.".into(),
        );
    }
    notes.extend(home::others_note(&binary));
    let entries = vec![
        // Amp's one MCP client serves every thread, and Amp names no thread in a call's
        // `_meta`, so the entry sends no `Steery-*` header: a `Steery-Harness` alone,
        // without `call_meta`, is what §3.3 forbids.
        Entry {
            path: home::settings(home),
            format: Format::Json,
            at: Some("/amp.mcpServers/steery".into()),
            value: json!({"url": setup.mcp_url}),
            carries: vec![Part::Mcp],
        },
        // Amp's own skills folder: no other harness reads it.
        Entry {
            path: home::config(home).join("skills/steery/SKILL.md"),
            format: Format::Text,
            at: None,
            value: json!(setup.skill),
            carries: vec![Part::Skill],
        },
        // Amp's system plugins folder: every `amp` process, TUI or `-x`, loads it.
        Entry {
            path: home::config(home).join("plugins/steery.ts"),
            format: Format::Text,
            at: None,
            value: json!(plugin(&setup.door)?),
            carries: vec![Part::Plugin],
        },
    ];
    Ok(Planned {
        found: Some(Found {
            binary,
            release: release.unwrap_or_default(),
            root: home::config(home),
        }),
        refused: None,
        entries,
        notes,
        install: None,
        needs: if lacks {
            vec![crate::sign_in::need()]
        } else {
            Vec::new()
        },
    })
}

/// Steery's plugin with the door's address filled in.
fn plugin(door: &str) -> Result<String, Error> {
    if door.is_empty() || door.contains(['"', '\\', '\n', '\r']) {
        return Err(Error::invalid(
            "`door` is not an address the plugin can hold",
        ));
    }
    Ok(PLUGIN.replace("__STEERY_DOOR__", door))
}

/// The release `amp --version` names (`0.0.1794225648-g45053b`, read as `0.0.1794225648`), or
/// none where it did not answer in time.
async fn release(binary: &Path) -> Option<String> {
    let (major, minor, patch) = release_within(&version(binary).await?)?;
    Some(format!("{major}.{minor}.{patch}"))
}
