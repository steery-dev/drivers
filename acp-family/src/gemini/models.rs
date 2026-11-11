//! `models`: what `gemini models list -o json` lists, the default first, and whether Gemini
//! has what its sign-in needs.
//!
//! Gemini CLI 0.67 prints, exit 0 signed in or not, a JSON array:
//!
//! ```json
//! [{"modelId": "auto", "id": "auto", "name": "Auto", "displayName": "Auto",
//!   "description": "Let Gemini CLI decide…", "contextLimits": {"inputTokens": 1048576}}]
//! ```
//!
//! It says nothing of its sign-in, so the driver tells that from what Gemini reads before it
//! signs in (`cli/src/config/auth.ts`, `core/src/core/contentGenerator.ts`): the auth type in
//! `security.auth.selectedType`, else the one the environment picks, and that type's key or
//! credentials. Gemini can keep a key or a Google sign-in in the macOS Keychain, which no
//! driver reads without a prompt: where only the Keychain can tell, the answer is none.

use std::path::Path;

use serde_json::Value;
use steery_driver_common::program::{PROBE, helper};
use steery_sdk::wire::harness::{Model, Models};
use steery_sdk::{Error, refusal};

use super::mcp::{last, settings, system};

/// `models` for Gemini in its root `root`: `no-binary` where Gemini is not here, refused
/// with why where `gemini models list` gave no list.
pub async fn list(root: &Path) -> Result<Models, Error> {
    let binary = super::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "gemini is not installed here"))?;
    let out = printed(&binary, root)
        .await
        .map_err(|why| Error::refused(refusal::REFUSED, why))?;
    let (system, defaults) = system();
    let scopes = [
        settings(&defaults),
        settings(&root.join("settings.json")),
        settings(&system),
    ];
    let scopes: Vec<&Value> = scopes.iter().collect();
    let mut models = models(&out).map_err(|why| Error::refused(refusal::REFUSED, why))?;
    // The model the settings name is Gemini's default; else the first it lists, `auto`.
    if let Some(chosen) = last(&scopes, "/model/name").and_then(Value::as_str)
        && let Some(at) = models.iter().position(|model| model.model == chosen)
    {
        let model = models.remove(at);
        models.insert(0, model);
    }
    let env = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let selected = last(&scopes, "/security/auth/selectedType").and_then(Value::as_str);
    Ok(Models {
        models,
        unlisted: None,
        watch: vec![
            defaults,
            root.join("settings.json"),
            system,
            root.join("oauth_creds.json"),
        ],
        signed_in: signed_in(selected, &env, root.join("oauth_creds.json").is_file()),
    })
}

/// `gemini models list -o json`, its home the parent of `root`, within [`PROBE`]: what it
/// printed, or why it gave nothing.
async fn printed(binary: &Path, root: &Path) -> Result<String, String> {
    let mut command = helper(binary);
    command
        .args(["models", "list", "-o", "json"])
        .stdout(std::process::Stdio::piped());
    if let Some(home) = root.parent() {
        command.env("GEMINI_CLI_HOME", home);
    }
    let output = tokio::time::timeout(PROBE, command.output())
        .await
        .map_err(|_| {
            format!(
                "gemini models list did not answer within {} s",
                PROBE.as_secs()
            )
        })?
        .map_err(|e| format!("gemini models list did not start: {e}"))?;
    if !output.status.success() {
        return Err(format!("gemini models list exited with {}", output.status));
    }
    String::from_utf8(output.stdout).map_err(|_| "gemini models list printed no text".to_owned())
}

/// The models of the printed array, in its order, each by its `modelId` (else `id`) and
/// `displayName` (else `name`).
fn models(out: &str) -> Result<Vec<Model>, String> {
    let listed: Vec<Value> = serde_json::from_str(out.trim())
        .map_err(|e| format!("gemini models list printed no JSON array: {e}"))?;
    Ok(listed
        .iter()
        .filter_map(|entry| {
            let text = |key: &str| {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            };
            Some(Model {
                provider: None,
                model: text("modelId").or_else(|| text("id"))?.to_owned(),
                label: text("displayName")
                    .or_else(|| text("name"))
                    .map(str::to_owned),
                thinking: Vec::new(),
                cost: None,
            })
        })
        .collect())
}

/// Whether Gemini has what its auth type needs: `selected` from the settings, else the type
/// the environment `env` picks; `oauth` says whether `oauth_creds.json` is there.
fn signed_in(
    selected: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
    oauth: bool,
) -> Option<bool> {
    let on = |name: &str| env(name).as_deref() == Some("true");
    let chosen = selected.or_else(|| {
        if on("GOOGLE_GENAI_USE_GCA") {
            Some("oauth-personal")
        } else if on("GOOGLE_GENAI_USE_VERTEXAI") {
            Some("vertex-ai")
        } else if env("GEMINI_API_KEY").is_some() {
            Some("gemini-api-key")
        } else {
            None
        }
    });
    match chosen {
        // Gemini asks which way to sign in before it runs anything.
        None => Some(false),
        // A Google sign-in kept outside the file lives in the Keychain.
        Some("oauth-personal") => oauth.then_some(true),
        // A key kept outside the environment lives in the Keychain.
        Some("gemini-api-key") => env("GEMINI_API_KEY").is_some().then_some(true),
        Some("vertex-ai") => Some(
            env("GOOGLE_API_KEY").is_some()
                || env("GOOGLE_CLOUD_PROJECT").is_some() && env("GOOGLE_CLOUD_LOCATION").is_some(),
        ),
        // Cloud Shell, application default credentials, a gateway: the machine's own
        // credentials, which the driver does not read.
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Run only where `gemini` and the corpus home are here, with `PATH` and `HOME` as the
    /// test's environment gives them (`PATH=/tmp/gem/prefix/bin:$PATH HOME=/tmp/gem/home`).
    #[tokio::test]
    async fn lists_the_models_signed_out() {
        let home = PathBuf::from("/tmp/gem/home");
        if super::super::binary().is_none() || !home.join(".gemini").is_dir() {
            eprintln!("skipped: no gemini on PATH or no /tmp/gem/home");
            return;
        }
        let root = home.join(".gemini");
        let models = list(&root).await.expect("models");
        let ids: Vec<&str> = models.models.iter().map(|m| m.model.as_str()).collect();
        assert_eq!(ids.len(), 4, "{ids:?}");
        assert_eq!(ids[0], "auto");
        assert_eq!(models.models[0].label.as_deref(), Some("Auto"));
        assert_ne!(models.signed_in, Some(true));
        assert!(models.watch.contains(&root.join("oauth_creds.json")));
    }
}
