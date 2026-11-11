//! `setup/plan`: what sets Goose up for Steery, each an entry the runtime writes and
//! records: Steery's MCP server as an extension in Goose's `config.yaml`, and Steery's hook
//! command line, in a file of its own that the turns this driver starts read. Goose runs no
//! hook command itself.

use std::path::{Path, PathBuf};

use serde_json::json;
use steery_driver_common::program::{others_note, release_within, version};
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

use super::place::{self, Place};

/// Steery's MCP server, as the entry names it: also the key of its extension.
const SERVER: &str = "steery";

/// Steery's hook command line, which a turn runs at a permission Goose asks for. Goose
/// reads nothing under its configuration folder's `steery`.
pub(crate) fn hook_file(config: &Path) -> PathBuf {
    config.join(SERVER).join("hook")
}

/// The plan for this machine under the protocol picked. Whether the release found is one
/// Steery speaks to is the runtime's to say, from `initialize`'s minimum.
pub(crate) async fn plan(place: &Place, protocol: u32, setup: Setup) -> Planned {
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
    let mut notes: Vec<String> =
        others_note("Goose", "goose", place.installed().as_deref(), &binary)
            .into_iter()
            .collect();
    if release.is_none() {
        notes.push(
            "`--version` did not answer, so the driver says nothing about this build.".into(),
        );
    }
    let mut entries = Vec::new();
    if protocol >= steery_sdk::YAML {
        entries.push(Entry {
            path: place.config_yaml(),
            format: Format::Yaml,
            at: Some(format!("extensions.{SERVER}")),
            value: json!({
                "enabled": true,
                "type": "streamable_http",
                "name": SERVER,
                "description": "Steery",
                "uri": setup.mcp_url,
                "headers": {"Steery-Harness": "goose"},
                "timeout": setup.timeout,
            }),
            carries: vec![Part::Mcp],
        });
    } else {
        notes.push(format!(
            "Goose keeps MCP servers only in config.yaml, which this runtime cannot write: add \
             the extension with `goose configure` (Add Extension, Remote Extension (Streamable \
             HTTP), named {SERVER}, at {}).",
            setup.mcp_url
        ));
    }
    entries.push(Entry {
        path: hook_file(&place.config),
        format: Format::Text,
        at: None,
        value: json!(setup.hook),
        carries: vec![Part::Hooks],
    });
    Planned {
        found: Some(Found {
            binary,
            release: release
                .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
                .unwrap_or_default(),
            root: place.config.clone(),
        }),
        refused: None,
        entries,
        notes,
        install: None,
        needs: Vec::new(),
    }
}
