//! The one need OpenClaw has (PROTOCOL.md §4.1, Needs): a sign-in, where `openclaw models
//! status --json --check` exits 1, OpenClaw's own word that a model in use lacks its
//! sign-in (0: signed in; 2: a sign-in expires soon, still signed in). Its person gives it
//! through OpenClaw's own `openclaw models auth login` or `openclaw onboard`, run on a
//! terminal of this host, which both pick a provider interactively, so the driver only
//! checks again afterwards; or with an OpenRouter API key, which the runtime keeps and sets
//! as `OPENROUTER_API_KEY` on every host (OpenClaw reads it from the environment).

use std::path::{Path, PathBuf};

use serde_json::Value;
use steery_driver_common::program::{ask, helper};
use steery_driver_common::signed::Known;
use steery_sdk::wire::harness::{Field, Give, Given, Need, NeedLink, Way};
use steery_sdk::{Error, refusal};

/// The need, by its id in every plan.
const NEED: &str = "sign-in";
/// Its ways.
const TERMINAL: &str = "openclaw-sign-in";
const API_KEY: &str = "api-key";
/// The field an API key is typed in, and the variable OpenClaw reads it from.
const KEY: &str = "OPENROUTER_API_KEY";
/// Where an OpenRouter API key is made.
const KEYS_PAGE: &str = "https://openrouter.ai/settings/keys";
/// The exit of `models status --check` where a model in use lacks its sign-in.
const MISSING: i32 = 1;

/// What `openclaw models status --json --check` said.
pub(super) struct Status {
    /// Its exit code; none where a signal ended it.
    pub code: Option<i32>,
    /// The providers of the models in use that lack a sign-in.
    pub missing: Vec<String>,
}

impl Status {
    /// Whether OpenClaw says it can run its models: None where it said neither.
    pub fn signed_in(&self) -> Option<bool> {
        match self.code {
            Some(0 | 2) => Some(true),
            Some(MISSING) => Some(false),
            _ => None,
        }
    }
}

/// `openclaw models status --json --check`, with `env` added, within the probe's bound.
pub(super) async fn status(binary: &Path, env: &[(&str, &str)]) -> Result<Status, String> {
    let mut command = helper(binary);
    command
        .args(["models", "status", "--json", "--check"])
        .envs(env.iter().copied());
    let answer = ask(command)
        .await
        .map_err(|why| format!("`openclaw models status` {why}"))?;
    let missing = serde_json::from_slice::<Value>(&answer.stdout)
        .ok()
        .and_then(|status| {
            let missing = status.pointer("/auth/missingProvidersInUse")?.as_array()?;
            Some(
                missing
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
            )
        })
        .unwrap_or_default();
    Ok(Status {
        code: answer.code,
        missing,
    })
}

/// The last answer of `openclaw models status` in a plan, in this process. A probe with no
/// answer uses it.
static SIGNED: Known = Known::new();

/// Whether OpenClaw is here and says a model in use lacks its sign-in. Where `models status`
/// says nothing, the last answer it gave.
pub async fn lacking(home: &Path) -> bool {
    let Some(binary) = super::setup::binary(home) else {
        return false;
    };
    let now = match status(&binary, &[]).await {
        Ok(status) => status.signed_in(),
        Err(why) => {
            eprintln!("openclaw sign-in probe: {why}");
            None
        }
    };
    lacks(&SIGNED, now)
}

/// Whether the steady answer says OpenClaw lacks a sign-in, with the probe's answer `now`.
fn lacks(known: &Known, now: Option<bool>) -> bool {
    known.steady(now) == Some(false)
}

/// The need, where OpenClaw is here and not signed in.
pub fn need() -> Need {
    Need {
        id: NEED.into(),
        title: "Sign OpenClaw in?".into(),
        line: "OpenClaw has no sign-in for the model it uses on this host. It cannot run a turn \
               here."
            .into(),
        ways: vec![
            Way {
                id: TERMINAL.into(),
                label: "Sign in on this host's terminal".into(),
                about: Some(
                    "Run `openclaw models auth login` (or `openclaw onboard`) in a terminal on \
                     this host and pick your provider. Then continue: Steery checks again."
                        .into(),
                ),
                link: None,
                fields: Vec::new(),
                ends_at: None,
            },
            Way {
                id: API_KEY.into(),
                label: "Use your OpenRouter API key".into(),
                about: Some(
                    "Your OpenRouter account pays for each token, on every host. It counts \
                     where OpenClaw's model is an OpenRouter one."
                        .into(),
                ),
                link: Some(NeedLink {
                    label: "Create a key".into(),
                    url: KEYS_PAGE.into(),
                    fills: None,
                }),
                fields: vec![Field {
                    name: KEY.into(),
                    label: "API key".into(),
                    about: Some("Starts with sk-or-".into()),
                    secret: true,
                }],
                ends_at: None,
            },
        ],
    }
}

/// OpenClaw's sign-ins: none waits on the driver, since its own login runs on a terminal
/// of its person's.
#[derive(Default)]
pub struct SignIns;

impl SignIns {
    /// `needs/give`, for OpenClaw with the person's `home`.
    pub async fn give(&self, home: &Path, give: Give) -> Result<Given, Error> {
        if give.need != NEED {
            return Err(gone(format!("OpenClaw has no need {} here", give.need)));
        }
        let binary = installed(home)?;
        match give.way.as_str() {
            TERMINAL => {
                checked(&binary, &[]).await?;
                Ok(Given::Done {
                    line: "OpenClaw is signed in on this host".into(),
                    env: Default::default(),
                    unset: Vec::new(),
                })
            }
            API_KEY => {
                let key = give.values.get(KEY).map_or("", |key| key.trim());
                if key.is_empty() {
                    return Err(refused("Paste your OpenRouter API key"));
                }
                checked(&binary, &[(KEY, key)]).await?;
                Ok(Given::Done {
                    line: "OpenClaw uses your OpenRouter API key".into(),
                    env: [(KEY.to_owned(), key.to_owned())].into(),
                    unset: Vec::new(),
                })
            }
            way => Err(gone(format!("OpenClaw has no sign-in way {way}"))),
        }
    }
}

/// OpenClaw, given `env`, must no longer say that a model in use lacks its sign-in.
async fn checked(binary: &Path, env: &[(&str, &str)]) -> Result<(), Error> {
    let status = status(binary, env).await.map_err(refused)?;
    match status.signed_in() {
        Some(true) => Ok(()),
        Some(false) if status.missing.is_empty() => {
            Err(refused("OpenClaw still has no sign-in for its model"))
        }
        Some(false) => Err(refused(format!(
            "OpenClaw still has no sign-in for {}",
            status.missing.join(", ")
        ))),
        None => Err(refused("`openclaw models status` did not say its sign-in")),
    }
}

fn installed(home: &Path) -> Result<PathBuf, Error> {
    super::setup::binary(home)
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "openclaw is not installed here"))
}

fn gone(line: impl Into<String>) -> Error {
    Error::refused(refusal::NOT_FOUND, line)
}

fn refused(line: impl Into<String>) -> Error {
    Error::refused(refusal::REFUSED, line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_with_no_answer_keeps_the_last_answer_for_the_need() {
        let known = Known::new();
        let planned = [Some(false), None, None, Some(true), None].map(|probe| lacks(&known, probe));
        assert_eq!(planned, [true, true, true, false, false]);
        assert!(!lacks(&Known::new(), None));
    }
}
