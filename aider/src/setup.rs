//! `setup/plan`: Aider has nothing to set up for Steery. It reads no MCP server, no skill
//! and no hooks, so the plan finds Aider and writes no entry.

use std::path::Path;

use steery_driver_common::program::{release_within, version};
use steery_sdk::wire::harness::{Found, Planned, Setup};

use crate::home;

/// The plan for this host.
pub async fn plan(home: &Path, _setup: Setup) -> Planned {
    let Some(binary) = home::binary(home) else {
        let (install, refused) = match home::install() {
            Ok(install) => (Some(install), None),
            Err(why) => (None, Some(why)),
        };
        return Planned {
            found: None,
            refused,
            entries: Vec::new(),
            notes: vec!["Not installed here. The driver found no `aider` executable.".into()],
            install,
            needs: Vec::new(),
        };
    };
    let release = release(&binary).await;
    let mut notes = vec![
        "Aider reads no MCP server, skill or hooks, so Steery writes nothing to it. Aider takes \
         a model's key from its environment or a `.env` in the repository or home folder."
            .to_owned(),
    ];
    if release.is_none() {
        notes.push(
            "`--version` named no release, so the driver says nothing about this build.".into(),
        );
    }
    Planned {
        found: Some(Found {
            binary,
            release: release.unwrap_or_default(),
            root: home.join(".aider"),
        }),
        refused: None,
        entries: Vec::new(),
        notes,
        install: None,
        needs: Vec::new(),
    }
}

/// The release `aider --version` names (`aider 0.86.2`), or none where it did not answer in
/// time.
async fn release(binary: &Path) -> Option<String> {
    let (major, minor, patch) = release_within(&version(binary).await?)?;
    Some(format!("{major}.{minor}.{patch}"))
}
