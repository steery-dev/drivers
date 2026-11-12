//! `models`: what `droid exec --help` lists. Droid 1.9.0 has no models command; its help
//! prints the models it runs here, Factory's and the person's custom ones from
//! `~/.factory/settings.json`, and each of Factory's thinking levels:
//!
//! ```text
//! Available Models:
//!   gpt-6.1-sol                  GPT-6.1 Sol (default)
//! Custom Models:
//!   custom:Qwen3-local-0         Qwen3 local
//!
//! Model details:
//!   - GPT-6.1 Sol: supports reasoning: Yes; supported: [low, medium, high]; default: medium
//! ```
//!
//! A custom model has no detail line, so it lists no thinking levels.

use std::collections::HashMap;
use std::path::Path;

use steery_driver_common::program::{PROBE, helper};
use steery_sdk::wire::harness::{Model, Models};
use steery_sdk::{Error, refusal};

use super::EXECUTABLE;

/// How Droid marks its default model's label.
const DEFAULT: &str = "(default)";

/// `models` for the person at `home`: `no-binary` where Droid is not here, refused with why
/// where its help gave no list.
pub async fn list(home: &Path, root: &Path) -> Result<Models, Error> {
    let binary = super::setup::binary(home)
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "droid is not installed here"))?;
    let help = help(&binary, home)
        .await
        .map_err(|why| Error::refused(refusal::REFUSED, why))?;
    let models = listed(&help);
    if models.is_empty() {
        return Err(Error::refused(
            refusal::REFUSED,
            "`droid exec --help` listed no models",
        ));
    }
    Ok(Models {
        models,
        unlisted: None,
        watch: vec![root.join("settings.json")],
        signed_in: None,
    })
}

/// `droid exec --help` for the person at `home`, within [`PROBE`].
async fn help(binary: &Path, home: &Path) -> Result<String, String> {
    let mut command = helper(binary);
    command
        .args(["exec", "--help"])
        .env("HOME", home)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let output = tokio::time::timeout(PROBE, command.output())
        .await
        .map_err(|_| {
            format!(
                "{EXECUTABLE} exec --help did not answer within {} s",
                PROBE.as_secs()
            )
        })?
        .map_err(|e| format!("{EXECUTABLE} exec --help did not start: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{EXECUTABLE} exec --help exited with {}",
            output.status
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The models of the help's two lists, the default first, each with the levels its detail
/// line names.
fn listed(help: &str) -> Vec<Model> {
    let mut levels: HashMap<&str, Vec<String>> = HashMap::new();
    let mut models = Vec::new();
    let mut section = "";
    for line in help.lines() {
        if !line.starts_with(' ') {
            section = line.trim();
            continue;
        }
        let line = line.trim();
        match section {
            "Available Models:" | "Custom Models:" => {
                let Some((id, label)) = line.split_once(char::is_whitespace) else {
                    continue;
                };
                let label = label.trim();
                let default = label.ends_with(DEFAULT);
                let label = label.trim_end_matches(DEFAULT).trim();
                let model = Model {
                    provider: None,
                    model: id.to_owned(),
                    label: (!label.is_empty()).then(|| label.to_owned()),
                    thinking: Vec::new(),
                    cost: None,
                };
                if default {
                    models.insert(0, model);
                } else {
                    models.push(model);
                }
            }
            "Model details:" => {
                let Some((label, facts)) = line
                    .strip_prefix("- ")
                    .and_then(|line| line.split_once(": supports reasoning:"))
                else {
                    continue;
                };
                let supported = facts
                    .split_once("supported: [")
                    .and_then(|(_, rest)| rest.split_once(']'))
                    .map(|(list, _)| list);
                if let Some(list) = supported {
                    levels.insert(
                        label,
                        list.split(',')
                            .map(str::trim)
                            .filter(|level| !level.is_empty())
                            .map(str::to_owned)
                            .collect(),
                    );
                }
            }
            _ => {}
        }
    }
    for model in &mut models {
        if let Some(found) = model.label.as_deref().and_then(|label| levels.get(label)) {
            model.thinking = found.clone();
        }
    }
    models
}
