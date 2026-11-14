//! `setup/plan`: what sets Warp up for Steery, each an entry the runtime writes and records:
//! Steery's MCP server in `~/.warp/.mcp.json`, under the key Warp reads there (`mcpServers`
//! where it has none), whose servers Warp starts on its own, and Steery's skill in Warp's
//! own skills folder.

use std::path::Path;

use serde_json::json;
use steery_driver_common::mcp::json;
use steery_driver_common::program::version;
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

use crate::{home, mcp};

/// The plan for this host.
pub async fn plan(home: &Path, setup: Setup) -> Planned {
    let Some(binary) = home::binary(home) else {
        let (install, refused) = match home::install() {
            Ok(install) => (Some(install), None),
            Err(why) => (None, Some(why)),
        };
        return Planned {
            found: None,
            refused,
            entries: Vec::new(),
            notes: vec![
                "Not installed here. The driver found no `oz` command and no Warp app. A \
                 `~/.warp` folder does not prove that Warp is installed."
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
            "`oz --version` named no release, so the driver says nothing about this build.".into(),
        );
    }
    // Warp reads one key of the file, the first that holds servers, so Steery's entry goes
    // under that key.
    let file = home::mcp_file(&home::config(home));
    let under = json(&file)
        .as_ref()
        .and_then(mcp::pointer)
        .unwrap_or("/mcpServers");
    let entries = vec![
        // Warp sends no `_meta` with a call, so the entry sends no `Steery-*` header.
        Entry {
            path: file,
            format: Format::Json,
            at: Some(format!("{under}/steery")),
            value: json!({"url": setup.mcp_url}),
            carries: vec![Part::Mcp],
        },
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

/// The release that `oz --version` names, as its date: `Oz v0.2026.11.11.08.17.stable_01`
/// is `2026.11.11`. None where it did not answer in time.
async fn release(binary: &Path) -> Option<String> {
    release_of(&version(binary).await?)
}

fn release_of(text: &str) -> Option<String> {
    let numbers = text
        .split_whitespace()
        .find_map(|word| word.strip_prefix("v0."))?;
    let parts: Vec<&str> = numbers.split('.').take(3).collect();
    (parts.len() == 3 && parts.iter().all(|part| part.parse::<u64>().is_ok()))
        .then(|| parts.join("."))
}
