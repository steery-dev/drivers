//! The one need Amp has (PROTOCOL.md §4.1, Needs): a sign-in, where no `AMP_API_KEY` is set
//! and `amp threads list` refuses (it exits 1 signed out). Its person gives it through Amp's
//! own `amp login`, run unmodified on a terminal of the driver's
//! ([`steery_driver_common::login`]), or with an Amp API key, which the runtime keeps sealed
//! and gives Amp as `AMP_API_KEY`: a key in the environment wins over Amp's saved accounts.
//! The driver keeps nothing either gives.
//!
//! UNVERIFIED until a person signs in through it: Amp 0.0.1794225648's `amp login` prints
//! "Open this URL in your browser to authorize:" and its link, and where it cannot open a
//! browser it asks on its terminal to "paste the callback URL or authorization code". The
//! driver relays the link and types what its person pastes into that prompt.

use std::time::Duration;

use steery_driver_common::login::{self, Login, Waiting};
use steery_driver_common::program::{ask, helper};
use steery_driver_common::signed::Known;
use steery_sdk::wire::harness::{Field, Give, Given, Need, NeedLink, Way};
use steery_sdk::{Error, refusal};

use crate::home;

/// The need, by its id in every plan.
const NEED: &str = "sign-in";
/// Its ways, and the step a started sign-in asks next.
const AMP: &str = "amp";
const AMP_RETURN: &str = "amp-return";
const API_KEY: &str = "api-key";
/// The field a return fills, or its person pastes into.
const CODE: &str = "code";
/// The field an API key is typed in, named as the variable it becomes.
const KEY: &str = "AMP_API_KEY";
/// What every Amp API key starts with (`sgamp_user_…`, `sgamp_sa_…`).
const KEY_PREFIX: &str = "sgamp_";
/// Where an Amp API key is made, as Amp's own code names it.
const KEYS_PAGE: &str = "https://ampcode.com/settings/security#access-token";
/// What Amp 0.0.1794225648 says, signed out: "Error: Invalid or missing API key. Run 'amp
/// login' to authenticate."
const SIGNED_OUT: &str = "Run 'amp login'";

/// How long a login has to print its link.
const LINK_WITHIN: Duration = Duration::from_secs(15);
/// How long Amp has to list one thread with a key given: inside `needs/give`'s 30 s.
const KEY_WITHIN: Duration = Duration::from_secs(20);

/// The last answer of `amp threads list` in a plan, in this process. A probe with no answer
/// uses it.
static SIGNED: Known = Known::new();

/// Whether Amp is signed out here: no `AMP_API_KEY`, and Amp says so ([`signed_in`]). Where
/// Amp says nothing, the last answer it gave.
pub async fn signed_out(binary: &std::path::Path) -> bool {
    if std::env::var_os(KEY).is_some_and(|key| !key.is_empty()) {
        return false;
    }
    lacks(&SIGNED, signed_in(binary).await)
}

/// Whether the plan names the sign-in need, with the probe's answer `now`.
fn lacks(known: &Known, now: Option<bool>) -> bool {
    known.steady(now) == Some(false)
}

/// What Amp says of its account here: `amp threads list --json --limit 1` lists (true), or
/// exits 1 saying to sign in (false), within the probe's time. Amp that does not answer in
/// time, or fails otherwise (offline), says nothing.
pub async fn signed_in(binary: &std::path::Path) -> Option<bool> {
    let mut command = helper(binary);
    command.args(["threads", "list", "--json", "--limit", "1"]);
    let answer = ask(command).await.ok()?;
    match answer.code {
        Some(0) => Some(true),
        Some(1) if answer.said.contains(SIGNED_OUT) => Some(false),
        _ => None,
    }
}

/// The need, where Amp is found here and signed out.
pub fn need() -> Need {
    Need {
        id: NEED.into(),
        title: "Sign Amp in?".into(),
        line: "Amp is not signed in on this host. Steery cannot read its threads here.".into(),
        ways: vec![
            Way {
                id: AMP.into(),
                label: "Sign in to Amp".into(),
                about: Some(
                    "Amp's own sign-in. This host keeps it. Each new host asks again.".into(),
                ),
                link: None,
                fields: Vec::new(),
                ends_at: None,
            },
            Way {
                id: API_KEY.into(),
                label: "Use your Amp API key".into(),
                about: Some(
                    "Steery keeps the key sealed and gives it to Amp on this machine.".into(),
                ),
                link: Some(NeedLink {
                    label: "Create a key".into(),
                    url: KEYS_PAGE.into(),
                    fills: None,
                }),
                fields: vec![Field {
                    name: KEY.into(),
                    label: "API key".into(),
                    about: Some(format!("Starts with {KEY_PREFIX}")),
                    secret: true,
                }],
                ends_at: None,
            },
        ],
    }
}

/// The sign-in started here and waiting for its person.
#[derive(Default, Clone)]
pub struct SignIns(Waiting);

impl SignIns {
    /// `needs/give`. A `done` forgets the last answer of the probe: the next probe with no
    /// answer plans no need, not the sign-in just made.
    pub async fn give(&self, give: Give) -> Result<Given, Error> {
        let given = self.step(give).await?;
        if matches!(given, Given::Done { .. }) {
            SIGNED.forget();
        }
        Ok(given)
    }

    async fn step(&self, give: Give) -> Result<Given, Error> {
        if give.need != NEED {
            return Err(gone(format!("Amp has no need {} here", give.need)));
        }
        let value = |name: &str| give.values.get(name).map_or("", |value| value.trim());
        match give.way.as_str() {
            AMP => self.amp().await,
            AMP_RETURN => {
                self.0.give(AMP_RETURN, value(CODE), "Amp").await?;
                Ok(Given::Done {
                    line: "Amp is signed in on this host".into(),
                    env: Default::default(),
                    unset: Vec::new(),
                })
            }
            API_KEY => {
                self.0.end();
                let key = value(KEY);
                api_key(key).await?;
                Ok(Given::Done {
                    line: "Amp uses your API key on this machine".into(),
                    env: [(KEY.to_owned(), key.to_owned())].into(),
                    unset: Vec::new(),
                })
            }
            way => Err(gone(format!("Amp has no sign-in way {way}"))),
        }
    }

    /// Start `amp login` and answer the step its link is.
    async fn amp(&self) -> Result<Given, Error> {
        self.0.end();
        let binary = home::binary().ok_or_else(|| refused("Amp is not installed here"))?;
        let mut command = helper(&binary);
        command.arg("login");
        let mut running = Login::start(command, AMP_RETURN.into(), true)
            .map_err(|e| refused(format!("amp login did not start: {e}")))?;
        let url = running
            .shows(LINK_WITHIN, login::link)
            .await
            .ok_or_else(|| unprinted(&running))?;
        let returns = running.link(&url);
        let ends_at = self.0.keep(running);
        Ok(Given::Next(Way {
            id: AMP_RETURN.into(),
            label: "Sign in to Amp".into(),
            about: Some(
                "Sign in on Amp's page. Then paste the address where your browser ended, or the \
                 code that the page shows."
                    .into(),
            ),
            link: Some(NeedLink {
                label: "Sign in to Amp".into(),
                url,
                fills: returns.then(|| CODE.into()),
            }),
            fields: vec![Field {
                name: CODE.into(),
                label: "Address or code".into(),
                about: Some(
                    "The address where your browser ended, or the code from Amp's page. Leave it \
                     empty when you are signed in."
                        .into(),
                ),
                secret: true,
            }],
            ends_at: Some(ends_at),
        }))
    }
}

/// Check `key`: Amp lists one thread with it as `AMP_API_KEY`. Refused with Amp's own line
/// where Amp refuses it; the key is in no line.
async fn api_key(key: &str) -> Result<(), Error> {
    if !key.starts_with(KEY_PREFIX) {
        return Err(refused(format!(
            "Paste your Amp API key. It starts with {KEY_PREFIX}"
        )));
    }
    let binary = home::binary().ok_or_else(|| refused("Amp is not installed here"))?;
    let ran = home::run(
        &binary,
        &["threads", "list", "--json", "--limit", "1"],
        None,
        &[(KEY, key)],
        KEY_WITHIN,
        1 << 20,
    )
    .await
    .map_err(|failed| refused(failed.to_string()))?;
    if ran.success {
        return Ok(());
    }
    let line = ran.line().map_or_else(
        || "Amp refused the key".to_owned(),
        |line| line.replace(key, "…"),
    );
    Err(refused(line))
}

/// Why a started login gave no link: its own last line, where it printed one.
fn unprinted(running: &Login) -> Error {
    refused(running.last_line().unwrap_or_else(|| {
        format!(
            "amp login printed no sign-in link within {} s",
            LINK_WITHIN.as_secs()
        )
    }))
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
