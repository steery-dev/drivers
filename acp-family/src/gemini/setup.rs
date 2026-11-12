//! `setup/plan`: what sets Gemini CLI up for Steery, each an entry the runtime writes and
//! records: Steery's MCP server under `/mcpServers/steery` and one hook per event Steery
//! reads ([`super::hooks::EVENTS`]) beside the person's own, both in the user
//! `<root>/settings.json`; Steery's skill in `<root>/skills/steery/SKILL.md`; the hook
//! command line that a turn Steery runs asks at a permission, in `<root>/steery/hook`; the
//! address of the runtime's plugin door, which a turn Steery runs dials as the
//! conversation's copy, in `<root>/steery/door`; and the folders the runtime names in
//! `<root>/trustedFolders.json`.
//!
//! Gemini runs the hooks of `settings.json` only in a folder its person trusted
//! (`hookRegistry.ts`, folder trust is on by default). Its `trusted_hooks.json` holds only
//! the hooks of a project's own `.gemini/settings.json`, which it warns about once: Steery's
//! user hooks need no entry there.

use std::path::{Path, PathBuf};

use serde_json::json;
use steery_driver_common::program::{ask, helper, on_path, release_within, version};
use steery_sdk::wire::harness::{Entry, Format, Found, Install, Part, Planned, Setup};

/// Steery's hook command line, which a turn runs at a permission Gemini asks for.
pub(crate) fn hook_file(root: &Path) -> PathBuf {
    root.join("steery").join("hook")
}

/// The address of the runtime's plugin door, which a turn Steery runs dials.
pub(crate) fn door_file(root: &Path) -> PathBuf {
    root.join("steery").join("door")
}

/// The oldest Node.js that Gemini CLI's package runs on (its `engines.node`).
const NODE: (u64, u64, u64) = (20, 0, 0);

/// The plan for this machine, with Gemini's root `root` (its `.gemini`). Gemini has no
/// offline check of its sign-in, so the plan asks for none.
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

/// The plan where `binary` is the `gemini` Steery runs.
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
                "Gemini CLI {major}.{minor}.{patch} is older than {}, the oldest that Steery \
                 reads. Run `npm install -g {}@latest`.",
                super::MINIMUM,
                super::PACKAGE,
            ));
        }
        Some(_) => {}
    }
    if refused.is_none() {
        entries = self::entries(root, &setup);
        notes.push(
            "Gemini CLI runs Steery's hooks only in a folder that you trust. It asks the first \
             time it opens in a folder."
                .into(),
        );
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
    // `httpUrl` is the streamable HTTP transport, with no fall back to SSE. Gemini's MCP
    // client serves every conversation, so the header names the harness alone. Its
    // `timeout` is in milliseconds (`mcp-client.ts`).
    let mut entries = vec![
        Entry {
            path: settings.clone(),
            format: Format::Json,
            at: Some("/mcpServers/steery".into()),
            value: json!({
                "httpUrl": setup.mcp_url,
                "headers": {"Steery-Harness": "gemini"},
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
            path: hook_file(root),
            format: Format::Text,
            at: None,
            value: json!(setup.hook),
            carries: vec![Part::Hooks],
        },
        Entry {
            path: door_file(root),
            format: Format::Text,
            at: None,
            value: json!(setup.door),
            carries: vec![Part::Plugin],
        },
    ];
    // One group per event with no `matcher`, which matches every tool and source, beside
    // whatever the person hooked there. Gemini hands the command to a shell and reads an
    // exit of 2 or more as a block, so a hook whose command cannot run (a machine
    // mid-upgrade, a filled disk) says nothing instead. Its `timeout` is in milliseconds.
    entries.extend(super::hooks::EVENTS.iter().map(|event| Entry {
        path: settings.clone(),
        format: Format::Json,
        at: Some(format!("/hooks/{}/-", event.name)),
        value: json!({
            "hooks": [{
                "type": "command",
                "name": "steery",
                "command": format!("{} {} || true", setup.hook, event.name),
                "timeout": u64::from(event.waits) * 1_000,
            }],
        }),
        carries: vec![Part::Hooks],
    }));
    // Gemini runs the hooks of `settings.json` only in a folder its person trusted: on a
    // host made for its conversations nobody answers its trust prompt, so the folders the
    // runtime names are trusted as their person would (`trust.ts`, `TrustLevel`).
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

/// npm's global install of Gemini CLI, where `npm` and a Node.js new enough are here.
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
            "Gemini CLI's installer needs Node.js 20 or newer and npm ({})",
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

/// Why `node` cannot run Gemini CLI: not on `PATH`, older than [`NODE`], or a
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

    /// Gemini CLI 0.67.0 as npm installs it, on the machine that recorded the corpus.
    const GEMINI: &str = "/tmp/gem/prefix/bin/gemini";

    #[tokio::test]
    async fn gemini_is_pointed_at_steery_in_its_own_spelling() {
        if !Path::new(GEMINI).exists() {
            eprintln!("skipped: no {GEMINI} here");
            return;
        }
        let root = std::env::temp_dir().join("steery-gemini-plan/.gemini");
        let setup = Setup {
            mcp_url: "http://127.0.0.1:7777/mcp".into(),
            skill: "# Steery".into(),
            hook: "/opt/steery/hook".into(),
            door: "http://127.0.0.1:7777/door".into(),
            release: "1.0.0".into(),
            timeout: 600,
            trusts: vec!["/home/daytona/work".into()],
        };
        let planned = found(GEMINI.into(), &root, setup).await;
        let found = planned.found.unwrap();
        assert_eq!(
            (found.release.as_str(), found.root.as_path()),
            ("0.67.0", root.as_path())
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
                   "headers": {"Steery-Harness": "gemini"}, "timeout": 600_000})
        );
        let skill = entries.next().unwrap();
        assert_eq!(
            (skill.path, skill.value),
            (root.join("skills/steery/SKILL.md"), json!("# Steery"))
        );
        // The command line a turn of Steery's runs at a permission.
        let hook_file = entries.next().unwrap();
        assert_eq!(
            (hook_file.path, hook_file.value, hook_file.carries),
            (
                root.join("steery/hook"),
                json!("/opt/steery/hook"),
                vec![Part::Hooks]
            )
        );
        // The door a turn of Steery's dials as the conversation's copy.
        let door_file = entries.next().unwrap();
        assert_eq!(
            (door_file.path, door_file.value, door_file.carries),
            (
                root.join("steery/door"),
                json!("http://127.0.0.1:7777/door"),
                vec![Part::Plugin]
            )
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
                json!({"hooks": [{"type": "command", "name": "steery",
                    "command": format!("/opt/steery/hook {} || true", hook.event),
                    "timeout": u64::from(hook.waits.unwrap()) * 1_000}]})
            );
        }
        let trust = entries.next().unwrap();
        assert_eq!(trust.path, root.join("trustedFolders.json"));
        assert_eq!(trust.at.as_deref(), Some("/~1home~1daytona~1work"));
        assert_eq!(trust.value, json!("TRUST_FOLDER"));
        assert!(entries.next().is_none());
    }
}
