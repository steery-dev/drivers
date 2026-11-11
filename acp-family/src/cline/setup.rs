//! `setup/plan`: what sets Cline up for Steery, each an entry the runtime writes and
//! records: Steery's MCP server in Cline's MCP settings, and Steery's hook command line, in
//! a file of its own that the turns this driver starts read. Cline runs no hook command
//! itself.

use std::path::{Path, PathBuf};

use serde_json::json;
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

use super::place::{self, Place};

/// Steery's MCP server, as the entry names it.
const SERVER: &str = "steery";

/// Steery's hook command line, which a turn runs at a permission Cline asks for. Cline
/// reads nothing under its folder's `steery`.
pub(crate) fn hook_file(dir: &Path) -> PathBuf {
    dir.join(SERVER).join("hook")
}

/// The plan for this machine. Whether the release found is one Steery speaks to is the
/// runtime's to say, from `initialize`'s minimum.
pub(crate) async fn plan(place: &Place, setup: Setup) -> Planned {
    let Some(binary) = place::binary() else {
        let (install, refused) = match place::install() {
            Ok(install) => (Some(install), None),
            Err(why) => (None, Some(why)),
        };
        return Planned {
            found: None,
            refused,
            entries: Vec::new(),
            notes: vec![
                "Not installed here. The driver found no executable. A configuration \
                 directory does not prove that an executable is installed."
                    .into(),
            ],
            install,
            needs: Vec::new(),
        };
    };
    let release = steery_driver_common::program::version(&binary).await;
    let mut notes = vec![
        "Live goes through Cline's hub (`cline hub ensure`). A Cline window joins the hub only \
         where it finds one running, and in `--yolo` or `--sandbox` never: such a window runs \
         its turns itself and has no door, so `say` is refused `no-door`."
            .to_owned(),
    ];
    if release.is_none() {
        notes.push(
            "`--version` named no release, so the driver says nothing about this build.".into(),
        );
    }
    let settings = place.mcp();
    if let Ok(text) = std::fs::read_to_string(&settings)
        && serde_json::from_str::<serde_json::Value>(&text).is_err()
    {
        notes.push(format!(
            "{} is not JSON. Steery cannot add its server there until it is.",
            settings.display()
        ));
    }
    let entries = vec![
        // Cline counts an MCP timeout in seconds.
        Entry {
            path: settings,
            format: Format::Json,
            at: Some(format!("/mcpServers/{SERVER}")),
            value: json!({
                "transport": {
                    "type": "streamableHttp",
                    "url": setup.mcp_url,
                    "headers": {"Steery-Harness": "cline"},
                },
                "timeout": setup.timeout,
            }),
            carries: vec![Part::Mcp],
        },
        Entry {
            path: hook_file(&place.dir),
            format: Format::Text,
            at: None,
            value: json!(setup.hook),
            carries: vec![Part::Hooks],
        },
    ];
    Planned {
        found: Some(Found {
            binary,
            release: release.unwrap_or_default(),
            root: place.dir.clone(),
        }),
        refused: None,
        entries,
        notes,
        install: None,
        needs: Vec::new(),
    }
}
