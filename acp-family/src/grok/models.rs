//! `models`: what `grok models` lists, the default first, and whether Grok says it is
//! signed in.
//!
//! Grok 1.0.77 prints, exit 0 either way, a first line that says its sign-in, then the list:
//!
//! ```text
//! You are logged in with grok.com.        (or: You are not authenticated.)
//! Default model: grok-4.7                 (or: You are using XAI_API_KEY.)
//! Available models:
//!   * grok-4.7 (default)
//!   - grok-4.7-build-fast
//! ```

use std::path::{Path, PathBuf};

use steery_driver_common::program::{PROBE, helper};
use steery_sdk::wire::harness::{Model, Models};
use steery_sdk::{Error, refusal};

/// The first line Grok prints when it has its own sign-in.
const LOGGED_IN: &str = "You are logged in";
/// The first line Grok prints when it has an `XAI_API_KEY`.
const API_KEY: &str = "You are using XAI_API_KEY";
/// The first line Grok prints when it has neither.
const SIGNED_OUT: &str = "You are not authenticated";

/// What `grok models` printed: on standard output, and its warnings on standard error.
pub(super) struct Printed {
    pub out: String,
    pub warned: String,
}

/// `grok models` in Grok's home `root`, with `env` added (`RUST_LOG` is `off` unless it
/// names another), within [`PROBE`]: what it printed, or why it gave nothing.
pub(super) async fn printed(
    binary: &Path,
    root: &Path,
    env: &[(&str, &str)],
) -> Result<Printed, String> {
    let mut command = helper(binary);
    command
        .arg("models")
        .env("GROK_HOME", root)
        .env("RUST_LOG", "off")
        .envs(env.iter().copied())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let output = tokio::time::timeout(PROBE, command.output())
        .await
        .map_err(|_| format!("grok models did not answer within {} s", PROBE.as_secs()))?
        .map_err(|e| format!("grok models did not start: {e}"))?;
    if !output.status.success() {
        return Err(format!("grok models exited with {}", output.status));
    }
    Ok(Printed {
        out: String::from_utf8_lossy(&output.stdout).into_owned(),
        warned: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

impl Printed {
    /// Whether Grok says it is signed in: its own sign-in or an API key. None where its
    /// first line is neither that nor signed out.
    pub fn signed_in(&self) -> Option<bool> {
        let first = self.out.lines().map(str::trim).find(|line| !line.is_empty())?;
        if first.starts_with(LOGGED_IN) || first.starts_with(API_KEY) {
            Some(true)
        } else if first.starts_with(SIGNED_OUT) {
            Some(false)
        } else {
            None
        }
    }

    /// The listed models, the one marked `*` (the default) first.
    fn models(&self) -> Vec<Model> {
        let mut models = Vec::new();
        for line in self.out.lines().map(str::trim) {
            let (default, rest) = match line.split_at_checked(2) {
                Some(("* ", rest)) => (true, rest),
                Some(("- ", rest)) => (false, rest),
                _ => continue,
            };
            let id = rest.trim_end_matches("(default)").trim();
            if id.is_empty() {
                continue;
            }
            let model = Model {
                provider: None,
                model: id.to_owned(),
                label: None,
                thinking: Vec::new(),
                cost: None,
            };
            if default {
                models.insert(0, model);
            } else {
                models.push(model);
            }
        }
        models
    }
}

/// `models` for Grok in its home `root`: `no-binary` where Grok is not here, refused with
/// why where `grok models` gave no list.
pub async fn list(root: &Path) -> Result<Models, Error> {
    let binary = super::binary(root)
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "grok is not installed here"))?;
    let printed = printed(&binary, root, &[])
        .await
        .map_err(|why| Error::refused(refusal::REFUSED, why))?;
    Ok(Models {
        models: printed.models(),
        unlisted: None,
        watch: watched(root),
        signed_in: printed.signed_in(),
    })
}

/// The files whose change changes the list: Grok's sign-in and its configuration.
fn watched(root: &Path) -> Vec<PathBuf> {
    vec![root.join("auth.json"), root.join("config.toml")]
}
