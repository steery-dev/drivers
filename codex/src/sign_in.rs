//! The one need Codex has (PROTOCOL.md §4.1, Needs): a sign-in, where `codex login status`
//! says it is not logged in. Its person gives it through Codex's own `codex login`, run
//! unmodified on a terminal of the driver's ([`steery_driver_common::login`]): with ChatGPT
//! through the loopback link it prints, with ChatGPT through a code typed on OpenAI's page
//! (`--device-auth`), or with an OpenAI API key (`--with-api-key`). Codex keeps what each
//! gives in its home; the driver keeps nothing.
//!
//! Codex 0.166.0's `codex login` listens on `127.0.0.1:1455` and prints its authorize link
//! (`redirect_uri=http://127.0.0.1:1455/auth/callback`); it reads nothing on its terminal.
//! `codex login --device-auth` prints `https://auth.openai.com/codex/device` and a one-time
//! code that "expires in 15 minutes".

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use steery_driver_common::login::{self, Login, Waiting};
use steery_driver_common::program::helper;
use steery_sdk::wire::harness::{Field, Give, Given, Need, NeedLink, Way};
use steery_sdk::{Error, refusal};
use tokio::io::AsyncWriteExt as _;

/// The need, by its id in every plan.
const NEED: &str = "sign-in";
/// Its ways, and the steps a started sign-in asks next.
const CHATGPT: &str = "chatgpt";
const CHATGPT_RETURN: &str = "chatgpt-return";
const DEVICE: &str = "device";
const DEVICE_CODE: &str = "device-code";
const API_KEY: &str = "api-key";
/// The field a return fills, or its person pastes into.
const ADDRESS: &str = "address";
/// The field an API key is typed in.
const KEY: &str = "key";
/// Where an OpenAI API key is made.
const KEYS_PAGE: &str = "https://platform.openai.com/api-keys";

/// How long a login has to print its link and code: it printed them at once (0.166.0).
const LINK_WITHIN: Duration = Duration::from_secs(15);
/// How long `codex login --with-api-key` has to keep the key: inside `needs/give`'s 30 s.
const KEY_WITHIN: Duration = Duration::from_secs(20);

/// The need, where Codex is found here and not logged in.
pub fn need() -> Need {
    Need {
        id: NEED.into(),
        title: "Sign Codex in?".into(),
        line: "Codex is not logged in on this host, so it runs no turn here.".into(),
        ways: vec![
            way(CHATGPT, "Sign in with ChatGPT", "Codex's own sign-in, kept on this host: each new host asks again."),
            way(
                DEVICE,
                "Sign in with ChatGPT using a code",
                "Codex shows a code to type on OpenAI's page, from any device.",
            ),
            Way {
                id: API_KEY.into(),
                label: "Use your OpenAI API key".into(),
                about: Some("Billed per token to your OpenAI account; kept on this host.".into()),
                link: Some(NeedLink {
                    label: "Create a key".into(),
                    url: KEYS_PAGE.into(),
                    fills: None,
                }),
                fields: vec![Field {
                    name: KEY.into(),
                    label: "API key".into(),
                    about: Some("Starts with sk-".into()),
                    secret: true,
                }],
                ends_at: None,
            },
        ],
    }
}

fn way(id: &str, label: &str, about: &str) -> Way {
    Way {
        id: id.into(),
        label: label.into(),
        about: Some(about.into()),
        link: None,
        fields: Vec::new(),
        ends_at: None,
    }
}

/// The sign-in started here and waiting for its person.
#[derive(Default, Clone)]
pub struct SignIns(Waiting);

impl SignIns {
    /// `needs/give`, for Codex in its home `root`.
    pub async fn give(&self, root: &Path, give: Give) -> Result<Given, Error> {
        if give.need != NEED {
            return Err(gone(format!("Codex has no need {} here", give.need)));
        }
        let value = |name: &str| give.values.get(name).map_or("", |value| value.trim());
        match give.way.as_str() {
            CHATGPT => self.chatgpt(root).await,
            DEVICE => self.device(root).await,
            API_KEY => {
                self.0.end();
                api_key(root, value(KEY)).await?;
                Ok(done("Codex uses your OpenAI API key on this host"))
            }
            way @ (CHATGPT_RETURN | DEVICE_CODE) => {
                self.0.give(way, value(ADDRESS), "Codex").await?;
                Ok(done("Codex is signed in on this host"))
            }
            way => Err(gone(format!("Codex signs in no way {way}"))),
        }
    }

    /// Start `codex login` and answer the step its loopback link is.
    async fn chatgpt(&self, root: &Path) -> Result<Given, Error> {
        let mut running = self.start(root, &[], CHATGPT_RETURN)?;
        let url = running
            .shows(LINK_WITHIN, login::link)
            .await
            .ok_or_else(|| unprinted(&running))?;
        let returns = running.link(&url);
        let ends_at = self.0.keep(running);
        Ok(Given::Next(Way {
            id: CHATGPT_RETURN.into(),
            label: "Sign in to ChatGPT".into(),
            about: Some(
                "Sign in on OpenAI's page. Where it ends on an address that does not load, \
                 paste that address here."
                    .into(),
            ),
            link: Some(NeedLink {
                label: "Sign in to ChatGPT".into(),
                url,
                fills: returns.then(|| ADDRESS.into()),
            }),
            fields: vec![Field {
                name: ADDRESS.into(),
                label: "Address".into(),
                about: Some(
                    "Where your browser ended (http://127.0.0.1:1455/…); empty once signed in"
                        .into(),
                ),
                secret: true,
            }],
            ends_at: Some(ends_at),
        }))
    }

    /// Start `codex login --device-auth` and answer the step its page and code are.
    async fn device(&self, root: &Path) -> Result<Given, Error> {
        let running = self.start(root, &["--device-auth"], DEVICE_CODE)?;
        // The code is the line after the one naming it, once a line follows that.
        let printed = running
            .shows(LINK_WITHIN, |shown| {
                let url = login::link(shown)?;
                let rest = &shown[shown.find("one-time code")?..];
                let mut lines = rest.lines().skip(1).map(str::trim).filter(|line| !line.is_empty());
                let code = lines.next()?;
                lines.next()?;
                Some((url, code.to_owned()))
            })
            .await;
        let (url, code) = printed.ok_or_else(|| unprinted(&running))?;
        let ends_at = self.0.keep(running);
        Ok(Given::Next(Way {
            id: DEVICE_CODE.into(),
            label: format!("Enter code {code} on OpenAI's page"),
            about: Some(format!(
                "Open the page, sign in, and enter {code}; then continue. Never enter a code \
                 you did not start."
            )),
            link: Some(NeedLink {
                label: "Open OpenAI's page".into(),
                url,
                fills: None,
            }),
            fields: Vec::new(),
            ends_at: Some(ends_at),
        }))
    }

    /// `codex login` with `args` on a terminal of the driver's, in Codex's home `root`,
    /// waiting on the step `way`. A sign-in started before is ended.
    fn start(&self, root: &Path, args: &[&str], way: &str) -> Result<Login, Error> {
        self.0.end();
        let binary = crate::drive::binary()?;
        let mut command = helper(&binary);
        command.arg("login").args(args).env("CODEX_HOME", root);
        Login::start(command, way.into(), false)
            .map_err(|e| refused(format!("codex login did not start: {e}")))
    }
}

/// Give Codex an API key: `codex login --with-api-key`, reading it on standard input.
async fn api_key(root: &Path, key: &str) -> Result<(), Error> {
    if key.is_empty() {
        return Err(refused("Paste your OpenAI API key"));
    }
    let binary = crate::drive::binary()?;
    let mut command = helper(&binary);
    command
        .args(["login", "--with-api-key"])
        .env("CODEX_HOME", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| refused(format!("codex login did not start: {e}")))?;
    let mut stdin = child.stdin.take().ok_or_else(|| refused("codex login took no input"))?;
    let ended = tokio::time::timeout(KEY_WITHIN, async {
        stdin.write_all(format!("{key}\n").as_bytes()).await?;
        drop(stdin);
        child.wait_with_output().await
    })
    .await
    .map_err(|_| refused(format!("codex login did not end within {} s", KEY_WITHIN.as_secs())))?
    .map_err(|e| refused(format!("codex login: {e}")))?;
    if ended.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&ended.stderr);
    Err(refused(
        said.lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .map_or_else(|| format!("codex login ended with {}", ended.status), str::to_owned),
    ))
}

/// Why a started login gave no link: its own last line, where it printed one.
fn unprinted(running: &Login) -> Error {
    refused(running.last_line().unwrap_or_else(|| {
        format!("codex login printed no sign-in link within {} s", LINK_WITHIN.as_secs())
    }))
}

fn done(line: &str) -> Given {
    Given::Done {
        line: line.into(),
        env: Default::default(),
    }
}

fn gone(line: impl Into<String>) -> Error {
    Error::refused(refusal::NOT_FOUND, line)
}

fn refused(line: impl Into<String>) -> Error {
    Error::refused(refusal::REFUSED, line)
}
