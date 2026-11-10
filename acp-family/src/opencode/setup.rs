//! `setup/plan`: what sets OpenCode 2 up for Steery, each an entry the runtime writes and
//! records: Steery's MCP server in the global `opencode.json`, Steery's skill in the global
//! `skills` folder, Steery's plugin in the global `plugin` folder, which OpenCode loads in
//! every server process and which dials the runtime's door, and Steery's hook command line,
//! in a file of its own that the turns this driver starts read. OpenCode's agents allow every
//! tool unless a person denies one, so Steery's tools need no approval entry, and OpenCode
//! runs no hook command itself.

use std::path::Path;

use serde_json::json;
use steery_sdk::Error;
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

use super::config;
use super::place::{self, Place};

/// Steery's MCP server, as the entry names it.
const SERVER: &str = "steery";

/// Steery's plugin for OpenCode, the door's address in a placeholder inside a string literal.
const PLUGIN: &str = include_str!("../../plugin/opencode.ts");

/// Steery's hook command line, which a turn runs at a permission OpenCode asks for.
/// OpenCode reads nothing under its config folder's `steery`.
pub(crate) fn hook_file(config: &Path) -> std::path::PathBuf {
    config.join(SERVER).join("hook")
}

/// Steery's plugin with the door's address filled in.
fn plugin(door: &str) -> Result<String, Error> {
    if door.is_empty() || door.contains(['"', '\\', '\n', '\r']) {
        return Err(Error::invalid(format!(
            "this door cannot be written into a string literal: {door}"
        )));
    }
    Ok(PLUGIN.replace("__STEERY_DOOR__", door))
}

/// The plan for this machine. Whether the release found is one Steery speaks to is the
/// runtime's to say, from `initialize`'s minimum.
pub(crate) async fn plan(place: &Place, setup: Setup) -> Result<Planned, Error> {
    let (second, first) = place::binary(place).await;
    let Some(binary) = second else {
        let mut notes = vec![match &first {
            Some(first) => format!(
                "OpenCode {} is installed at {}. Steery reads OpenCode 2 only. OpenCode 2's \
                 first run adds its own tables to the same database and keeps OpenCode 1's.",
                first.release.as_deref().unwrap_or("of an unnamed release"),
                first.path.display()
            ),
            None => "Not installed here. The driver found no executable. A configuration \
                     directory does not prove that an executable is installed."
                .into(),
        }];
        if let Some(first) = &first
            && first.path.parent() == Some(place.installer_folder().as_path())
        {
            notes.push(format!(
                "The installer replaces {}: OpenCode 1 is not there afterwards.",
                first.path.display()
            ));
        }
        let (install, refused) = match place::install() {
            Ok(install) => (Some(install), None),
            Err(why) => (None, Some(why)),
        };
        return Ok(Planned {
            found: None,
            refused,
            entries: Vec::new(),
            notes,
            install,
            needs: Vec::new(),
        });
    };
    let mut notes = Vec::new();
    if binary.release.is_none() {
        notes.push("`--version` named no release, so the driver says nothing about this build.".into());
    }
    if let Some(first) = &first {
        notes.push(format!(
            "OpenCode {} at {} is ahead on PATH. A person who types `opencode` runs it, and \
             Steery does not read its conversations.",
            first.release.as_deref().unwrap_or("1"),
            first.path.display()
        ));
    }
    let plugin = plugin(&setup.door)?;
    let settings = place.config.join("opencode.json");
    notes.extend(strict(&settings));
    let entries = vec![
        // One MCP client serves every conversation, so the entry names the harness alone and
        // each call's `_meta` the conversation (`identity.call_meta`). OpenCode counts an MCP
        // timeout in milliseconds.
        Entry {
            path: settings,
            format: Format::Json,
            at: Some(format!("/mcp/servers/{SERVER}")),
            value: json!({
                "type": "remote",
                "url": setup.mcp_url,
                "headers": {"Steery-Harness": "opencode"},
                "timeout": {"execution": u64::from(setup.timeout) * 1000},
            }),
            carries: vec![Part::Mcp],
        },
        Entry {
            path: place.config.join("skills/steery/SKILL.md"),
            format: Format::Text,
            at: None,
            value: json!(setup.skill),
            carries: vec![Part::Skill],
        },
        Entry {
            path: place.config.join("plugin/steery.ts"),
            format: Format::Text,
            at: None,
            value: json!(plugin),
            carries: vec![Part::Plugin],
        },
        Entry {
            path: hook_file(&place.config),
            format: Format::Text,
            at: None,
            value: json!(setup.hook),
            carries: vec![Part::Hooks],
        },
    ];
    Ok(Planned {
        found: Some(Found {
            binary: binary.path,
            release: binary.release.unwrap_or_default(),
            root: place.config.clone(),
        }),
        refused: None,
        entries,
        notes,
        install: None,
        needs: Vec::new(),
    })
}

/// A line where `opencode.json` holds comments or trailing commas, which OpenCode reads and
/// a JSON writer does not.
fn strict(settings: &Path) -> Option<String> {
    let text = std::fs::read_to_string(settings).ok()?;
    if serde_json::from_str::<serde_json::Value>(&text).is_ok() || config::parse(&text).is_err() {
        return None;
    }
    Some(format!(
        "{} holds comments or trailing commas. Steery writes JSON only, so it cannot add its \
         server there until they are gone.",
        settings.display()
    ))
}
