//! `setup/plan`: what sets Kiro up for Steery, each an entry the runtime writes and records:
//! Steery's MCP server under `/mcpServers/steery` of `~/.kiro/settings/mcp.json`; Steery's
//! skill in `~/.kiro/skills/steery/SKILL.md`; Steery's own hooks file,
//! `~/.kiro/hooks/steery.json`, one hook per event Steery reads ([`super::hooks::EVENTS`]),
//! which Kiro loads beside the person's own files there; and the hook command line a turn
//! runs at a permission, `~/.kiro/steery/hook`.

use std::path::{Path, PathBuf};

use serde_json::json;
use steery_driver_common::program::{
    executable, lacks, on_path, others_note, release_within, version,
};
use steery_sdk::wire::harness::{Entry, Format, Found, Install, Part, Planned, Setup};

use super::{EXECUTABLE, MINIMUM};

/// Kiro's own installer, as its documentation gives it. It installs `kiro-cli` into
/// `~/.local/bin`.
const INSTALLER: &str = "curl -fsSL https://cli.kiro.dev/install | bash";

/// The folder Kiro's installer writes `kiro-cli` to, which a shell that has not read its
/// profile since may not have on `PATH`.
fn installer_folder(home: &Path) -> PathBuf {
    home.join(".local/bin")
}

/// Kiro's executable: the first on `PATH`, else the one in its installer's folder.
pub(crate) fn binary(home: &Path) -> Option<PathBuf> {
    on_path(EXECUTABLE).or_else(|| executable(installer_folder(home).join(EXECUTABLE)))
}

/// Steery's hook command line, which a turn runs at a permission Kiro asks for. Kiro reads
/// nothing under `~/.kiro/steery`.
pub(crate) fn hook_file(root: &Path) -> PathBuf {
    root.join("steery").join("hook")
}

/// The plan for this machine, with Kiro's folder `root` (`~/.kiro`).
pub async fn plan(home: &Path, root: &Path, setup: Setup) -> Planned {
    let Some(binary) = binary(home) else {
        let (install, refused) = match lacks(&["curl", "bash"]) {
            None => (
                Some(Install {
                    argv: vec!["bash".into(), "-c".into(), INSTALLER.into()],
                    about: format!(
                        "Kiro's own installer. It installs kiro-cli into ~/.local/bin: \
                         {INSTALLER}"
                    ),
                }),
                None,
            ),
            Some(why) => (None, Some(format!("Kiro's installer {why}"))),
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
                "Kiro CLI {major}.{minor}.{patch} is older than {MINIMUM}, the oldest that \
                 Steery reads. Run `{EXECUTABLE} update`."
            ));
        }
        Some(_) => {}
    }
    notes.extend(others_note(
        "Kiro",
        EXECUTABLE,
        Some(&installer_folder(home)),
        &binary,
    ));
    let entries = if refused.is_none() {
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
    vec![
        Entry {
            path: root.join("settings").join("mcp.json"),
            format: Format::Json,
            at: Some(format!("/mcpServers/{}", super::events::STEERY)),
            value: json!({
                "url": setup.mcp_url,
                "headers": {"Steery-Harness": "kiro"},
                // Milliseconds, per tool call: a Steery ask waits on its person.
                "timeout": u64::from(setup.timeout) * 1_000,
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
        Entry {
            path: super::hooks::file(root),
            format: Format::Json,
            at: None,
            value: super::hooks::document(&setup.hook),
            carries: vec![Part::Hooks],
        },
        Entry {
            path: hook_file(root),
            format: Format::Text,
            at: None,
            value: json!(setup.hook),
            carries: vec![Part::Hooks],
        },
    ]
}
