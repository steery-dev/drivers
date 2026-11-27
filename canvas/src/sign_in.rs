//! The one need the canvas has (PROTOCOL.md §4.1, Needs): a sign-in to Claude, where
//! `canvas models` says `signed_in: false`. The canvas reads Claude Code's own variables, so
//! its person gives it in one of Claude Code's three ways:
//!
//! - A Claude plan: Claude Code's own `claude auth login`, run unmodified on a terminal of
//!   the driver's ([`steery_driver_common::login`]). The canvas calls Claude through Claude
//!   Code, which keeps what its sign-in gives. Offered only where Claude Code is here.
//! - An Anthropic API key, kept by the runtime as `ANTHROPIC_API_KEY` on every host.
//! - A company's gateway: `ANTHROPIC_BASE_URL`, and its key as `ANTHROPIC_AUTH_TOKEN` (sent as
//!   `Authorization: Bearer`) or `ANTHROPIC_API_KEY` (sent as `x-api-key`), once one
//!   `GET /v1/models` with it did not refuse it ([`steery_driver_common::gateway::check`]).
//!
//! A key or a gateway outranks a plan in the canvas, so from protocol 15 each `done` unsets
//! the variables of the ways it replaces.
//!
//! `claude auth login` opens its loopback link through `$BROWSER` and prints a manual one.
//! The driver's `$BROWSER` records the link it is given, and the card shows that one: the
//! Steery app catches its return and fills the field, and the driver fetches that address
//! inside this host, where Claude Code's own listener completes the sign-in.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use steery_driver_common::gateway::{self, Checked};
use steery_driver_common::login::{self, Login, Waiting};
use steery_driver_common::{anthropic, program};
use steery_sdk::wire::harness::{Field, Give, Given, Need, NeedLink, Way};
use steery_sdk::{Error, refusal};

/// The need, by its id in every plan.
pub const NEED: &str = "sign-in";
/// Claude Code's own sign-in, and the step it asks next once started.
const CLAUDE_SIGN_IN: &str = "claude-sign-in";
const CLAUDE_SIGN_IN_RETURN: &str = "claude-sign-in-return";
/// The field that step's return fills, or its person pastes into.
const ADDRESS: &str = "address";

/// The variables of a gateway: its address, and its key sent as `Authorization: Bearer`.
const BASE_URL: &str = "ANTHROPIC_BASE_URL";
const AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";
/// The gateway way's fields.
const GATEWAY_URL: &str = "url";
const GATEWAY_KEY: &str = "key";
const GATEWAY_HEADER: &str = "header";
/// What a gateway's key is tried on, and the `anthropic-version` the canvas sends it.
const CHECKED_PATH: &str = "/v1/models";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// How long `claude auth login` has to call `$BROWSER` or print its link.
const LINK_WITHIN: Duration = Duration::from_secs(15);
/// How long `$BROWSER` may still be called once the manual link is printed.
const BROWSER_AFTER: Duration = Duration::from_secs(3);

/// The need, where the canvas is here and not signed in: `line` is the canvas's own why.
pub fn need(line: Option<String>, claude: bool) -> Need {
    let mut ways = Vec::new();
    if claude {
        ways.push(Way {
            id: CLAUDE_SIGN_IN.into(),
            label: "Sign in with your Claude plan".into(),
            about: Some(
                "Claude Code's own sign-in. The canvas calls Claude through Claude Code. This \
                 host keeps it. Each new host asks again."
                    .into(),
            ),
            link: None,
            fields: Vec::new(),
            ends_at: None,
        });
    }
    ways.push(anthropic::way(
        "Your Claude Console account pays for each token, on every host. The canvas uses the \
         key in place of a Claude plan."
            .into(),
    ));
    ways.push(gateway_way());
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

fn gateway_way() -> Way {
    Way {
        id: gateway::WAY.into(),
        label: "Use your company's Claude gateway".into(),
        about: Some(
            "For a company that sends Claude through its own gateway. Every host uses this \
             address and key."
                .into(),
        ),
        link: None,
        fields: vec![
            Field {
                name: GATEWAY_URL.into(),
                label: "Gateway address".into(),
                about: Some("For example https://llm-gateway.example.com".into()),
                secret: false,
            },
            Field {
                name: GATEWAY_KEY.into(),
                label: "Gateway key".into(),
                about: Some(
                    "The key or token of your gateway. Type any word if it asks for none.".into(),
                ),
                secret: true,
            },
            Field {
                name: GATEWAY_HEADER.into(),
                label: "Key header".into(),
                about: Some(
                    "Empty: Authorization: Bearer. Type x-api-key to send the key in that header."
                        .into(),
                ),
                secret: false,
            },
        ],
        ends_at: None,
    }
}

/// The names `unsets` (protocol 15 or newer) gives, else none.
fn unset(unsets: bool, names: &[&str]) -> Vec<String> {
    match unsets {
        true => names.iter().map(|&name| name.to_owned()).collect(),
        false => Vec::new(),
    }
}

/// The `done` of the plan's own sign-in: the key and the gateway are unset, since the
/// canvas uses them before a plan.
fn signed_in(unsets: bool) -> Given {
    Given::Done {
        line: "The canvas runs on your Claude plan on this host".into(),
        env: BTreeMap::new(),
        unset: unset(unsets, &[anthropic::VARIABLE, AUTH_TOKEN, BASE_URL]),
    }
}

/// The gateway's way: the address and the key as the canvas's variables, once the gateway
/// did not refuse the key. A gateway that said nothing of it keeps it, and the line says so.
async fn gateway_given(values: &BTreeMap<String, String>, unsets: bool) -> Result<Given, Error> {
    let value = |name: &str| values.get(name).map_or("", |value| value.trim());
    let address = gateway::address(value(GATEWAY_URL), "the canvas").map_err(refused)?;
    let key = value(GATEWAY_KEY);
    if key.is_empty() {
        return Err(refused("Type the key of your gateway"));
    }
    if key.chars().any(char::is_control) {
        return Err(refused("The key of your gateway has no line break"));
    }
    let (variable, other, header) = match value(GATEWAY_HEADER).to_ascii_lowercase().as_str() {
        "" | "bearer" | "authorization" | "authorization: bearer" => (
            AUTH_TOKEN,
            anthropic::VARIABLE,
            ("Authorization", format!("Bearer {key}")),
        ),
        "x-api-key" => (anthropic::VARIABLE, AUTH_TOKEN, ("x-api-key", key.to_owned())),
        given => {
            return Err(refused(format!(
                "The key header is empty, bearer or x-api-key, not {given}"
            )));
        }
    };
    let checked = gateway::check(
        &address,
        CHECKED_PATH,
        &[
            (header.0, header.1.as_str()),
            ("anthropic-version", ANTHROPIC_VERSION),
        ],
        key,
    )
    .await;
    let line = match checked {
        Checked::Refused { status, said } => {
            let said = said.map_or(String::new(), |said| {
                format!(": {}", said.trim_end_matches('.'))
            });
            return Err(refused(format!(
                "The gateway at {address} refused your key (HTTP {status}){said}. Check the key \
                 and the key header."
            )));
        }
        Checked::Taken => format!("The canvas uses your company's gateway at {address}"),
        Checked::Unchecked(why) => format!(
            "The canvas uses your company's gateway at {address}. Steery did not check the key: \
             {why}."
        ),
    };
    Ok(Given::Done {
        line,
        env: BTreeMap::from([
            (BASE_URL.to_owned(), address),
            (variable.to_owned(), key.to_owned()),
        ]),
        unset: unset(unsets, &[other]),
    })
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
                let outranked: &[&str] = if unsets { &[BASE_URL, AUTH_TOKEN] } else { &[] };
                anthropic::give(
                    &give.values,
                    "The canvas uses your Anthropic API key",
                    outranked,
                )?
            }
            gateway::WAY => {
                self.0.end();
                gateway_given(&give.values, unsets).await?
            }
            CLAUDE_SIGN_IN => self.start(home).await?,
            CLAUDE_SIGN_IN_RETURN => {
                let given = give.values.get(ADDRESS).map_or("", String::as_str);
                self.0
                    .give(CLAUDE_SIGN_IN_RETURN, given, "Claude Code")
                    .await?;
                signed_in(unsets)
            }
            way => return Err(gone(format!("the canvas has no sign-in way {way}"))),
        };
        if matches!(given, Given::Done { .. }) {
            crate::setup::SIGNED.forget();
        }
        Ok(given)
    }

    /// Start `claude auth login` with the driver's `$BROWSER`, and answer the step its link
    /// is: the loopback link it gave `$BROWSER`, else the manual one it printed.
    async fn start(&self, home: &Path) -> Result<Given, Error> {
        self.0.end();
        let binary =
            crate::home::claude(home).ok_or_else(|| gone("Claude Code is not installed here"))?;
        let browser = Browser::new()
            .map_err(|e| refused(format!("cannot make a $BROWSER for Claude Code: {e}")))?;
        let mut command = program::helper(&binary);
        command
            .args(["auth", "login"])
            .env("BROWSER", browser.program());
        let mut running = Login::start(command, CLAUDE_SIGN_IN_RETURN.into(), true)
            .map_err(|e| refused(format!("claude auth login did not start: {e}")))?;
        let began = Instant::now();
        let mut printed = None;
        let url = loop {
            if let Some(url) = browser.given() {
                break Some(url);
            }
            let now = Instant::now();
            if printed.is_none() && running.showed(login::link).is_some() {
                printed = Some(now);
            }
            if now >= began + LINK_WITHIN || printed.is_some_and(|at| now >= at + BROWSER_AFTER) {
                break running.showed(login::link);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        drop(browser);
        let Some(url) = url else {
            return Err(refused(running.last_line().unwrap_or_else(|| {
                format!(
                    "claude auth login gave no sign-in link within {} s",
                    LINK_WITHIN.as_secs()
                )
            })));
        };
        let returns = running.link(&url);
        let ends_at = self.0.keep(running);
        Ok(Given::Next(Way {
            id: CLAUDE_SIGN_IN_RETURN.into(),
            label: "Sign in to Claude".into(),
            about: Some(
                "Sign in on Claude's page. If the page ends on an address that does not load, \
                 paste that address here. If the page shows a code, paste the code here."
                    .into(),
            ),
            link: Some(NeedLink {
                label: "Sign in to Claude".into(),
                url,
                fills: returns.then(|| ADDRESS.into()),
            }),
            fields: vec![Field {
                name: ADDRESS.into(),
                label: "Address or code".into(),
                about: Some(
                    "The address where your browser ended, or the code that Claude's page \
                     shows. Leave it empty when you are signed in."
                        .into(),
                ),
                secret: true,
            }],
            ends_at: Some(ends_at),
        }))
    }
}

/// A `$BROWSER` of the driver's: a script in a folder of its own that records the link it
/// is given, and opens nothing. The folder goes once it is dropped.
struct Browser(PathBuf);

impl Browser {
    fn new() -> std::io::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let folder = std::env::temp_dir().join(format!(
            "steery-canvas-browser-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&folder)?;
        let browser = Self(folder);
        let at = format!(
            "'{}'",
            browser.0.display().to_string().replace('\'', r"'\''")
        );
        // Written whole, then moved, so it is never read half written.
        std::fs::write(
            browser.program(),
            format!(
                "#!/bin/sh\nprintf '%s' \"$1\" > {at}/given.tmp && mv {at}/given.tmp {at}/given\n"
            ),
        )?;
        std::fs::set_permissions(browser.program(), std::fs::Permissions::from_mode(0o700))?;
        Ok(browser)
    }

    fn program(&self) -> PathBuf {
        self.0.join("browser")
    }

    /// The link it was given, where it was.
    fn given(&self) -> Option<String> {
        let given = std::fs::read_to_string(self.0.join("given")).ok()?;
        given.starts_with("https://").then_some(given)
    }
}

impl Drop for Browser {
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
