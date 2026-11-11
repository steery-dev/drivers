//! `models`: what `agy models` lists, in its order, and whether Antigravity is signed in.
//!
//! Antigravity CLI 1.5.5 asks its service for the list, so it is the account's own. Signed
//! in, it prints one model on each line, its id then its label, and exits 0:
//!
//! ```text
//! gemini-3.1-pro-high                Gemini 3.1 Pro (High)
//! ```
//!
//! Signed out it prints nothing and exits 1, its standard error ending `Error: Please sign
//! in to view available models.`; with `modelProvider` `gemini` in its settings and no
//! `GEMINI_API_KEY`, it says that the variable is not set. Either is signed out. A Google
//! sign-in lives in the system keyring, which no driver reads, so the command is what tells.

use std::path::Path;

use steery_driver_common::program::{PROBE, helper};
use steery_sdk::wire::harness::{Model, Models};
use steery_sdk::{Error, refusal};

/// What `agy models` says where there is no sign-in, and where the key its settings ask for
/// is not in the environment.
const SIGNED_OUT: [&str; 2] = ["Please sign in", "environment variable is not set"];

/// `models` for Antigravity in the person's home `home`: `no-binary` where `agy` is not
/// here, refused with why where `agy models` gave no list and no sign-in answer.
pub async fn list(home: &Path, root: &Path) -> Result<Models, Error> {
    let binary = super::binary(home)
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "agy is not installed here"))?;
    let watch = vec![root.join("settings.json")];
    match printed(&binary, home)
        .await
        .map_err(|why| Error::refused(refusal::REFUSED, why))?
    {
        Printed::Listed(out) => Ok(Models {
            models: models(&out),
            unlisted: None,
            watch,
            signed_in: Some(true),
        }),
        Printed::SignedOut => Ok(Models {
            models: Vec::new(),
            unlisted: None,
            watch,
            signed_in: Some(false),
        }),
    }
}

enum Printed {
    Listed(String),
    SignedOut,
}

/// `agy models` in `home`, within [`PROBE`].
async fn printed(binary: &Path, home: &Path) -> Result<Printed, String> {
    let mut command = helper(binary);
    command
        .arg("models")
        .env("HOME", home)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = tokio::time::timeout(PROBE, command.output())
        .await
        .map_err(|_| format!("agy models did not answer within {} s", PROBE.as_secs()))?
        .map_err(|e| format!("agy models did not start: {e}"))?;
    if output.status.success() {
        return String::from_utf8(output.stdout)
            .map(Printed::Listed)
            .map_err(|_| "agy models printed no text".to_owned());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    if SIGNED_OUT
        .iter()
        .any(|signed_out| said.contains(signed_out))
    {
        return Ok(Printed::SignedOut);
    }
    let last = said
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    Err(format!(
        "agy models exited with {}: {}",
        output.status,
        last.trim()
    ))
}

/// Each printed line's id and the label after it.
fn models(out: &str) -> Vec<Model> {
    out.lines()
        .filter_map(|line| {
            let line = line.trim();
            let (id, label) = line
                .split_once(char::is_whitespace)
                .map_or((line, ""), |(id, label)| (id, label.trim()));
            (!id.is_empty()).then(|| Model {
                provider: None,
                model: id.to_owned(),
                label: (!label.is_empty()).then(|| label.to_owned()),
                thinking: Vec::new(),
                cost: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `agy models` printed in the recorded `models1` scene, against the corpus's mock.
    #[test]
    fn reads_each_line_as_a_model() {
        let out = "gemini-4-argon-high                Gemini 4 Argon (High)\n\
                   gemini-3.1-pro-low                 Gemini 3.1 Pro (Low)\n";
        let listed = models(out);
        let read: Vec<(&str, Option<&str>)> = listed
            .iter()
            .map(|model| (model.model.as_str(), model.label.as_deref()))
            .collect();
        assert_eq!(
            read,
            [
                ("gemini-4-argon-high", Some("Gemini 4 Argon (High)")),
                ("gemini-3.1-pro-low", Some("Gemini 3.1 Pro (Low)")),
            ]
        );
    }
}
