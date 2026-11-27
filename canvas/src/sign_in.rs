//! The one need the canvas has (PROTOCOL.md §4.1, Needs): a sign-in to Claude, where
//! `canvas models` says `signed_in: false`. The canvas reads Claude Code's own variables, so
//! its person gives it in one of Claude Code's three ways ([`steery_driver_common::claude`]):
//!
//! - A Claude plan: Claude Code's own `claude auth login`. The canvas calls Claude through
//!   Claude Code, which keeps what its sign-in gives. Offered only where Claude Code is here.
//! - An Anthropic API key, kept by the runtime as `ANTHROPIC_API_KEY` on every host.
//! - A company's gateway: `ANTHROPIC_BASE_URL`, and its key as `ANTHROPIC_AUTH_TOKEN` or
//!   `ANTHROPIC_API_KEY`, once one `GET /v1/models` with it did not refuse it.
//!
//! A key or a gateway outranks a plan in the canvas, so from protocol 15 each `done` unsets
//! the variables of the ways it replaces.

use std::collections::BTreeMap;
use std::path::Path;

use steery_driver_common::login::Waiting;
use steery_driver_common::{anthropic, claude, gateway};
use steery_sdk::wire::harness::{Give, Given, Need};
use steery_sdk::{Error, refusal};

/// The need, by its id in every plan.
pub const NEED: &str = "sign-in";
/// How the canvas's lines name it.
const CANVAS: &str = "Canvas";

/// The need, where the canvas is here and not signed in: `line` is the canvas's own why.
pub fn need(line: Option<String>, claude: bool) -> Need {
    let mut ways = Vec::new();
    if claude {
        ways.push(claude::plan_way(
            "Claude Code's own sign-in. The canvas calls Claude through Claude Code. This host \
             keeps it. Each new host asks again."
                .into(),
        ));
    }
    ways.push(anthropic::way(
        "Your Claude Console account pays for each token, on every host. The canvas uses the \
         key in place of a Claude plan."
            .into(),
    ));
    ways.push(claude::gateway_way(
        "For a company that sends Claude through its own gateway. Every host uses this \
         address and key."
            .into(),
    ));
    Need {
        id: NEED.into(),
        title: "Sign the canvas in to Claude?".into(),
        line: line
            .filter(|line| !line.trim().is_empty())
            .unwrap_or_else(|| "The canvas is not signed in to Claude on this host.".into()),
        ways,
    }
}

/// The sign-in a person asked to give again, where the canvas is signed in already.
pub fn change(claude: bool) -> Need {
    Need {
        title: "Change how the canvas signs in to Claude?".into(),
        line: "The canvas is signed in to Claude on this host. Pick a new way to sign in.".into(),
        ..need(None, claude)
    }
}

/// The `done` of the plan's own sign-in: the key and the gateway are unset, since the
/// canvas uses them before a plan.
fn signed_in(unsets: bool) -> Given {
    Given::Done {
        line: "The canvas runs on your Claude plan on this host".into(),
        env: BTreeMap::new(),
        unset: match unsets {
            true => [anthropic::VARIABLE, claude::AUTH_TOKEN, claude::BASE_URL]
                .map(str::to_owned)
                .into(),
            false => Vec::new(),
        },
    }
}

/// The sign-in started here and waiting for its person.
#[derive(Default, Clone)]
pub struct SignIns(Waiting);

impl SignIns {
    /// `needs/give`. `unsets` is whether the driver picked protocol 15 or newer. A `done`
    /// forgets the last answer of the sign-in probe.
    pub async fn give(&self, home: &Path, give: Give, unsets: bool) -> Result<Given, Error> {
        if give.need != NEED {
            return Err(gone(format!("the canvas has no need {} here", give.need)));
        }
        let given = match give.way.as_str() {
            anthropic::WAY => {
                self.0.end();
                let outranked: &[&str] = match unsets {
                    true => &[claude::BASE_URL, claude::AUTH_TOKEN],
                    false => &[],
                };
                anthropic::give(&give.values, "The canvas uses your Anthropic API key", outranked)?
            }
            gateway::WAY => {
                self.0.end();
                let unset: Option<&[&str]> = unsets.then_some(&[][..]);
                claude::gateway_given(&give.values, CANVAS, unset).await?
            }
            claude::PLAN_WAY => {
                self.0.end();
                let binary = crate::home::claude(home)
                    .ok_or_else(|| gone("Claude Code is not installed here"))?;
                claude::start(&binary, &self.0).await?
            }
            claude::PLAN_RETURN => {
                claude::returned(&self.0, &give.values).await?;
                signed_in(unsets)
            }
            way => return Err(gone(format!("the canvas has no sign-in way {way}"))),
        };
        if matches!(given, Given::Done { .. }) {
            crate::setup::SIGNED.forget();
        }
        Ok(given)
    }
}

fn gone(line: impl Into<String>) -> Error {
    Error::refused(refusal::NOT_FOUND, line)
}
