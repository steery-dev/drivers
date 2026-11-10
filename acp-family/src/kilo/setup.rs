//! `setup/plan`: what sets Kilo up for Steery: Steery's skill, in its global folder's
//! `skills`; Steery's MCP server, under `mcp` in its global `kilo.json`, a file Kilo
//! merges with the person's own `kilo.jsonc`; Steery's plugin, in its global folder's
//! `plugin`, which Kilo loads in every process and which dials the runtime's door; and
//! Steery's hook command line, in a file of its own that the turns this driver starts read.
//! Kilo runs no hook command itself.

use std::path::{Path, PathBuf};

use serde_json::json;
use steery_driver_common::program::{self, release_within};
use steery_sdk::Error;
use steery_sdk::wire::harness::{Entry, Format, Found, Install, Part, Planned, Setup};

use super::calls::STEERY;

/// Steery's plugin for Kilo, the door's address in a placeholder inside a string literal.
const PLUGIN: &str = include_str!("../../plugin/kilo.ts");

/// Kilo's installer, as its docs give it: it puts `kilo` in `~/.kilo/bin`.
const INSTALLER: &str = "curl -fsSL https://kilo.ai/cli/install | bash";

/// Kilo's executable: the first `kilo` on `PATH`, else the one its installer put in
/// `~/.kilo/bin`.
pub fn binary(home: &Path) -> Option<PathBuf> {
    program::on_path("kilo").or_else(|| program::executable(home.join(".kilo/bin/kilo")))
}

/// Steery's hook command line, which a turn runs at a permission Kilo asks for. Kilo reads
/// nothing under its config folder's `steery`.
pub fn hook_file(config: &Path) -> PathBuf {
    config.join(STEERY).join("hook")
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

pub async fn plan(home: &Path, data: &Path, config: &Path, asked: Setup) -> Result<Planned, Error> {
    let Some(binary) = binary(home) else {
        return Ok(Planned {
            install: Some(Install {
                argv: vec!["sh".into(), "-c".into(), INSTALLER.into()],
                about: format!(
                    "Run `{INSTALLER}`, Kilo's installer, which puts `kilo` in ~/.kilo/bin."
                ),
            }),
            found: None,
            refused: None,
            entries: Vec::new(),
            notes: Vec::new(),
            needs: Vec::new(),
        });
    };
    let Some(release) = program::version(&binary)
        .await
        .as_deref()
        .and_then(release_within)
        .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
    else {
        return Ok(Planned {
            refused: Some(format!(
                "{} --version did not say its release",
                binary.display()
            )),
            found: None,
            entries: Vec::new(),
            notes: Vec::new(),
            install: None,
            needs: Vec::new(),
        });
    };
    let plugin = plugin(&asked.door)?;
    Ok(Planned {
        found: Some(Found {
            binary,
            release,
            root: data.to_path_buf(),
        }),
        refused: None,
        entries: vec![
            Entry {
                path: config.join("skills").join(STEERY).join("SKILL.md"),
                format: Format::Text,
                at: None,
                value: json!(asked.skill),
                carries: vec![Part::Skill],
            },
            Entry {
                path: config.join("plugin").join("steery.ts"),
                format: Format::Text,
                at: None,
                value: json!(plugin),
                carries: vec![Part::Plugin],
            },
            Entry {
                path: config.join("kilo.json"),
                format: Format::Json,
                at: Some(format!("/mcp/{STEERY}")),
                // One MCP client serves every conversation of a Kilo process, so no header
                // can name a conversation.
                value: json!({
                    "type": "remote",
                    "url": asked.mcp_url,
                    "oauth": false,
                    "timeout": u64::from(asked.timeout) * 1000,
                }),
                carries: vec![Part::Mcp],
            },
            Entry {
                path: hook_file(config),
                format: Format::Text,
                at: None,
                value: json!(asked.hook),
                carries: vec![Part::Hooks],
            },
        ],
        notes: Vec::new(),
        install: None,
        needs: Vec::new(),
    })
}
