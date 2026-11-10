//! The one need Grok has (PROTOCOL.md §4.1, Needs): a sign-in, where `grok models` says
//! `You are not authenticated.` Its person gives it through Grok's own `grok login
//! --device-auth`, run unmodified on a terminal of the driver's
//! ([`steery_driver_common::login`]), which Grok keeps in `<root>/auth.json`; or with an
//! xAI API key, which the runtime keeps and sets as `XAI_API_KEY` on every host.
//!
//! Grok 1.0.77's `grok login --device-auth` prints `To sign in, open this URL in your
//! browser:` and `https://accounts.x.ai/oauth2/device?user_code=…`, then `Confirm this code
//! in your browser:` and the code, each on its own line, and waits; it reads nothing on its
//! terminal.

use std::path::{Path, PathBuf};
use std::time::Duration;

use steery_driver_common::login::{self, Login, Waiting};
use steery_driver_common::program::helper;
use steery_sdk::wire::harness::{Field, Give, Given, Need, NeedLink, Way};
use steery_sdk::{Error, refusal};

use super::models;

/// The need, by its id in every plan.
const NEED: &str = "sign-in";
/// Its ways. A started `grok-sign-in` waits on its own way.
const GROK_SIGN_IN: &str = "grok-sign-in";
const API_KEY: &str = "api-key";
/// The field an API key is typed in, and the variable Grok reads it from.
const KEY: &str = "XAI_API_KEY";
/// Where an xAI API key is made.
const KEYS_PAGE: &str = "https://console.x.ai";
/// What Grok prints before its code, after its page.
const CONFIRM: &str = "Confirm this code in your browser:";
/// What `grok models` warns where the key it was given is refused.
const UNFETCHED: &str = "Failed to fetch models";

/// How long a login has to print its page and code.
const LINK_WITHIN: Duration = Duration::from_secs(15);

/// Whether Grok in its home `root` is here and says it is not signed in.
pub async fn lacking(root: &Path) -> bool {
    let Some(binary) = super::binary(root) else {
        return false;
    };
    match models::printed(&binary, root, &[]).await {
        Ok(printed) => printed.signed_in() == Some(false),
        Err(why) => {
            eprintln!("grok sign-in probe: {why}");
            false
        }
    }
}

/// The need, where Grok is here and not signed in.
pub fn need() -> Need {
    Need {
        id: NEED.into(),
        title: "Sign Grok Build in?".into(),
        line: "Grok Build is not signed in on this host. It cannot run a turn here.".into(),
        ways: vec![
            Way {
                id: GROK_SIGN_IN.into(),
                label: "Sign in with your xAI account".into(),
                about: Some(
                    "Grok shows a code. Confirm it on xAI's page, from any device. This host \
                     keeps the sign-in. Each new host asks again."
                        .into(),
                ),
                link: None,
                fields: Vec::new(),
                ends_at: None,
            },
            Way {
                id: API_KEY.into(),
                label: "Use your xAI API key".into(),
                about: Some("Your xAI account pays for each token, on every host.".into()),
                link: Some(NeedLink {
                    label: "Create a key".into(),
                    url: KEYS_PAGE.into(),
                    fills: None,
                }),
                fields: vec![Field {
                    name: KEY.into(),
                    label: "API key".into(),
                    about: Some("Starts with xai-".into()),
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
    /// `needs/give`, for Grok in its home `root`.
    pub async fn give(&self, root: &Path, give: Give) -> Result<Given, Error> {
        if give.need != NEED {
            return Err(gone(format!("Grok Build has no need {} here", give.need)));
        }
        match give.way.as_str() {
            GROK_SIGN_IN if self.0.waits_on(GROK_SIGN_IN) => {
                self.0.give(GROK_SIGN_IN, "", "Grok Build").await?;
                Ok(Given::Done {
                    line: "Grok Build is signed in on this host".into(),
                    env: Default::default(),
                })
            }
            GROK_SIGN_IN => self.device(root).await,
            API_KEY => {
                self.0.end();
                let key = give.values.get(KEY).map_or("", |key| key.trim());
                checked(root, key).await?;
                Ok(Given::Done {
                    line: "Grok Build uses your xAI API key".into(),
                    env: [(KEY.to_owned(), key.to_owned())].into(),
                })
            }
            way => Err(gone(format!("Grok Build has no sign-in way {way}"))),
        }
    }

    /// Start `grok login --device-auth` and answer the step its page and code are. A
    /// sign-in started before is ended.
    async fn device(&self, root: &Path) -> Result<Given, Error> {
        self.0.end();
        let binary = installed(root)?;
        let mut command = helper(&binary);
        command
            .args(["login", "--device-auth"])
            .env("GROK_HOME", root)
            .env("RUST_LOG", "off");
        let running = Login::start(command, GROK_SIGN_IN.into(), false)
            .map_err(|e| refused(format!("grok login did not start: {e}")))?;
        // The page comes first; the code is the line after the one naming it, once that
        // line has ended.
        let printed = running
            .shows(LINK_WITHIN, |shown| {
                let url = login::link(shown)?;
                let rest = &shown[shown.find(CONFIRM)? + CONFIRM.len()..];
                let (code, _) = rest.trim_start().split_once(['\r', '\n'])?;
                let code = code.trim();
                (!code.is_empty()).then(|| (url, code.to_owned()))
            })
            .await;
        let (url, code) = printed.ok_or_else(|| {
            refused(running.last_line().unwrap_or_else(|| {
                format!(
                    "grok login printed no sign-in page within {} s",
                    LINK_WITHIN.as_secs()
                )
            }))
        })?;
        let ends_at = self.0.keep(running);
        Ok(Given::Next(Way {
            id: GROK_SIGN_IN.into(),
            label: format!("Confirm code {code} on xAI's page"),
            about: Some(format!(
                "Open the page, sign in, and confirm {code}. Then continue. Do not confirm a code \
                 that you did not start."
            )),
            link: Some(NeedLink {
                label: "Open xAI's page".into(),
                url,
                fills: None,
            }),
            fields: Vec::new(),
            ends_at: Some(ends_at),
        }))
    }
}

/// Check `key` the way Grok takes it: `grok models` with it as `XAI_API_KEY`, in an empty
/// home of its own, must say it uses it and fetch xAI's models with it.
async fn checked(root: &Path, key: &str) -> Result<(), Error> {
    if key.is_empty() {
        return Err(refused("Paste your xAI API key"));
    }
    let binary = installed(root)?;
    let scratch =
        Scratch::new().map_err(|e| refused(format!("no folder to check the key in: {e}")))?;
    let printed = models::printed(&binary, &scratch.0, &[(KEY, key), ("RUST_LOG", "warn")])
        .await
        .map_err(refused)?;
    if printed.signed_in() != Some(true) {
        return Err(refused("Grok Build did not take the key"));
    }
    match printed.warned.lines().find(|line| line.contains(UNFETCHED)) {
        Some(line) => Err(refused(xai_said(line).unwrap_or("xAI refused the key"))),
        None => Ok(()),
    }
}

/// xAI's own `"error"` in a warning that quotes its answer.
fn xai_said(line: &str) -> Option<&str> {
    const ERROR: &str = r#""error":""#;
    let rest = &line[line.find(ERROR)? + ERROR.len()..];
    Some(&rest[..rest.find('"')?])
}

fn installed(root: &Path) -> Result<PathBuf, Error> {
    super::binary(root)
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "grok is not installed here"))
}

/// An empty folder of the driver's, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!("steery-grok-{}-{nanos}", std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn gone(line: impl Into<String>) -> Error {
    Error::refused(refusal::NOT_FOUND, line)
}

fn refused(line: impl Into<String>) -> Error {
    Error::refused(refusal::REFUSED, line)
}
