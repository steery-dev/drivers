//! `setup/plan`: what sets the harness up for Steery, each an entry the runtime writes and
//! records: Steery's skill; Steery's plugin, which is a loader in the harness's
//! `extensions` folder and the copy of Steery's extension it runs, kept in the runtime's
//! config folder and named there; and Steery's MCP server. omp reads that from its
//! `mcp.json`; pi lets an extension register a server for each conversation, so in pi the
//! plugin carries it, with headers naming the conversation. Neither runs hook commands:
//! the plugin says the moments over the door.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use steery_driver_common::program::{release_within, version};
use steery_sdk::Error;
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

use crate::Harness;

/// Steery's extension, as Steery writes it into the harness: the module that dials the
/// runtime's door and says what the conversation is doing. What is this machine's sits in
/// placeholders inside string literals, so the source stays valid as it stands.
const EXTENSION: &str = include_str!("../plugin/extension.ts");
/// The file the harness loads: it runs the copy of the extension named in one file of the
/// runtime's, and moves every session onto a newer copy once one is named there.
const LOADER: &str = include_str!("../plugin/loader.ts");

/// Steery's MCP server, as the entries name it.
const SERVER: &str = "steery";

/// The plan for this machine. Whether the release found is one Steery speaks to is the
/// runtime's to say, from `initialize`'s minimum.
pub async fn plan(
    harness: Harness,
    home: &Path,
    config: &Path,
    setup: Setup,
) -> Result<Planned, Error> {
    let refused = |why: String| Planned {
        found: None,
        refused: Some(why),
        entries: Vec::new(),
        notes: Vec::new(),
        install: None,
    };
    let root = match harness.root(home) {
        Ok(root) => root,
        Err(why) => return Ok(refused(why)),
    };
    let Some(binary) = harness.binary() else {
        return Ok(Planned {
            found: None,
            refused: None,
            entries: Vec::new(),
            notes: vec![
                "Not installed here: no executable found, and a configuration directory is not \
                 evidence of one."
                    .into(),
            ],
            install: harness.install().await,
        });
    };
    // omp and pi read the same variable for their folder, and one folder holds one skill
    // and one plugin: where both are installed there, neither is set up.
    let other = harness.other();
    if other.binary().is_some() && other.root(home).as_ref() == Ok(&root) {
        return Ok(refused(format!(
            "pi and omp are both installed here and both keep their configuration in {} — the \
             skill Steery writes is one file, and it cannot belong to both. Unset the variable \
             that moved them together, or give each one its own directory, and run setup again.",
            root.display()
        )));
    }
    let release = version(&binary).await.as_deref().and_then(release_within);
    let mut notes = Vec::new();
    if release.is_none() {
        notes.push("`--version` did not answer, so nothing about this build is claimed.".into());
    }
    let found = Found {
        binary,
        release: release
            .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
            .unwrap_or_default(),
        root: root.clone(),
    };
    let named = config.join("extension").join(harness.id());
    let extension = extension(harness, &setup)?;
    let digest = Sha256::digest(extension.as_bytes());
    let digest: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let copy = config
        .join("extension")
        .join(format!("{}-{}.ts", harness.id(), &digest[..16]));
    let mut entries = Vec::new();
    if harness == Harness::Omp {
        // omp documents `timeout: 0` as turning its client's MCP timeout off; a process-wide
        // `OMP_MCP_TIMEOUT_MS` still takes precedence, and Steery sets that for nobody.
        entries.push(Entry {
            path: root.join("mcp.json"),
            format: Format::Json,
            at: Some(format!("/mcpServers/{SERVER}")),
            value: json!({ "type": "http", "url": setup.mcp_url, "timeout": 0 }),
            carries: vec![Part::Mcp],
        });
    }
    entries.extend([
        Entry {
            path: root.join("skills/steery/SKILL.md"),
            format: Format::Text,
            at: None,
            value: json!(setup.skill),
            carries: vec![Part::Skill],
        },
        Entry {
            path: root.join("extensions/steery.ts"),
            format: Format::Text,
            at: None,
            value: json!(LOADER.replace("__STEERY_CURRENT__", literal(&named)?)),
            carries: vec![Part::Plugin],
        },
        Entry {
            path: named.clone(),
            format: Format::Text,
            at: None,
            value: json!(format!("{}\n", literal(&copy)?)),
            carries: vec![Part::Plugin],
        },
        Entry {
            path: copy,
            format: Format::Text,
            at: None,
            value: json!(extension),
            carries: match harness {
                Harness::Omp => vec![Part::Plugin],
                Harness::Pi => vec![Part::Plugin, Part::Mcp],
            },
        },
    ]);
    notes.push(format!(
        "It gets {} {} decides for itself when to ask and when to ask for the next \
         instruction; nothing Steery writes stands in front of it.",
        match harness {
            Harness::Omp => "the MCP entry, the skill and Steery's extension.",
            Harness::Pi => "the skill and Steery's extension, which carries its MCP entry.",
        },
        harness.id()
    ));
    notes.push(
        "Steery's extension says what each of its conversations is doing, and takes your \
         words into one that is open. Whatever fails in it leaves the harness as it would be \
         with no Steery."
            .into(),
    );
    if harness == Harness::Pi {
        notes.extend(own_servers(&root).into_iter().map(|file| {
            format!(
                "{} has its own `{SERVER}` server; remove it to use Steery's extension.",
                file.display()
            )
        }));
    }
    Ok(Planned {
        found: Some(found),
        refused: None,
        entries,
        notes,
        install: None,
    })
}

/// Steery's MCP entry as pi's plugin registers it. pi bounds one request at `timeout`
/// seconds, 60 unless the entry says otherwise, and refuses anything but a positive
/// number. Its default exposure, `codemode`, declares a server's tools to the model only
/// inside scripts and keeps the server's instructions out of every prompt; `direct`
/// declares them as tools, the way every other harness holds them.
fn pi_entry(setup: &Setup) -> Value {
    json!({
        "url": setup.mcp_url,
        "type": "http",
        "timeout": setup.timeout,
        "exposure": "direct",
    })
}

/// Steery's extension for the harness, its placeholders filled: the harness; the door's
/// address; and the MCP entry the extension registers, none in omp, which reads Steery's
/// from its own `mcp.json`.
fn extension(harness: Harness, setup: &Setup) -> Result<String, Error> {
    let door = setup.door.as_str();
    if door.is_empty() || door.contains(['"', '\\', '\n', '\r']) {
        return Err(Error::invalid(format!(
            "this door cannot be written into a string literal: {door}"
        )));
    }
    let mcp = match harness {
        Harness::Omp => "{}".to_owned(),
        Harness::Pi => pi_entry(setup).to_string(),
    };
    // Written into a "…" literal: the JSON's own quotes and escapes are escaped once more.
    let mcp = mcp.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(EXTENSION
        .replace("__STEERY_HARNESS__", harness.id())
        .replace("__STEERY_DOOR__", door)
        .replace("__STEERY_MCP__", &mcp))
}

/// The `mcp.json` files pi reads that hold a `steery` server of a person's own, which
/// would stand beside the one the plugin registers: pi's own, and each trusted project's
/// `.pi/mcp.json`. pi reads `-` and `_` in a server's name alike.
fn own_servers(root: &Path) -> Vec<PathBuf> {
    let json = |file: &Path| -> Option<Value> {
        let bytes = std::fs::read(file).ok()?;
        serde_json::from_slice(bytes.strip_prefix("\u{feff}".as_bytes()).unwrap_or(&bytes)).ok()
    };
    let mut files = vec![root.join("mcp.json")];
    if let Some(Value::Object(decisions)) = json(&root.join("trust.json")) {
        files.extend(
            decisions
                .iter()
                .filter(|(folder, trusted)| {
                    trusted.as_bool() == Some(true) && Path::new(folder).is_absolute()
                })
                .map(|(folder, _)| Path::new(folder).join(".pi/mcp.json")),
        );
    }
    files
        .into_iter()
        .filter(|file| {
            json(file)
                .and_then(|config| match config.get("mcpServers") {
                    Some(Value::Object(servers)) => {
                        Some(servers.keys().any(|name| name.replace('-', "_") == SERVER))
                    }
                    _ => None,
                })
                .unwrap_or(false)
        })
        .collect()
}

/// A path as it goes into a string literal of the plugin.
fn literal(path: &Path) -> Result<&str, Error> {
    path.to_str()
        .filter(|path| !path.contains(['"', '\\', '\n', '\r']))
        .ok_or_else(|| {
            Error::invalid(format!(
                "this path cannot be named in the plugin: {}",
                path.display()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variables omp and pi read for their folders, and `PATH`, are the process's: one
    /// test at a time changes them.
    static ENVIRONMENT: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const MOVED: [&str; 4] = [
        "PI_CODING_AGENT_DIR",
        "PI_CONFIG_DIR",
        "OMP_PROFILE",
        "PI_PROFILE",
    ];

    fn given() -> Setup {
        Setup {
            mcp_url: "http://127.0.0.1:7777/mcp".into(),
            skill: "---\nname: steery\n---\n".into(),
            hook: "/usr/local/bin/steery hook omp".into(),
            door: "ws://127.0.0.1:7777/door".into(),
            release: "1.0.0".into(),
            timeout: 120,
        }
    }

    #[test]
    fn each_harness_has_one_root_and_never_an_invented_one() {
        let _held = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
        let home = Path::new("/home/person");
        for name in MOVED {
            // SAFETY: the environment is changed only under `ENVIRONMENT`.
            unsafe { std::env::remove_var(name) };
        }
        for harness in [Harness::Omp, Harness::Pi] {
            assert_eq!(
                harness.root(home).unwrap(),
                harness.default_root(home),
                "{}",
                harness.id()
            );
        }
        // omp forked from pi and took its own folder: one harness's conversations are never
        // read or configured as the other's.
        assert_eq!(Harness::Pi.root(home).unwrap(), home.join(".pi/agent"));
        assert_eq!(Harness::Omp.root(home).unwrap(), home.join(".omp/agent"));
        // SAFETY: as above.
        unsafe { std::env::set_var("OMP_PROFILE", "work") };
        assert_eq!(
            Harness::Omp.root(home).unwrap(),
            home.join(".omp/profiles/work/agent")
        );
        unsafe { std::env::set_var("PI_CODING_AGENT_DIR", "relative/agent") };
        assert!(
            Harness::Omp.root(home).is_err(),
            "a relative folder is no root"
        );
        for name in MOVED {
            unsafe { std::env::remove_var(name) };
        }
    }

    /// pi and omp read the same variables, so a machine that moves one moves both, and the
    /// skill Steery writes is one file that cannot belong to two harnesses. Neither is set
    /// up on the guess that the folder is really the other's.
    #[test]
    fn two_harnesses_in_one_directory_are_both_refused() {
        let _held = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("pi-family-shared-{}", std::process::id()));
        let bin = dir.join("bin");
        let shared = dir.join("agent");
        std::fs::create_dir_all(&bin).unwrap();
        for harness in [Harness::Omp, Harness::Pi] {
            let fake = bin.join(harness.id());
            std::fs::write(&fake, "#!/bin/sh\necho 99.0.0\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        // SAFETY: the environment is changed only under `ENVIRONMENT`.
        unsafe {
            std::env::set_var("PATH", &bin);
            std::env::set_var("PI_CODING_AGENT_DIR", &shared);
        }
        for harness in [Harness::Omp, Harness::Pi] {
            let planned = runtime
                .block_on(plan(harness, &dir, &dir.join("config"), given()))
                .unwrap();
            let why = planned.refused.expect("refused with a reason");
            assert!(why.contains(&shared.display().to_string()), "{why}");
            assert!(planned.entries.is_empty(), "nothing is written for either");
        }
        // Only one installed: the folder is that one's alone.
        std::fs::remove_file(bin.join("pi")).unwrap();
        let planned = runtime
            .block_on(plan(Harness::Omp, &dir, &dir.join("config"), given()))
            .unwrap();
        assert_eq!(planned.refused, None);
        assert!(!planned.entries.is_empty());
        unsafe {
            std::env::set_var("PATH", path);
            std::env::remove_var("PI_CODING_AGENT_DIR");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// omp's installer writes `~/.local/bin/omp`, which a shell that has not read its
    /// profile since lacks on `PATH`: the plan finds it there, by its absolute path, and
    /// where it is not there either, offers omp's own installer.
    #[test]
    fn omp_in_its_installers_folder_off_path_is_found() {
        let _held = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let home = std::env::temp_dir().join(format!("pi-family-installed-{}", std::process::id()));
        let tools = home.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        use std::os::unix::fs::PermissionsExt;
        for tool in ["sh", "curl"] {
            let real = ["/usr/bin", "/bin"]
                .iter()
                .map(|dir| Path::new(dir).join(tool))
                .find(|candidate| candidate.exists())
                .expect("sh and curl on this machine");
            std::os::unix::fs::symlink(real, tools.join(tool)).unwrap();
        }
        let (path, home_was) = (std::env::var_os("PATH"), std::env::var_os("HOME"));
        // SAFETY: the environment is changed only under `ENVIRONMENT`.
        unsafe {
            std::env::set_var("PATH", &tools);
            std::env::set_var("HOME", &home);
            std::env::remove_var("PI_INSTALL_DIR");
        }
        let missing = runtime
            .block_on(plan(Harness::Omp, &home, &home.join("config"), given()))
            .unwrap();
        assert!(missing.found.is_none());
        let install = missing.install.expect("omp's installer is offered");
        assert_eq!(install.argv[..2], ["sh".to_owned(), "-c".to_owned()]);
        assert!(
            install.about.contains(&install.argv[2]),
            "{}",
            install.about
        );
        let omp = home.join(".local/bin/omp");
        std::fs::create_dir_all(omp.parent().unwrap()).unwrap();
        std::fs::write(&omp, "#!/bin/sh\necho 99.0.0\n").unwrap();
        std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
        let found = runtime
            .block_on(plan(Harness::Omp, &home, &home.join("config"), given()))
            .unwrap();
        assert_eq!(found.found.map(|found| found.binary), Some(omp));
        assert_eq!(found.install, None);
        unsafe {
            for (name, was) in [("PATH", path), ("HOME", home_was)] {
                match was {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        std::fs::remove_dir_all(&home).unwrap();
    }

    /// pi's installer is offered only where `node` names a release new enough; one whose
    /// `--version` hangs (a broken shim, a version manager waiting on input) is given up
    /// on after the probe's time and killed, and never holds the driver meanwhile.
    #[test]
    fn pi_is_offered_only_where_node_answers_new_enough() {
        let _held = ENVIRONMENT.lock().unwrap_or_else(|e| e.into_inner());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let home = std::env::temp_dir().join(format!("pi-family-node-{}", std::process::id()));
        let tools = home.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        use std::os::unix::fs::PermissionsExt;
        for tool in ["sh", "curl"] {
            let real = ["/usr/bin", "/bin"]
                .iter()
                .map(|dir| Path::new(dir).join(tool))
                .find(|candidate| candidate.exists())
                .expect("sh and curl on this machine");
            std::os::unix::fs::symlink(real, tools.join(tool)).unwrap();
        }
        // Written whole before it is in place, so none is run half written.
        let script = |name: &str, body: &str| {
            let written = tools.join(format!("{name}.tmp"));
            std::fs::write(&written, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&written, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&written, tools.join(name)).unwrap();
        };
        script("npm", "exit 0");
        let (path, home_was) = (std::env::var_os("PATH"), std::env::var_os("HOME"));
        // SAFETY: the environment is changed only under `ENVIRONMENT`.
        unsafe {
            std::env::set_var("PATH", &tools);
            std::env::set_var("HOME", &home);
        }
        let offered = || {
            runtime
                .block_on(plan(Harness::Pi, &home, &home.join("config"), given()))
                .unwrap()
                .install
                .is_some()
        };
        script("node", "echo v22.19.0");
        assert!(offered(), "new enough");
        script("node", "echo v20.11.1");
        assert!(!offered(), "too old");
        let sleep = ["/usr/bin/sleep", "/bin/sleep"]
            .into_iter()
            .find(|sleep| Path::new(sleep).exists())
            .expect("sleep on this machine");
        script("node", &format!("exec {sleep} 60"));
        let began = std::time::Instant::now();
        let (planned, ticked) = runtime.block_on(async {
            let ticked = async {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                std::time::Instant::now()
            };
            let planned = async {
                let planned = plan(Harness::Pi, &home, &home.join("config"), given()).await;
                (planned, std::time::Instant::now())
            };
            let ((planned, at), ticked) = tokio::join!(planned, ticked);
            (planned.map(|planned| (planned, at)), ticked)
        });
        let (planned, at) = planned.unwrap();
        assert_eq!(planned.install, None, "a node that hangs names no release");
        assert!(ticked < at, "the driver went on meanwhile");
        let took = began.elapsed();
        assert!(
            took < steery_driver_common::program::PROBE + std::time::Duration::from_secs(3),
            "{took:?}"
        );
        unsafe {
            for (name, was) in [("PATH", path), ("HOME", home_was)] {
                match was {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        std::fs::remove_dir_all(&home).unwrap();
    }
}
