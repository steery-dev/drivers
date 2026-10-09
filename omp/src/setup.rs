//! `setup/plan`: what sets omp up for Steery. Four things, each an entry the runtime
//! writes and records: Steery's MCP server in omp's `mcp.json`; Steery's skill; Steery's
//! plugin, which is a loader in omp's `extensions` folder and the copy of Steery's
//! extension it runs, kept in the runtime's config folder and named there.

use std::path::Path;
use std::time::Duration;

use serde_json::json;
use sha2::{Digest, Sha256};
use steery_sdk::wire::harness::{Entry, Format, Found, Planned, Setup};
use steery_sdk::{Error, refusal};

/// Steery's extension, as Steery writes it into omp: the module that dials the runtime's
/// door and says what the conversation is doing. What is this machine's sits in
/// placeholders inside string literals, so the source stays valid as it stands.
const EXTENSION: &str = include_str!("../plugin/extension.ts");
/// The file omp loads: it runs the copy of the extension named in one file of the
/// runtime's, and moves every session onto a newer copy once one is named there.
const LOADER: &str = include_str!("../plugin/loader.ts");
/// The rule every surface draws a request's form by, carried whole into the extension
/// because a copy loads nothing over the network.
const FORM_JS: &str = include_str!("../plugin/form.js");

/// How long `omp --version` has to answer before nothing about the build is claimed.
const PROBE: Duration = Duration::from_secs(5);

pub async fn plan(config: &Path, setup: Setup) -> Result<Planned, Error> {
    let root = crate::root(&setup.home).map_err(|why| Error::refused(refusal::REFUSED, why))?;
    let Some(binary) = crate::binary() else {
        return Ok(Planned {
            found: None,
            outdated: None,
            entries: Vec::new(),
            notes: vec![
                "Not installed here: no executable found, and a configuration directory is not \
                 evidence of one."
                    .into(),
            ],
        });
    };
    let answered = version(&binary).await;
    let release = answered.as_deref().and_then(release_within);
    let mut notes = Vec::new();
    if answered.is_none() {
        notes.push("`--version` did not answer, so nothing about this build is claimed.".into());
    }
    let minimum = release_within(crate::MINIMUM).expect("the minimum is a release");
    // A release older than Steery needs is not set up, and says how to update.
    let outdated = release.filter(|running| *running < minimum).map(|running| {
        format!(
            "omp {}.{}.{} is older than Steery needs ({}): run `{}`, then `steery setup`.",
            running.0,
            running.1,
            running.2,
            crate::MINIMUM,
            crate::UPDATE
        )
    });
    let found = Found {
        binary,
        release: release
            .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"))
            .unwrap_or_default(),
        root: root.clone(),
    };
    if let Some(why) = &outdated {
        notes.push(why.clone());
        return Ok(Planned {
            found: Some(found),
            outdated,
            entries: Vec::new(),
            notes,
        });
    }
    let named = config.join("extension").join("omp");
    let extension = extension(&setup)?;
    let digest = Sha256::digest(extension.as_bytes());
    let digest: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let copy = config
        .join("extension")
        .join(format!("omp-{}.ts", &digest[..16]));
    let entries = vec![
        // omp documents `timeout: 0` as turning its client's MCP timeout off, which a call
        // that waits for a person needs.
        Entry {
            path: root.join("mcp.json"),
            format: Format::Json,
            at: Some("/mcpServers/steery".into()),
            value: json!({ "type": "http", "url": setup.mcp_url, "timeout": 0 }),
        },
        Entry {
            path: root.join("skills/steery/SKILL.md"),
            format: Format::Text,
            at: None,
            value: json!(setup.skill),
        },
        Entry {
            path: root.join("extensions/steery.ts"),
            format: Format::Text,
            at: None,
            value: json!(LOADER.replace("__STEERY_CURRENT__", literal(&named)?)),
        },
        Entry {
            path: named.clone(),
            format: Format::Text,
            at: None,
            value: json!(format!("{}\n", literal(&copy)?)),
        },
        Entry {
            path: copy,
            format: Format::Text,
            at: None,
            value: json!(extension),
        },
    ];
    Ok(Planned {
        found: Some(found),
        outdated: None,
        entries,
        notes,
    })
}

/// Steery's extension for omp, its placeholders filled: the harness; the runtime's
/// release, the only one its door accepts; the runtime's loopback address, where the card
/// hands a person's answer to `/answer`, which is the MCP URL without its `/mcp`; the
/// door's address; and the MCP entry the extension registers, none in omp, which reads
/// Steery's from its own `mcp.json`.
fn extension(setup: &Setup) -> Result<String, Error> {
    let runtime = setup.mcp_url.strip_suffix("/mcp").ok_or_else(|| {
        Error::invalid(format!(
            "the MCP URL does not end in /mcp: {}",
            setup.mcp_url
        ))
    })?;
    for (what, value) in [
        ("release", setup.release.as_str()),
        ("MCP URL", runtime),
        ("door", setup.door.as_str()),
    ] {
        if value.is_empty() || value.contains(['"', '\\', '\n', '\r']) {
            return Err(Error::invalid(format!(
                "this {what} cannot be written into a string literal: {value}"
            )));
        }
    }
    Ok(EXTENSION
        .replace("__STEERY_HARNESS__", "omp")
        .replace("__STEERY_RELEASE__", &setup.release)
        .replace("__STEERY_RUNTIME__", runtime)
        .replace("__STEERY_DOOR__", &setup.door)
        .replace("__STEERY_MCP__", "{}")
        .replace("/*__STEERY_FORM_JS__*/", FORM_JS))
}

/// A path as it goes into a string literal of the plugin.
fn literal(path: &Path) -> Result<&str, Error> {
    path.to_str()
        .filter(|path| !path.contains(['"', '\\', '\n', '\r']))
        .ok_or_else(|| {
            Error::invalid(format!(
                "this path cannot be named in omp's plugin: {}",
                path.display()
            ))
        })
}

/// The first line `omp --version` prints, or none where it did not answer in time.
async fn version(binary: &Path) -> Option<String> {
    let output = tokio::time::timeout(
        PROBE,
        crate::helper(binary)
            .arg("--version")
            .stdout(std::process::Stdio::piped())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// The first `major.minor.patch` a line names: `omp/18.4.12` is 18.4.12, a build's `+…`
/// aside.
fn release_within(text: &str) -> Option<(u64, u64, u64)> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|word| {
            let mut parts = word.trim_matches('.').split('.');
            let release = (
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
                parts.next()?.parse().ok()?,
            );
            parts.next().is_none().then_some(release)
        })
}
