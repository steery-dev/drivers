//! `setup/plan`: what sets Grok Build up for Steery, each an entry the runtime writes and
//! records: Steery's MCP server under `[mcp_servers.steery]` in `<root>/config.toml`,
//! Steery's skill in `<root>/skills/steery/SKILL.md`, and Steery's hooks as a file of their
//! own, `<root>/hooks/steery.json`, beside any the person keeps there (Grok reads every
//! `*.json` in that folder).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use steery_driver_common::program::{lacks, others_note, release_within, version};
use steery_sdk::wire::harness::{Entry, Format, Found, Install, Part, Planned, Setup};

/// The oldest release whose conversations, hooks and `[mcp_servers]` this driver reads.
const MINIMUM: (u64, u64, u64) = (1, 0, 77);

/// The plan for this machine, with Grok's home `root`. Where `needs` (the protocol picked
/// has them) and Grok says it is not signed in, the plan asks for a sign-in.
pub async fn plan(root: &Path, setup: Setup, needs: bool) -> Planned {
    let Some(binary) = super::binary(root) else {
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
        Some((major, minor, patch)) if (major, minor, patch) < MINIMUM => {
            let (least_major, least_minor, least_patch) = MINIMUM;
            refused = Some(format!(
                "Grok Build {major}.{minor}.{patch} is older than \
                 {least_major}.{least_minor}.{least_patch}, the oldest that Steery reads. Run \
                 `grok update`."
            ));
        }
        Some(_) => {}
    }
    let mut asked = Vec::new();
    if refused.is_none() {
        notes.extend(others_note(
            "grok",
            "grok",
            Some(&root.join("bin")),
            &binary,
        ));
        entries = self::entries(root, &setup);
        if needs && super::sign_in::lacking(root).await {
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
    // Grok's MCP client serves every conversation, so the header names the harness alone.
    // Grok calls the header table `headers`, not Codex's `http_headers`.
    let mcp = Entry {
        path: root.join("config.toml"),
        format: Format::Toml,
        at: Some("mcp_servers.steery".into()),
        value: json!({
            "url": setup.mcp_url,
            "headers": {"Steery-Harness": "grok"},
            "tool_timeout_sec": setup.timeout,
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
    let hooks: Map<String, Value> = super::hooks::EVENTS
        .iter()
        .map(|event| {
            let group = json!([{"hooks": [{
                "type": "command",
                "command": super::hooks::command(&setup.hook, event.name),
                "timeout": event.waits,
            }]}]);
            (event.name.to_owned(), group)
        })
        .collect();
    let hooks = Entry {
        path: hooks_file(root),
        format: Format::Json,
        at: None,
        value: json!({"hooks": hooks}),
        carries: vec![Part::Hooks],
    };
    vec![mcp, skill, hooks]
}

/// Where setup writes Steery's hooks.
pub(crate) fn hooks_file(root: &Path) -> PathBuf {
    root.join("hooks/steery.json")
}

/// Whether Steery's `PreToolUse` hook is in the file setup wrote, on every tool: a group
/// with no `matcher` that runs the command setup writes ([`super::hooks::guards`]). A turn
/// Steery starts runs with `--always-approve`, so this hook is its only gate beside the
/// person's `deny` rules, and a hook's `allow` does not get past Grok's own ask, which a
/// headless run cancels. Grok 1.0.77 still runs the call when the hook times out.
pub(crate) fn gated(root: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(hooks_file(root)) else {
        return false;
    };
    let Ok(document) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    document["hooks"][super::hooks::BEFORE_CALL]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|group| group.get("matcher").is_none())
        .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
        .any(|hook| {
            hook["type"] == "command"
                && hook["command"].as_str().is_some_and(super::hooks::guards)
        })
}

/// Grok Build's own installer, where `bash` and `curl` are here: it links `grok` into
/// `~/.grok/bin`. Where not, why, in one line.
fn install() -> Result<Install, String> {
    if let Some(why) = lacks(&["bash", "curl"]) {
        return Err(format!("grok's installer {why}"));
    }
    let line = "curl -fsSL https://x.ai/cli/install.sh | bash";
    Ok(Install {
        argv: vec!["bash".into(), "-c".into(), line.into()],
        about: format!("Grok Build's own installer. It installs grok into ~/.grok/bin: {line}"),
    })
}
