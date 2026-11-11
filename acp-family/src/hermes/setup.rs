//! `setup/plan`: Steery's skill, as a whole file in Hermes's skills folder, Steery's MCP
//! server under `mcp_servers.steery` in `<root>/config.yaml`, the only place Hermes reads MCP
//! servers from, and Steery's hook command line, which a turn runs at a permission Hermes
//! asks for. A `yaml` entry needs protocol 14; under an older one the plan says the command
//! that adds the server instead.

use std::path::{Path, PathBuf};

use serde_json::json;
use steery_driver_common::program::{
    executable, lacks, on_path, others_note, release_within, version,
};
use steery_sdk::wire::harness::{Entry, Format, Found, Install, Part, Planned, Setup};

/// Where Hermes's installer puts the command (`scripts/install.sh`, `~/.local/bin`).
fn installed() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    executable(home.join(".local/bin/hermes"))
}

/// The `hermes` that runs here: the one on `PATH`, else the installer's.
pub fn binary() -> Option<PathBuf> {
    on_path("hermes").or_else(installed)
}

/// Steery's hook command line. Hermes reads nothing under its home's `steery`.
pub fn hook_file(root: &Path) -> PathBuf {
    root.join("steery/hook")
}

pub async fn plan(root: &Path, setup: Setup, protocol: u32) -> Planned {
    let Some(binary) = binary() else {
        let (install, refused) = match lacks(&["bash", "curl", "git"]) {
            None => (
                Some(Install {
                    argv: vec![
                        "bash".into(),
                        "-c".into(),
                        "curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash".into(),
                    ],
                    about: "Hermes Agent's own installer. It puts its checkout in \
                            ~/.hermes/hermes-agent and the command in ~/.local/bin"
                        .into(),
                }),
                None,
            ),
            Some(why) => (None, Some(format!("hermes's installer {why}"))),
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
        others_note("Hermes Agent", "hermes", installed().as_deref(), &binary)
            .into_iter()
            .collect();
    if release.is_none() {
        notes.push(
            "`--version` did not answer, so the driver says nothing about this build.".into(),
        );
    }
    let mut entries = vec![Entry {
        path: root.join("skills/steery/SKILL.md"),
        format: Format::Text,
        at: None,
        value: json!(setup.skill),
        carries: vec![Part::Skill],
    }];
    if protocol >= steery_sdk::YAML {
        // One MCP client serves every conversation of a Hermes process, so the header names
        // the harness alone. Hermes reads `timeout` in seconds (`tools/mcp_tool_common.py`).
        entries.push(Entry {
            path: root.join("config.yaml"),
            format: Format::Yaml,
            at: Some("mcp_servers.steery".into()),
            value: json!({
                "url": setup.mcp_url,
                "headers": {"Steery-Harness": "hermes"},
                "timeout": setup.timeout,
            }),
            carries: vec![Part::Mcp],
        });
    } else {
        notes.push(format!(
            "This runtime cannot write Hermes's config.yaml: run \
             `hermes mcp add steery --url {}`.",
            setup.mcp_url
        ));
    }
    entries.push(Entry {
        path: hook_file(root),
        format: Format::Text,
        at: None,
        value: json!(setup.hook),
        carries: vec![Part::Hooks],
    });
    notes.push(
        "It gets the skill. Hermes itself decides when to ask and when to ask for the next \
         instruction. Nothing that Steery writes overrides that decision."
            .into(),
    );
    Planned {
        found: Some(Found {
            binary,
            release: release
                .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
                .unwrap_or_default(),
            root: root.to_owned(),
        }),
        refused: None,
        entries,
        notes,
        install: None,
        needs: Vec::new(),
    }
}
