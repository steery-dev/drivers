//! `setup/plan`: what sets Copilot up for Steery. Steery's MCP server among the person's
//! servers in `mcp-config.json`; Steery's skill in Copilot's skills folder; and Steery's own
//! hooks file in Copilot's `hooks` folder, one hook per event Steery reads
//! ([`super::hooks::EVENTS`]), loaded beside the person's own hooks files. Where `needs`
//! (protocol 5) and Copilot is not signed in, its sign-in ([`super::sign_in::need`]).

use std::path::Path;

use serde_json::{Map, Value, json};
use steery_driver_common::program::{release_within, version};
use steery_sdk::Error;
use steery_sdk::wire;
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

use super::{STEERY, home, hooks, sign_in};

pub fn facts() -> wire::SetupFacts {
    wire::SetupFacts {
        reads_skills: true,
        cuts_tool_text: false,
        call_bound: None,
    }
}

/// The plan for this machine; with its needs where the runtime's protocol has them.
pub async fn plan(_home: &Path, root: &Path, setup: Setup, needs: bool) -> Result<Planned, Error> {
    let Some(binary) = home::binary() else {
        let (install, refused) = match home::install() {
            Ok(install) => (Some(install), None),
            Err(why) => (None, Some(why)),
        };
        return Ok(Planned {
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
        });
    };
    let release = named(version(&binary).await.as_deref().and_then(release_within));
    let needs = match needs && sign_in::lacking(&binary).await {
        true => vec![sign_in::need()],
        false => Vec::new(),
    };
    let mut notes = Vec::new();
    if release.is_none() {
        notes.push(
            "`--version` did not answer, so the driver says nothing about this build.".into(),
        );
    }
    notes.push(
        "Steery reaches a Copilot window only when it starts with `copilot --ui-server`. \
         A window that Steery opens has it; start your own windows the same way."
            .into(),
    );
    notes.extend(home::others_note(&binary));
    Ok(Planned {
        found: Some(Found {
            binary,
            release: release.unwrap_or_default(),
            root: root.to_owned(),
        }),
        refused: None,
        entries: entries(root, &setup),
        notes,
        install: None,
        needs,
    })
}

/// Every entry, in the order Copilot reads them.
fn entries(root: &Path, setup: &Setup) -> Vec<Entry> {
    vec![
        // A per-server `timeout`, in milliseconds, bounds tool discovery and each call.
        Entry {
            path: root.join("mcp-config.json"),
            format: Format::Json,
            at: Some(format!("/mcpServers/{STEERY}")),
            value: json!({
                "type": "http",
                "url": setup.mcp_url,
                "timeout": u64::from(setup.timeout) * 1_000,
            }),
            carries: vec![Part::Mcp],
        },
        Entry {
            path: root.join("skills").join(STEERY).join("SKILL.md"),
            format: Format::Text,
            at: None,
            value: json!(setup.skill),
            carries: vec![Part::Skill],
        },
        // Steery's own file, whole: Copilot loads every file in its `hooks` folder beside the
        // others, so the person's hooks stay where they are. Copilot hands `bash` to a shell,
        // and reads a non-zero exit at `preToolUse` as a denied call. A hook whose command
        // cannot run (a machine mid-upgrade, a filled disk) says nothing, except at a gate in
        // a turn Steery started, where the call is blocked.
        Entry {
            path: root.join("hooks").join(format!("{STEERY}.json")),
            format: Format::Json,
            at: None,
            value: json!({"version": 1, "hooks": hooked(&setup.hook)}),
            carries: vec![Part::Hooks],
        },
    ]
}

/// In a turn Steery started, `STEERY_STARTED` names a turn and the pid it started: Copilot
/// itself, as the turn mode execs it, which is the parent of the shell running the hook.
const GUARD: &str = r#" || { s=$?; case "${STEERY_STARTED:-}" in "$PPID":turn*) echo "Steery's hook failed (exit $s) in a turn Steery started, so the call is blocked" >&2; exit 2;; esac; exit 0; }"#;

/// One command hook per event, running `hook` with the event added.
fn hooked(hook: &str) -> Value {
    let events: Map<String, Value> = hooks::EVENTS
        .iter()
        .map(|event| {
            let fails = if event.gates() { GUARD } else { " || true" };
            let entry = json!([{
                "type": "command",
                "bash": format!("{hook} {}{fails}", event.name),
                "timeoutSec": event.waits,
            }]);
            (event.name.to_owned(), entry)
        })
        .collect();
    Value::Object(events)
}

/// A release as `major.minor.patch`.
fn named(release: Option<(u64, u64, u64)>) -> Option<String> {
    release.map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
}
