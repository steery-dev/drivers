//! `setup/plan`: Steery's skill, as a whole file in Hermes's skills folder. Hermes keeps its
//! MCP servers only in `config.yaml`. No runtime merges a `yaml` entry yet, so the plan says
//! the command that adds Steery's server instead of planning it.

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

pub async fn plan(root: &Path, setup: Setup) -> Planned {
    let Some(binary) = on_path("hermes").or_else(installed) else {
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
    notes.push(format!(
        "Hermes keeps MCP servers only in config.yaml, which setup cannot write yet: run \
         `hermes mcp add steery --url {}`.",
        setup.mcp_url
    ));
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
        entries: vec![Entry {
            path: root.join("skills/steery/SKILL.md"),
            format: Format::Text,
            at: None,
            value: json!(setup.skill),
            carries: vec![Part::Skill],
        }],
        notes,
        install: None,
        needs: Vec::new(),
    }
}
