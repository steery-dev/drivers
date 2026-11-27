//! The canvas's models, each in its role (API.md §6.1, §8.1), and `models`: those models and
//! whether Claude answers here, by the check of [`crate::claude::Source::check`].

use steery_sdk::wire::harness::{Model, Models};

use crate::claude::Source;
use crate::transcript::PROVIDER;

/// One role and the model it runs: its variable, else its default.
pub struct Role {
    pub role: &'static str,
    variable: &'static str,
    default: &'static str,
}

impl Role {
    pub fn model(&self) -> String {
        std::env::var(self.variable)
            .ok()
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| self.default.to_owned())
    }
}

pub const SKETCHER: Role = Role {
    role: "sketcher",
    variable: "CANVAS_SKETCH_MODEL",
    default: "claude-haiku-5-5",
};
pub const PLOTTER: Role = Role {
    role: "plotter",
    variable: "CANVAS_PLOTTER_MODEL",
    default: "claude-sonnet-5-5",
};
/// The navigator runs in Steery, in the person's own harness; its model is listed here.
pub const NAVIGATOR: Role = Role {
    role: "navigator",
    variable: "CANVAS_NAVIGATOR_MODEL",
    default: "claude-opus-5-5",
};

/// `models`: the plotter's first, since it writes the reply. Each runs in its own role, so
/// no other model runs.
pub async fn models() -> Models {
    let signed_in = Source::read().check().await.is_none();
    Models {
        models: [&PLOTTER, &SKETCHER, &NAVIGATOR]
            .into_iter()
            .map(|role| {
                let model = role.model();
                Model {
                    provider: Some(PROVIDER.into()),
                    label: Some(format!("{model} ({})", role.role)),
                    model,
                    thinking: Vec::new(),
                    cost: None,
                }
            })
            .collect(),
        unlisted: None,
        watch: Vec::new(),
        signed_in: Some(signed_in),
        signed_in_at: None,
    }
}
