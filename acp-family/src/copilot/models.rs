//! The models Copilot runs here, from its own server's `models.list`.

use std::path::Path;

use serde_json::Value;
use steery_sdk::wire::harness::{Model, Models};
use steery_sdk::{Error, refusal};

use super::{home, server};

/// Copilot's model that picks a model for each request. It is listed first, but a session
/// that names no model runs the first model listed after it (1.0.108).
const AUTO: &str = "auto";

/// `models`: Copilot's own list, its default first. Each model's thinking is its
/// `supportedReasoningEfforts`. Copilot keeps its settings in `settings.json` and its
/// sign-ins in `config.json`, so a change to either changes the list.
pub async fn models(root: &Path) -> Result<Models, Error> {
    let binary = home::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "copilot is not installed here"))?;
    let signed_in = server::signed_in(&binary).await;
    let listed = server::models(&binary)
        .await
        .map_err(|why| Error::refused(refusal::REFUSED, why))?;
    Ok(Models {
        models: ordered(&listed),
        unlisted: None,
        watch: vec![root.join("settings.json"), root.join("config.json")],
        signed_in,
    })
}

/// The models of `listed`, the default first: `auto` goes after the first model it names.
fn ordered(listed: &[Value]) -> Vec<Model> {
    let mut models: Vec<Model> = listed.iter().filter_map(model).collect();
    if models.first().is_some_and(|first| first.model == AUTO) && models.len() > 1 {
        models.swap(0, 1);
    }
    models
}

fn model(listed: &Value) -> Option<Model> {
    let thinking = listed
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
        .map(|efforts| {
            efforts
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Some(Model {
        provider: listed
            .pointer("/provider/id")
            .and_then(Value::as_str)
            .map(str::to_owned),
        model: listed.get("id")?.as_str()?.to_owned(),
        label: listed
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        thinking,
        cost: None,
    })
}
