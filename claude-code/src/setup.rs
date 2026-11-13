//! `setup/plan`: what sets Claude Code up for Steery. Steery's MCP server among Claude
//! Code's user-scope servers; Steery's skill in its skills folder; and one hook per event
//! Steery reads ([`crate::hooks::EVENTS`]) in its `settings.json`, each added beside the
//! person's own hooks on that event. Where `needs` (protocol 5) and Claude Code is not
//! signed in, its sign-in ([`crate::sign_in::need`]).

use std::path::Path;

use serde_json::json;
use steery_driver_common::program::{release_within, version};
use steery_driver_common::signed::Known;
use steery_sdk::Error;
use steery_sdk::wire::harness::{Entry, Format, Found, Need, Part, Planned, Setup};

/// The plan for this machine; with its needs where the protocol picked has them, and the
/// sign-in again where the person asked and the protocol picked is 15 or newer.
pub async fn plan(home: &Path, root: &Path, setup: Setup, protocol: u32) -> Result<Planned, Error> {
    let Some(binary) = crate::binary() else {
        let (install, refused) = match crate::install() {
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
    let needs = match protocol >= steery_driver_common::program::NEEDS {
        true => asked(
            &SIGNED,
            crate::drive::signed_in(true).await,
            asking(protocol, &setup),
        ),
        false => Vec::new(),
    };
    let mut notes = Vec::new();
    if release.is_none() {
        notes.push(
            "`--version` did not answer, so the driver says nothing about this build.".into(),
        );
    }
    notes.extend(crate::others_note(&binary));
    Ok(Planned {
        found: Some(Found {
            binary,
            release: release.unwrap_or_default(),
            root: root.to_owned(),
        }),
        refused: None,
        entries: entries(home, root, &setup),
        notes,
        install: None,
        needs,
    })
}

/// Every entry, in the order Claude Code reads them.
fn entries(home: &Path, root: &Path, setup: &Setup) -> Vec<Entry> {
    let state = crate::state_file(home, root);
    let mut entries = vec![
        // A per-server `timeout`, in milliseconds, is at once the bound on one call and the
        // floor of Claude Code's idle watchdog on an `http` server. Claude Code reads no
        // per-conversation headers from this file.
        Entry {
            path: state.clone(),
            format: Format::Json,
            at: Some(format!("/mcpServers/{}", crate::STEERY)),
            value: json!({
                "type": "http",
                "url": setup.mcp_url,
                "timeout": u64::from(setup.timeout) * 1_000,
            }),
            carries: vec![Part::Mcp],
        },
        Entry {
            path: root.join("skills").join(crate::STEERY).join("SKILL.md"),
            format: Format::Text,
            at: None,
            value: json!(setup.skill),
            carries: vec![Part::Skill],
        },
    ];
    // One group per event, matching every tool, beside whatever the person hooked there.
    // Claude Code hands the command to a shell, and reads a non-zero exit as a hook that
    // failed — some events by refusing the call about to run — so a hook whose command
    // cannot run (a machine mid-upgrade, a filled disk) says nothing instead.
    let settings = root.join("settings.json");
    entries.extend(crate::hooks::EVENTS.iter().map(|event| Entry {
        path: settings.clone(),
        format: Format::Json,
        at: Some(format!("/hooks/{}/-", event.name)),
        value: json!({
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": format!("{} {} || true", setup.hook, event.name),
                "timeout": event.waits,
            }],
        }),
        carries: vec![Part::Hooks],
    }));
    // Claude Code starts a background session only in a folder its person trusted, or one
    // under it: on a host made for its conversations nobody opens a terminal to answer
    // that, so the folders the runtime names are trusted as their person would.
    entries.extend(setup.trusts.iter().filter_map(|folder| {
        let key = folder.to_str()?.replace('~', "~0").replace('/', "~1");
        Some(Entry {
            path: state.clone(),
            format: Format::Json,
            at: Some(format!("/projects/{key}/hasTrustDialogAccepted")),
            value: json!(true),
            carries: Vec::new(),
        })
    }));
    entries
}

/// The last answer of the sign-in probe in this process. A probe with no answer uses it.
static SIGNED: Known = Known::new();

/// The needs of the plan, with the probe's answer `now`: the sign-in where the steady answer
/// says Claude Code is not signed in; and, where the person `ask`ed to give it again, the
/// sign-in as a change, though Claude Code is signed in (or its state is not known).
fn asked(known: &Known, now: Option<bool>, ask: bool) -> Vec<Need> {
    match (known.steady(now) == Some(false), ask) {
        (true, _) => vec![crate::sign_in::need()],
        (false, true) => vec![crate::sign_in::change()],
        (false, false) => Vec::new(),
    }
}

/// A release as `major.minor.patch`.
fn named(release: Option<(u64, u64, u64)>) -> Option<String> {
    release.map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
}

/// Whether the person asked to give the sign-in again: the protocol picked is 15 or newer
/// and `setup.ask` names it.
fn asking(protocol: u32, setup: &Setup) -> bool {
    steery_driver_common::program::asks(protocol, &setup.ask, crate::sign_in::NEED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Setup {
        Setup {
            mcp_url: "http://127.0.0.1:7777/mcp".into(),
            skill: "---\nname: steery\n---\n".into(),
            hook: "/usr/local/bin/steery hook claude-code".into(),
            door: "ws://127.0.0.1:7777/door".into(),
            release: "1.0.0".into(),
            timeout: 120,
            trusts: Vec::new(),
            ask: Vec::new(),
        }
    }

    #[test]
    fn a_probe_with_no_answer_keeps_the_last_answer_for_the_need() {
        let known = Known::new();
        let planned = [Some(false), None, None, Some(true), None]
            .map(|probe| asked(&known, probe, false).len());
        assert_eq!(planned, [1, 1, 1, 0, 0]);
    }

    #[test]
    fn a_probe_that_never_answered_plans_no_need() {
        assert!(asked(&Known::new(), None, false).is_empty());
    }

    /// A person who asked plans the sign-in again, with the title of a change; a person who
    /// did not ask plans none while Claude Code is signed in.
    #[test]
    fn a_signed_in_claude_code_plans_the_sign_in_only_where_asked() {
        let asking = |protocol, ask: &[&str]| {
            let setup = Setup {
                ask: ask.iter().map(|id| (*id).to_owned()).collect(),
                ..setup()
            };
            asking(protocol, &setup)
        };
        let planned = |ask: bool| asked(&Known::new(), Some(true), ask);
        let plan = planned(asking(15, &["sign-in"]));
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].id, "sign-in");
        assert_eq!(plan[0].title, "Change how Claude Code signs in?");
        assert_eq!(
            plan[0].line,
            "Claude Code is signed in on this host. Pick a new way to sign in."
        );
        assert!(planned(asking(15, &[])).is_empty());
        assert!(planned(asking(15, &["other"])).is_empty());
        // Protocol 14 ignores `ask`.
        assert!(planned(asking(14, &["sign-in"])).is_empty());
    }

    /// Where Claude Code is not signed in, the need is the one that lacks, asked or not.
    #[test]
    fn a_claude_code_not_signed_in_plans_the_lacking_need_also_where_asked() {
        for ask in [false, true] {
            let plan = asked(&Known::new(), Some(false), ask);
            assert_eq!(plan.len(), 1);
            assert_eq!(plan[0].title, "Sign Claude Code in?");
        }
    }

    #[test]
    fn a_version_is_the_first_release_the_probe_names() {
        assert_eq!(
            named(release_within("2.1.0 (Claude Code)")).as_deref(),
            Some("2.1.0")
        );
        assert_eq!(release_within("Claude Code"), None);
    }

    #[test]
    fn claude_code_is_pointed_at_the_local_server_in_its_own_spelling() {
        let home = Path::new("/home/a");
        let entries = entries(home, &home.join(".claude"), &setup());
        let mcp = &entries[0];
        assert_eq!(mcp.path, home.join(".claude.json"));
        assert_eq!(mcp.at.as_deref(), Some("/mcpServers/steery"));
        assert_eq!(
            mcp.value,
            json!({"type": "http", "url": "http://127.0.0.1:7777/mcp", "timeout": 120_000})
        );
        let moved = super::entries(home, Path::new("/srv/claude"), &setup());
        assert_eq!(moved[0].path, Path::new("/srv/claude/.claude.json"));
        assert_eq!(
            moved[1].path,
            Path::new("/srv/claude/skills/steery/SKILL.md")
        );
    }

    /// Every event Steery reads is hooked once, running the command the runtime named
    /// with the event added, within the bound the runtime is told it has, and unable to
    /// fail as far as Claude Code can tell.
    #[test]
    fn what_setup_writes_is_what_the_command_reads() {
        let root = Path::new("/home/a/.claude");
        let entries = entries(Path::new("/home/a"), root, &setup());
        let hooked: Vec<&Entry> = entries
            .iter()
            .filter(|entry| entry.carries == [Part::Hooks])
            .collect();
        let table = crate::hooks::table();
        assert_eq!(hooked.len(), table.len());
        for (entry, hook) in hooked.iter().zip(&table) {
            assert_eq!(entry.path, root.join("settings.json"));
            assert_eq!(entry.at, Some(format!("/hooks/{}/-", hook.event)));
            assert_eq!(entry.value["matcher"], "");
            let command = &entry.value["hooks"][0];
            assert_eq!(
                command["command"],
                format!(
                    "/usr/local/bin/steery hook claude-code {} || true",
                    hook.event
                )
            );
            assert_eq!(command["timeout"], json!(hook.waits));
        }
    }

    /// A folder the runtime names is trusted where Claude Code looks for its person's
    /// answer, and nothing is trusted where it names none: a person's own computer.
    #[test]
    fn only_the_folders_the_runtime_names_are_trusted() {
        let home = Path::new("/home/daytona");
        let root = home.join(".claude");
        let trusted = |setup: &Setup| -> Vec<Entry> {
            entries(home, &root, setup)
                .into_iter()
                .filter(|entry| entry.carries.is_empty())
                .collect()
        };
        assert!(trusted(&setup()).is_empty());
        let made = Setup {
            trusts: vec![home.to_owned()],
            ..setup()
        };
        let trusts = trusted(&made);
        assert_eq!(trusts.len(), 1);
        assert_eq!(trusts[0].path, home.join(".claude.json"));
        assert_eq!(
            trusts[0].at.as_deref(),
            Some("/projects/~1home~1daytona/hasTrustDialogAccepted")
        );
        assert_eq!(trusts[0].value, json!(true));
    }
}
