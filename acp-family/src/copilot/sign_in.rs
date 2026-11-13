//! The one need Copilot has (PROTOCOL.md §4.1, Needs): a sign-in, where it is found and its
//! own server's `auth.getStatus` says it is not signed in. Its person gives it through
//! Copilot's own `copilot login --device-code`, run unmodified on a terminal of the driver's
//! ([`steery_driver_common::login`]): GitHub's device page takes the code it prints, from
//! any device, so no address comes back to this host. Or with a GitHub token that
//! `copilot login --with-token` reads on its standard input. Copilot keeps what its sign-in
//! gives; the driver reads no token back and keeps none.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use steery_driver_common::login::{self, Login, Waiting};
use steery_driver_common::program::helper;
use steery_driver_common::signed::Known;
use steery_sdk::wire::harness::{Field, Give, Given, Need, NeedLink, Way};
use steery_sdk::{Error, refusal};
use tokio::io::AsyncWriteExt;

use super::{home, server};

/// The need, by its id in every plan.
const NEED: &str = "sign-in";
/// Copilot's own sign-in, and the step it asks next once started.
const SIGN_IN: &str = "copilot-sign-in";
const SIGN_IN_DONE: &str = "copilot-sign-in-done";
/// A token, and its field.
const TOKEN_WAY: &str = "github-token";
const TOKEN: &str = "token";
/// How long `copilot login --device-code` has to print its code: at once (1.0.108).
const CODE_WITHIN: Duration = Duration::from_secs(15);
/// How long `copilot login --with-token` has to check and keep a token.
const TOKEN_WITHIN: Duration = Duration::from_secs(30);

/// The last answer of Copilot's server in a plan, in this process. A probe with no answer
/// uses it.
static SIGNED: Known = Known::new();

/// Whether the plan names the sign-in need: Copilot's server says it is not signed in, or
/// says nothing and said so last.
pub async fn lacking(binary: &Path) -> bool {
    lacks(&SIGNED, server::signed_in(binary).await)
}

/// Whether the steady answer says Copilot is not signed in, with the probe's answer `now`.
fn lacks(known: &Known, now: Option<bool>) -> bool {
    known.steady(now) == Some(false)
}

/// The need, where Copilot is found here and not signed in.
pub fn need() -> Need {
    Need {
        id: NEED.into(),
        title: "Sign Copilot in?".into(),
        line: "Copilot is not signed in on this host. It cannot run a turn here.".into(),
        ways: vec![
            Way {
                id: SIGN_IN.into(),
                label: "Sign in with GitHub".into(),
                about: Some(
                    "Copilot's own sign-in, with a code on GitHub's device page. This host keeps \
                     it. Each new host asks again."
                        .into(),
                ),
                link: None,
                fields: Vec::new(),
                ends_at: None,
            },
            Way {
                id: TOKEN_WAY.into(),
                label: "Use a GitHub token".into(),
                about: Some(
                    "A fine-grained personal access token with the Copilot Requests permission. \
                     `copilot login --with-token` checks it, and this host keeps it."
                        .into(),
                ),
                link: None,
                fields: vec![Field {
                    name: TOKEN.into(),
                    label: "GitHub token".into(),
                    about: None,
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
    /// `needs/give`.
    pub async fn give(&self, give: Give) -> Result<Given, Error> {
        if give.need != NEED {
            return Err(gone(format!("Copilot has no need {} here", give.need)));
        }
        match give.way.as_str() {
            SIGN_IN => self.start().await,
            SIGN_IN_DONE => {
                self.0.give(SIGN_IN_DONE, "", "Copilot").await?;
                Ok(signed())
            }
            TOKEN_WAY => {
                self.0.end();
                let token = give.values.get(TOKEN).map_or("", |token| token.trim());
                with_token(token).await?;
                Ok(signed())
            }
            way => Err(gone(format!("Copilot has no sign-in way {way}"))),
        }
    }

    /// Start `copilot login --device-code`, and answer the step its code is. A sign-in
    /// started before is ended.
    async fn start(&self) -> Result<Given, Error> {
        self.0.end();
        let binary = home::binary().ok_or_else(|| gone("Copilot is not installed here"))?;
        let mut command = helper(&binary);
        command.args(["login", "--device-code"]);
        let running = Login::start(command, SIGN_IN_DONE.into(), false)
            .map_err(|e| refused(format!("copilot login did not start: {e}")))?;
        let shown = running
            .shows(CODE_WITHIN, |shown| {
                Some((login::link(shown)?, login::after(shown, "enter code ")?))
            })
            .await;
        let Some((url, code)) = shown else {
            return Err(refused(running.last_line().unwrap_or_else(|| {
                format!(
                    "copilot login gave no code within {} s",
                    CODE_WITHIN.as_secs()
                )
            })));
        };
        let ends_at = self.0.keep(running);
        Ok(Given::Next(Way {
            id: SIGN_IN_DONE.into(),
            label: format!("Enter {code} on GitHub"),
            about: Some(format!(
                "Open GitHub's device page, enter the code {code} and allow GitHub Copilot CLI. \
                 Then tap Done."
            )),
            link: Some(NeedLink {
                label: "Open GitHub".into(),
                url,
                fills: None,
            }),
            fields: Vec::new(),
            ends_at: Some(ends_at),
        }))
    }
}

/// `copilot login --with-token`, the token on its standard input.
async fn with_token(token: &str) -> Result<(), Error> {
    if token.is_empty() {
        return Err(refused("Paste a GitHub token."));
    }
    let binary = home::binary().ok_or_else(|| gone("Copilot is not installed here"))?;
    let mut command = helper(&binary);
    command
        .args(["login", "--with-token"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| refused(format!("copilot login did not start: {e}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| refused("copilot login took no input"))?;
    let ran = async {
        stdin.write_all(format!("{token}\n").as_bytes()).await?;
        drop(stdin);
        child.wait_with_output().await
    };
    let output = tokio::time::timeout(TOKEN_WITHIN, ran)
        .await
        .map_err(|_| {
            refused(format!(
                "copilot login did not end within {} s",
                TOKEN_WITHIN.as_secs()
            ))
        })?
        .map_err(|e| refused(format!("copilot login did not run: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&output.stderr);
    Err(refused(
        said.lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map_or_else(
                || format!("copilot login refused the token: {}", output.status),
                str::to_owned,
            ),
    ))
}

fn signed() -> Given {
    Given::Done {
        line: "Copilot is signed in on this host".into(),
        env: Default::default(),
        unset: Vec::new(),
    }
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
