//! `canvas models`: the models the canvas runs, each in its role, and whether it is signed in
//! to Claude here, from the same check its server runs at start (`GET <base>/v1/models`
//! within 8 s, or `claude auth status` for a plan).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use steery_driver_common::program::helper;
use steery_sdk::wire::harness::{Model, Models};
use steery_sdk::{Error, refusal};

use crate::home;
use crate::transcript::PROVIDER;

/// How long `canvas models` has: its own check's 8 s, and Bun's start.
const BOUND: Duration = Duration::from_secs(12);
/// The role whose model writes the reply: the session's model.
const ANSWERS: &str = "plotter";

/// What `canvas models` prints.
#[derive(Debug, Deserialize)]
pub struct Said {
    pub signed_in: bool,
    /// Why it is not signed in, in plain words.
    #[serde(default)]
    pub line: Option<String>,
    #[serde(default)]
    pub models: Vec<Role>,
}

/// One model, in its role: `sketcher`, `plotter` or `navigator`.
#[derive(Debug, Deserialize)]
pub struct Role {
    pub id: String,
    pub role: String,
}

/// `canvas models`, run within [`BOUND`]. Why it gave no answer, in one line, where it did
/// not.
pub async fn ask(binary: &Path) -> Result<Said, String> {
    let mut command = helper(binary);
    command.arg("models").stdout(Stdio::piped());
    let output = tokio::time::timeout(BOUND, command.output())
        .await
        .map_err(|_| format!("`canvas models` did not answer within {} s", BOUND.as_secs()))?
        .map_err(|e| format!("`canvas models` did not start: {e}"))?;
    if !output.status.success() {
        return Err(format!("`canvas models` exited with {}", output.status));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("`canvas models` printed no answer that reads: {e}"))
}

/// `models`: the canvas's models, the plotter's first, as `canvas models` lists them. The
/// canvas picks each model by its role, so no other model runs.
pub async fn models(home: &Path) -> Result<Models, Error> {
    let binary = home::installed(home)?;
    let said = ask(&binary)
        .await
        .map_err(|why| Error::refused(refusal::REFUSED, why))?;
    let mut roles = said.models;
    roles.sort_by_key(|role| role.role != ANSWERS);
    Ok(Models {
        models: roles
            .into_iter()
            .map(|role| Model {
                provider: Some(PROVIDER.into()),
                label: Some(format!("{} ({})", role.id, role.role)),
                model: role.id,
                thinking: Vec::new(),
                cost: None,
            })
            .collect(),
        unlisted: None,
        watch: Vec::new(),
        signed_in: Some(said.signed_in),
        signed_in_at: None,
    })
}
