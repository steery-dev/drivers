//! `setup/plan`: what sets Droid up for Steery, each an entry the runtime writes and
//! records: Steery's MCP server under `/mcpServers/steery` of `~/.factory/mcp.json`; one hook
//! per event Steery reads ([`super::hooks::EVENTS`]) in `~/.factory/hooks.json`, beside the
//! person's own; and Steery's skill in `~/.factory/skills/steery/SKILL.md`. Droid runs the
//! user's hooks in every folder, with no trust prompt.

use std::path::{Path, PathBuf};

use serde_json::json;
use steery_driver_common::program::{executable, lacks, on_path, others_note, release_within, version};
use steery_sdk::wire::harness::{Entry, Format, Found, Install, Part, Planned, Setup};

use super::{EXECUTABLE, MINIMUM};

/// Factory's own installer. It installs `droid` into `~/.local/bin`.
const INSTALLER: &str = "curl -fsSL https://app.factory.ai/cli | sh";

/// The folder Factory's installer writes `droid` to, which a shell that has not read its
/// profile since may not have on `PATH`.
fn installer_folder(home: &Path) -> PathBuf {
    home.join(".local/bin")
}

/// Droid's executable: the first on `PATH`, else the one in its installer's folder.
pub(crate) fn binary(home: &Path) -> Option<PathBuf> {
    on_path(EXECUTABLE).or_else(|| executable(installer_folder(home).join(EXECUTABLE)))
}

/// The plan for this machine, with Droid's folder `root` (`~/.factory`). A custom model runs
/// with no Factory account, so the plan asks for no sign-in.
pub async fn plan(home: &Path, root: &Path, setup: Setup) -> Planned {
    let Some(binary) = binary(home) else {
        let (install, refused) = match lacks(&["curl", "sh"]) {
            None => (
                Some(Install {
                    argv: vec!["sh".into(), "-c".into(), INSTALLER.into()],
                    about: format!(
                        "Factory's own installer. It installs droid into ~/.local/bin: \
                         {INSTALLER}"
                    ),
                }),
                None,
            ),
            Some(why) => (None, Some(format!("Factory's installer {why}"))),
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
    let least = release_within(MINIMUM).unwrap_or_default();
    let mut notes = Vec::new();
    let mut refused = None;
    match release {
        None => notes.push(
            "`--version` named no release, so the driver says nothing about this build.".into(),
        ),
        Some((major, minor, patch)) if (major, minor, patch) < least => {
            refused = Some(format!(
                "Droid {major}.{minor}.{patch} is older than {MINIMUM}, the oldest that Steery \
                 reads. Run `{EXECUTABLE} update`."
            ));
        }
        Some(_) => {}
    }
    notes.extend(others_note(
        "Droid",
        EXECUTABLE,
        Some(&installer_folder(home)),
        &binary,
    ));
    let entries = if refused.is_none() {
        notes.push(
            "Factory's own models need a sign-in (`droid`, then sign in, or `FACTORY_API_KEY`). \
             A custom model in Droid's settings runs without one."
                .into(),
        );
        self::entries(root, &setup)
    } else {
        Vec::new()
    };
    Planned {
        found: Some(Found {
            binary,
            release: release.map_or_else(String::new, |(major, minor, patch)| {
                format!("{major}.{minor}.{patch}")
            }),
            root: root.to_path_buf(),
        }),
        refused,
        entries,
        notes,
        install: None,
        needs: Vec::new(),
    }
}

pub(crate) fn entries(root: &Path, setup: &Setup) -> Vec<Entry> {
    let hooks = root.join("hooks.json");
    let mut entries = vec![
        Entry {
            path: root.join("mcp.json"),
            format: Format::Json,
            at: Some(format!("/mcpServers/{}", super::events::STEERY)),
            value: json!({
                "type": "http",
                "url": setup.mcp_url,
                "headers": {"Steery-Harness": "droid"},
            }),
            carries: vec![Part::Mcp],
        },
        Entry {
            path: root.join("skills/steery/SKILL.md"),
            format: Format::Text,
            at: None,
            value: json!(setup.skill),
            carries: vec![Part::Skill],
        },
    ];
    // One group per event with no `matcher`, which matches every tool, beside whatever the
    // person hooked there. Droid reads an exit of 2 as a block, so a hook whose command
    // cannot run says nothing instead. Its `timeout` is in seconds.
    entries.extend(super::hooks::EVENTS.iter().map(|event| Entry {
        path: hooks.clone(),
        format: Format::Json,
        at: Some(format!("/{}/-", event.name)),
        value: json!({
            "hooks": [{
                "type": "command",
                "command": format!("{} {} || true", setup.hook, event.name),
                "timeout": event.waits,
            }],
        }),
        carries: vec![Part::Hooks],
    }));
    entries
}
