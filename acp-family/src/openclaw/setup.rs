//! `setup/plan`: what sets OpenClaw up for Steery, each an entry the runtime writes and
//! records: Steery's MCP server at `/mcp/servers/steery` of `<root>/openclaw.json`,
//! Steery's skill in `<root>/skills/steery/SKILL.md`, the managed skills folder OpenClaw
//! reads for every agent (`src/skills/loading/workspace-skill-sources.ts:101,148`), and
//! Steery's hook command line, which a turn of this driver's runs at a permission OpenClaw
//! asks for.

use std::path::{Path, PathBuf};

use serde_json::json;
use steery_driver_common::program::{
    executable, lacks, on_path, others_note, release_within, version,
};
use steery_sdk::wire::harness::{Entry, Format, Found, Install, Part, Planned, Setup};

/// The oldest release whose store, `mcp.servers` and `models status --check` this driver
/// reads.
pub const MINIMUM_TEXT: &str = "2026.11.3";
const MINIMUM: (u64, u64, u64) = (2026, 11, 3);

/// The `openclaw` that Steery runs: the first on `PATH`, else where OpenClaw's installer
/// puts it: npm's user prefix `~/.npm-global/bin`, which it sets where the global prefix is
/// not writable (`install.sh` `fix_npm_permissions`), else the wrapper of its git install,
/// `~/.local/bin/openclaw` (`install.sh` `publish_executable_wrapper`).
pub(crate) fn binary(home: &Path) -> Option<PathBuf> {
    on_path("openclaw")
        .or_else(|| executable(installed(home).join("openclaw")))
        .or_else(|| executable(home.join(".local/bin/openclaw")))
}

/// The npm user prefix's `bin` that OpenClaw's installer falls back to.
fn installed(home: &Path) -> PathBuf {
    home.join(".npm-global/bin")
}

/// Steery's hook command line. OpenClaw reads nothing under its state folder's `steery`.
pub fn hook_file(root: &Path) -> PathBuf {
    root.join("steery/hook")
}

/// The plan for this machine, with the person's `home` and OpenClaw's state folder `root`.
/// Where `needs` (the protocol picked has them) and `openclaw models status --check` says a
/// model in use lacks its sign-in, the plan asks for one.
pub async fn plan(home: &Path, root: &Path, setup: Setup, needs: bool) -> Planned {
    let Some(binary) = binary(home) else {
        let (install, refused) = match install() {
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
    let mut notes = Vec::new();
    let mut refused = None;
    let mut entries = Vec::new();
    match release {
        None => notes.push(
            "`--version` named no release, so the driver says nothing about this build.".into(),
        ),
        Some(release) if release < MINIMUM => {
            let (major, minor, patch) = release;
            refused = Some(format!(
                "OpenClaw {major}.{minor}.{patch} is older than {MINIMUM_TEXT}, the oldest that \
                 Steery reads. Run `openclaw update`."
            ));
        }
        Some(_) => {}
    }
    let mut asked = Vec::new();
    if refused.is_none() {
        notes.extend(others_note(
            "openclaw",
            "openclaw",
            Some(&installed(home)),
            &binary,
        ));
        let config = root.join("openclaw.json");
        if let Ok(text) = std::fs::read(&config)
            && serde_json::from_slice::<serde_json::Value>(&text).is_err()
        {
            notes.push(format!(
                "{} is not plain JSON. OpenClaw reads JSON5 (comments, trailing commas), \
                 the runtime writes JSON only: make the file plain JSON, or add the steery \
                 server under mcp.servers yourself.",
                config.display()
            ));
        }
        entries = self::entries(root, &setup);
        if needs && super::sign_in::lacking(home).await {
            asked.push(super::sign_in::need());
        }
    }
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
        needs: asked,
    }
}

fn entries(root: &Path, setup: &Setup) -> Vec<Entry> {
    // `openclaw acp` refuses the servers an ACP client forwards, so Steery's sits in the
    // person's own `mcp.servers`, which serves every conversation: the header names the
    // harness alone. A URL server without `transport` is spoken to over SSE
    // (`src/agents/mcp-transport-config.ts:138`).
    let mcp = Entry {
        path: root.join("openclaw.json"),
        format: Format::Json,
        at: Some("/mcp/servers/steery".into()),
        value: json!({
            "url": setup.mcp_url,
            "transport": "streamable-http",
            "headers": {"Steery-Harness": "openclaw"},
            "requestTimeoutMs": u64::from(setup.timeout) * 1000,
        }),
        carries: vec![Part::Mcp],
    };
    let skill = Entry {
        path: root.join("skills/steery/SKILL.md"),
        format: Format::Text,
        at: None,
        value: json!(setup.skill),
        carries: vec![Part::Skill],
    };
    let hook = Entry {
        path: hook_file(root),
        format: Format::Text,
        at: None,
        value: json!(setup.hook),
        carries: vec![Part::Hooks],
    };
    vec![mcp, skill, hook]
}

/// OpenClaw's own installer, where `bash` and `curl` are here: it installs Node where it
/// lacks one, then `openclaw` with npm globally. `--no-onboard --no-prompt`: nobody is at
/// its terminal (`install.sh` `usage`). Where not, why, in one line.
fn install() -> Result<Install, String> {
    if let Some(why) = lacks(&["bash", "curl"]) {
        return Err(format!("OpenClaw's installer {why}"));
    }
    let line = "curl -fsSL https://openclaw.ai/install.sh | bash -s -- --no-onboard --no-prompt";
    Ok(Install {
        argv: vec!["bash".into(), "-c".into(), line.into()],
        about: format!("OpenClaw's own installer. It installs openclaw with npm, globally: {line}"),
    })
}
