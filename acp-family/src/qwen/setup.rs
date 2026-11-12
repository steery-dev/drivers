//! `setup/plan`: what sets Qwen Code up for Steery, each an entry the runtime writes and
//! records: Steery's MCP server under `/mcpServers/steery` and one hook per event Steery
//! reads ([`super::hooks::EVENTS`]) beside the person's own, both in the user
//! `<root>/settings.json`; Steery's skill in `<root>/skills/steery/SKILL.md`; and the
//! folders the runtime names in `<root>/trustedFolders.json`.
//!
//! Qwen's folder trust is off unless a person turns on `security.folderTrust.enabled`; where
//! it is on, a folder that is not trusted loses its project settings. The folders the
//! runtime names are trusted either way, so turning it on later changes nothing for them.

use std::path::{Path, PathBuf};

use serde_json::json;
use steery_driver_common::program::{ask, helper, on_path, release_within, version};
use steery_sdk::wire::harness::{Entry, Format, Found, Install, Part, Planned, Setup};

/// The oldest Node.js that Qwen Code's package runs on (its `engines.node`).
const NODE: (u64, u64, u64) = (22, 0, 0);

/// The plan for this machine, with Qwen's root `root` (its `.qwen`). Qwen has no offline
/// check of its sign-in, so the plan asks for none.
pub async fn plan(root: &Path, setup: Setup) -> Planned {
    let Some(binary) = super::binary() else {
        let (install, refused) = match install().await {
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
    found(binary, root, setup).await
}

/// The plan where `binary` is the `qwen` Steery runs.
async fn found(binary: PathBuf, root: &Path, setup: Setup) -> Planned {
    let release = version(&binary).await.as_deref().and_then(release_within);
    let mut notes = Vec::new();
    let mut refused = None;
    let mut entries = Vec::new();
    let least = release_within(super::MINIMUM).unwrap_or_default();
    match release {
        None => notes.push(
            "`--version` named no release, so the driver says nothing about this build.".into(),
        ),
        Some((major, minor, patch)) if (major, minor, patch) < least => {
            refused = Some(format!(
                "Qwen Code {major}.{minor}.{patch} is older than {}, the oldest that Steery \
                 reads. Run `npm install -g {}@latest`.",
                super::MINIMUM,
                super::PACKAGE,
            ));
        }
        Some(_) => {}
    }
    if refused.is_none() {
        entries = self::entries(root, &setup);
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
        needs: Vec::new(),
    }
}

fn entries(root: &Path, setup: &Setup) -> Vec<Entry> {
    let settings = root.join("settings.json");
    // `httpUrl` is the streamable HTTP transport, with no fall back to SSE. Qwen's MCP
    // client serves every conversation, so the header names the harness alone; a server
    // that wants the conversation reads `QWEN_CODE_SESSION_ID`. Its `timeout` is in
    // milliseconds (`MCP_DEFAULT_TIMEOUT_MSEC`).
    let mut entries = vec![
        Entry {
            path: settings.clone(),
            format: Format::Json,
            at: Some("/mcpServers/steery".into()),
            value: json!({
                "httpUrl": setup.mcp_url,
                "headers": {"Steery-Harness": "qwen"},
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
    ];
    // One group per event with no `matcher`, which matches every tool and source, beside
    // whatever the person hooked there, in the shape Qwen 0.27.2 ran. Each command is
    // [`super::hooks::command`]: the `PreToolUse` one blocks the call where Steery's hook
    // fails in a turn Steery started, the others say nothing. Its `timeout` is
    // [`super::hooks::timeout`]: seconds, or milliseconds from 1000 on (`hook-timeout.ts`).
    entries.extend(super::hooks::EVENTS.iter().map(|event| Entry {
        path: settings.clone(),
        format: Format::Json,
        at: Some(format!("/hooks/{}/-", event.name)),
        value: json!({
            "hooks": [{
                "type": "command",
                "command": super::hooks::command(&setup.hook, event.name),
                "timeout": super::hooks::timeout(event.waits),
            }],
        }),
        carries: vec![Part::Hooks],
    }));
    // Where folder trust is on, nobody answers its prompt on a host made for conversations,
    // so the folders the runtime names are trusted as their person would
    // (`trustedFolders.ts`, `TRUST_FOLDER`).
    let trusted = root.join("trustedFolders.json");
    entries.extend(setup.trusts.iter().filter_map(|folder| {
        let key = folder.to_str()?.replace('~', "~0").replace('/', "~1");
        Some(Entry {
            path: trusted.clone(),
            format: Format::Json,
            at: Some(format!("/{key}")),
            value: json!("TRUST_FOLDER"),
            carries: Vec::new(),
        })
    }));
    entries
}

/// Whether Qwen, run in `cwd`, runs Steery's guarded `PreToolUse` hook before every call:
/// a settings scope it loads holds a group with no `matcher` that runs the command setup
/// writes ([`super::hooks::guards`]), and no scope turns every hook off
/// (`disableAllHooks`). A turn Steery starts runs with `--approval-mode yolo`, so this hook
/// is its only gate.
pub(crate) fn gated(root: &Path, cwd: &Path) -> bool {
    let scopes = super::mcp::scopes(root, cwd);
    let off = scopes
        .iter()
        .rev()
        .find_map(|scope| scope.get("disableAllHooks"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    !off && scopes.iter().any(|scope| {
        scope["hooks"][super::hooks::BEFORE_CALL]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|group| group["matcher"].as_str().is_none_or(str::is_empty))
            .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
            .any(|hook| {
                hook["type"] == "command"
                    && hook["command"].as_str().is_some_and(super::hooks::guards)
            })
    })
}

/// npm's global install of Qwen Code, where `npm` and a Node.js new enough are here.
/// Where not, why, in one line.
async fn install() -> Result<Install, String> {
    let mut why: Vec<String> = on_path("npm")
        .is_none()
        .then(|| "`npm` is not on PATH".to_owned())
        .into_iter()
        .collect();
    why.extend(node_lacks().await);
    if !why.is_empty() {
        return Err(format!(
            "Qwen Code's installer needs Node.js 22 or newer and npm ({})",
            why.join("; ")
        ));
    }
    let package = super::PACKAGE;
    Ok(Install {
        argv: vec!["npm".into(), "install".into(), "-g".into(), package.into()],
        about: format!(
            "npm installs the package {package} into npm's global folder: npm install -g {package}"
        ),
    })
}

/// Why `node` cannot run Qwen Code: not on `PATH`, older than [`NODE`], or a
/// `--version` that failed or named no release. None where `node` is new enough.
async fn node_lacks() -> Option<String> {
    let Some(node) = on_path("node") else {
        return Some("`node` is not on PATH".into());
    };
    let mut command = helper(&node);
    command.arg("--version");
    let answer = match ask(command).await {
        Ok(answer) => answer,
        Err(why) => return Some(format!("`node --version` {why}")),
    };
    if answer.code != Some(0) {
        return Some(format!(
            "`node --version` exited {}",
            answer
                .code
                .map_or("by signal".to_owned(), |code| code.to_string())
        ));
    }
    let said = String::from_utf8_lossy(&answer.stdout).trim().to_owned();
    match release_within(&said) {
        None => Some(format!(
            "`node --version` said `{said}`, which names no release"
        )),
        Some(release) if release < NODE => Some(format!("`node` is {said}")),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Qwen Code 0.27.2 as npm installs it, on the machine that recorded the fixtures.
    const QWEN: &str = "/opt/homebrew/bin/qwen";

    #[tokio::test]
    async fn qwen_is_pointed_at_steery_in_its_own_spelling() {
        if !Path::new(QWEN).exists() {
            eprintln!("skipped: no {QWEN} here");
            return;
        }
        let root = std::env::temp_dir().join("steery-qwen-plan/.qwen");
        let setup = Setup {
            mcp_url: "http://127.0.0.1:7777/mcp".into(),
            skill: "# Steery".into(),
            hook: "/opt/steery/hook".into(),
            door: "http://127.0.0.1:7777/door".into(),
            release: "1.0.0".into(),
            timeout: 600,
            trusts: vec!["/home/daytona/work".into()],
        };
        let planned = found(QWEN.into(), &root, setup).await;
        let found = planned.found.unwrap();
        assert_eq!(
            (found.release.as_str(), found.root.as_path()),
            ("0.27.2", root.as_path())
        );
        assert_eq!((planned.refused, planned.install), (None, None));
        assert!(planned.needs.is_empty());
        let settings = root.join("settings.json");
        let mut entries = planned.entries.into_iter();
        let mcp = entries.next().unwrap();
        assert_eq!(
            (&mcp.path, mcp.at.as_deref()),
            (&settings, Some("/mcpServers/steery"))
        );
        assert_eq!(
            mcp.value,
            json!({"httpUrl": "http://127.0.0.1:7777/mcp",
                   "headers": {"Steery-Harness": "qwen"}, "timeout": 600_000})
        );
        let skill = entries.next().unwrap();
        assert_eq!(
            (skill.path, skill.value),
            (root.join("skills/steery/SKILL.md"), json!("# Steery"))
        );
        // What setup writes is what the hooks read: one entry per declared hook.
        for hook in super::super::hooks::table() {
            let entry = entries.next().unwrap();
            assert_eq!(
                (&entry.path, entry.carries.as_slice()),
                (&settings, &[Part::Hooks][..])
            );
            assert_eq!(entry.at, Some(format!("/hooks/{}/-", hook.event)));
            assert_eq!(
                entry.value,
                json!({"hooks": [{"type": "command",
                    "command": super::super::hooks::command("/opt/steery/hook", &hook.event),
                    "timeout": super::super::hooks::timeout(hook.waits.unwrap())}]})
            );
            let command = entry.value["hooks"][0]["command"].as_str().unwrap();
            assert_eq!(
                super::super::hooks::guards(command),
                hook.event == super::super::hooks::BEFORE_CALL
            );
        }
        let trust = entries.next().unwrap();
        assert_eq!(trust.path, root.join("trustedFolders.json"));
        assert_eq!(trust.at.as_deref(), Some("/~1home~1daytona~1work"));
        assert_eq!(trust.value, json!("TRUST_FOLDER"));
        assert!(entries.next().is_none());
    }
}
