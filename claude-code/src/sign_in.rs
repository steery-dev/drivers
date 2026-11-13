//! The one need Claude Code has (PROTOCOL.md §4.1, Needs): a sign-in, where it is found and
//! `claude auth status` says it is not signed in. Its person gives it through Claude Code's
//! own `claude auth login`, run unmodified on a terminal of the driver's
//! ([`steery_driver_common::login`]), with an Anthropic API key, which the runtime keeps
//! and sets as `ANTHROPIC_API_KEY` on every host, or with a company's own gateway
//! ([`crate::gateway`]). Claude Code keeps what its sign-in gives; the driver reads no token
//! and keeps none.
//!
//! `claude auth login` opens its loopback link (`redirect_uri=http://localhost:<port>/callback`)
//! through `$BROWSER` and prints a manual one whose page shows a code
//! (`redirect_uri=https://platform.claude.com/oauth/code/callback`; both Claude Code 2.1.317 on
//! macOS). The driver's `$BROWSER` records the link it is given, and the card shows that one:
//! the Steery app catches its return and fills the field, and the driver fetches that address
//! inside this host, where Claude Code's own listener completes the sign-in.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use steery_driver_common::anthropic;
use steery_driver_common::login::{self, Login, Waiting};
use steery_sdk::wire::harness::{Field, Give, Given, Need, NeedLink, Way};
use steery_sdk::{Error, refusal};

/// The need, by its id in every plan.
pub const NEED: &str = "sign-in";
/// Claude Code's own sign-in, and the step it asks next once started.
const CLAUDE_SIGN_IN: &str = "claude-sign-in";
const CLAUDE_SIGN_IN_RETURN: &str = "claude-sign-in-return";
/// The field that step's return fills, or its person pastes into.
const ADDRESS: &str = "address";

/// How long `claude auth login` has to call `$BROWSER` or print its link: it did both at
/// once (2.1.317).
const LINK_WITHIN: Duration = Duration::from_secs(15);
/// How long `$BROWSER` may still be called once the manual link is printed.
const BROWSER_AFTER: Duration = Duration::from_secs(3);

/// The need, where Claude Code is found here and not signed in.
pub fn need() -> Need {
    Need {
        id: NEED.into(),
        title: "Sign Claude Code in?".into(),
        line: "Claude Code is not signed in on this host. It cannot run a turn here.".into(),
        ways: vec![
            Way {
                id: CLAUDE_SIGN_IN.into(),
                label: "Sign in with your Claude plan".into(),
                about: Some(
                    "Claude Code's own sign-in. This host keeps it. Each new host asks again."
                        .into(),
                ),
                link: None,
                fields: Vec::new(),
                ends_at: None,
            },
            // Claude Code's docs (https://code.claude.com/docs/en/authentication.md,
            // "Authentication precedence"): in non-interactive mode (`-p`) the key is always
            // used when present, over a plan's sign-in; an interactive session asks once to
            // approve it. `claude auth status --json` counts it as signed in (`"authMethod":
            // "api_key"`, `"apiKeySource": "ANTHROPIC_API_KEY"`, Claude Code 2.1.310).
            anthropic::way(
                "Your Claude Console account pays for each token, on every host. Claude Code \
                 always uses the key in non-interactive runs, also when it is signed in to a plan."
                    .into(),
            ),
            crate::gateway::way(),
        ],
    }
}

/// The `done` of the plan's own sign-in. With `unsets`, the variables of the key (and its
/// marker) and the gateway are unset: Claude Code uses them before a plan's sign-in, so the
/// sign-in just made is used only where none is kept.
fn signed_in(unsets: bool) -> Given {
    Given::Done {
        line: "Claude Code is signed in on this host".into(),
        env: Default::default(),
        unset: match unsets {
            true => [
                anthropic::VARIABLE,
                crate::gateway::AUTH_TOKEN,
                crate::gateway::BASE_URL,
                KEY_MARKER,
            ]
            .map(str::to_owned)
            .into(),
            false => Vec::new(),
        },
    }
}

/// The sign-in a person asked to give again, where Claude Code is signed in already: the
/// same ways, in a question about a change. The key's way says that the key is used in place
/// of the plan, which stays signed in.
pub fn change() -> Need {
    let mut need = need();
    for way in &mut need.ways {
        if way.id == anthropic::WAY {
            way.about = way
                .about
                .take()
                .map(|about| format!("{about}{USED_INSTEAD}"));
        }
    }
    Need {
        title: "Change how Claude Code signs in?".into(),
        line: "Claude Code is signed in on this host. Pick a new way to sign in.".into(),
        ..need
    }
}

/// What the key's way adds to its `about` in a change: the key is used in place of the
/// plan on this host, and the plan stays signed in.
const USED_INSTEAD: &str =
    " Claude Code uses the key in place of your plan on this host. The plan stays signed in.";

/// The variable that goes with a key given through Steery. It says that the person gave this
/// key here, so turns use it also where Claude Code is signed in to a plan
/// ([`crate::turn`]). Its name does not start with `STEERY_`: the turn moves every such
/// variable of the driver's environment into the session's settings, and this one is for the
/// driver alone, so the session's processes do not carry it.
pub const KEY_MARKER: &str = "CLAUDE_KEY_FROM_STEERY";

/// The key's way: the key is checked and given, with [`KEY_MARKER`]. Nothing is signed out:
/// a plan stays signed in, and the marker makes turns use the key.
fn key_given(
    values: &std::collections::BTreeMap<String, String>,
    unsets: bool,
) -> Result<Given, Error> {
    let unset: &[&str] = match unsets {
        true => &[crate::gateway::BASE_URL, crate::gateway::AUTH_TOKEN],
        false => &[],
    };
    let mut given = anthropic::give(values, "Claude Code uses your Anthropic API key", unset)?;
    if let Given::Done { env, .. } = &mut given {
        env.insert(KEY_MARKER.to_owned(), "1".to_owned());
    }
    Ok(given)
}

/// The sign-in started here and waiting for its person.
#[derive(Default, Clone)]
pub struct SignIns(Waiting);

impl SignIns {
    /// `needs/give`. `unsets` is whether the driver picked protocol 15 or newer, where a
    /// `done` names the variables of another way that it outranks. A `done` forgets the last
    /// answer of the sign-in probe: the next probe with no answer plans no need, not the
    /// sign-in just made.
    pub async fn give(&self, give: Give, unsets: bool) -> Result<Given, Error> {
        let given = self.step(give, unsets).await?;
        if matches!(given, Given::Done { .. }) {
            crate::setup::SIGNED.forget();
        }
        Ok(given)
    }

    async fn step(&self, give: Give, unsets: bool) -> Result<Given, Error> {
        if give.need != NEED {
            return Err(gone(format!("Claude Code has no need {} here", give.need)));
        }
        match give.way.as_str() {
            anthropic::WAY => {
                self.0.end();
                key_given(&give.values, unsets)
            }
            crate::gateway::WAY => {
                self.0.end();
                crate::gateway::give(&give.values, unsets)
            }
            CLAUDE_SIGN_IN => self.start().await,
            CLAUDE_SIGN_IN_RETURN => {
                let given = give.values.get(ADDRESS).map_or("", String::as_str);
                self.0
                    .give(CLAUDE_SIGN_IN_RETURN, given, "Claude Code")
                    .await?;
                Ok(signed_in(unsets))
            }
            way => Err(gone(format!("Claude Code has no sign-in way {way}"))),
        }
    }

    /// Start `claude auth login` with the driver's `$BROWSER`, and answer the step its link
    /// is: the loopback link it gave `$BROWSER`, else the manual one it printed. A sign-in
    /// started before is ended.
    async fn start(&self) -> Result<Given, Error> {
        self.0.end();
        let binary = crate::binary().ok_or_else(|| gone("Claude Code is not installed here"))?;
        let browser = Browser::new()
            .map_err(|e| refused(format!("cannot make a $BROWSER for Claude Code: {e}")))?;
        let mut command = steery_driver_common::program::helper(&binary);
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
            "steery-browser-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&folder)?;
        let browser = Self(folder);
        let at = quoted(&browser.0);
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

/// `path` as one word of a shell's.
fn quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

fn gone(line: impl Into<String>) -> Error {
    Error::refused(refusal::NOT_FOUND, line)
}

fn refused(line: impl Into<String>) -> Error {
    Error::refused(refusal::REFUSED, line)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn asked(way: &str, values: &[(&str, &str)]) -> Give {
        Give {
            need: NEED.into(),
            way: way.into(),
            values: values
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    /// What a way's `done` unsets, given at protocol `unsets`.
    async fn unset(way: &str, values: &[(&str, &str)], unsets: bool) -> Vec<String> {
        match SignIns::default().step(asked(way, values), unsets).await {
            Ok(Given::Done { unset, .. }) => unset,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn the_key_is_given_with_the_marker_and_signs_nothing_out() {
        match SignIns::default()
            .step(asked(anthropic::WAY, KEY), true)
            .await
        {
            Ok(Given::Done { env, .. }) => {
                assert_eq!(env[anthropic::VARIABLE], "sk-ant-api03-abc");
                assert_eq!(env[KEY_MARKER], "1");
                assert_eq!(env.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_wrong_key_is_refused() {
        let given = SignIns::default()
            .step(asked(anthropic::WAY, &[("key", "oops")]), true)
            .await;
        assert!(given.is_err());
    }

    #[test]
    fn the_marker_is_no_variable_of_steery_s_own() {
        assert!(!KEY_MARKER.starts_with("STEERY"));
    }

    const KEY: &[(&str, &str)] = &[("key", "sk-ant-api03-abc")];
    const GATEWAY: &[(&str, &str)] = &[("url", "https://gw.example.com"), ("key", "k")];

    #[tokio::test]
    async fn an_api_key_unsets_the_gateway_from_protocol_15() {
        assert_eq!(
            unset(anthropic::WAY, KEY, true).await,
            ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN"]
        );
        assert!(unset(anthropic::WAY, KEY, false).await.is_empty());
    }

    #[tokio::test]
    async fn a_gateway_unsets_the_variable_of_the_other_header_from_protocol_15() {
        assert_eq!(
            unset(crate::gateway::WAY, GATEWAY, true).await,
            ["ANTHROPIC_API_KEY", KEY_MARKER]
        );
        let x_api_key = [GATEWAY, &[("header", "x-api-key")]].concat();
        assert_eq!(
            unset(crate::gateway::WAY, &x_api_key, true).await,
            ["ANTHROPIC_AUTH_TOKEN", KEY_MARKER]
        );
        assert!(unset(crate::gateway::WAY, GATEWAY, false).await.is_empty());
        assert!(
            unset(crate::gateway::WAY, &x_api_key, false)
                .await
                .is_empty()
        );
    }

    #[test]
    fn the_plan_sign_in_unsets_the_key_and_the_gateway_from_protocol_15() {
        let unset = |unsets| match signed_in(unsets) {
            Given::Done { unset, env, .. } => {
                assert!(env.is_empty());
                unset
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(
            unset(true),
            [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_BASE_URL",
                KEY_MARKER
            ]
        );
        assert!(unset(false).is_empty());
    }

    #[test]
    fn a_change_keeps_the_ways_and_says_what_is_there() {
        let (lacking, change) = (need(), change());
        assert_eq!(change.id, lacking.id);
        assert_eq!(change.title, "Change how Claude Code signs in?");
        assert_eq!(
            change.line,
            "Claude Code is signed in on this host. Pick a new way to sign in."
        );
        let ids = |need: &Need| {
            need.ways
                .iter()
                .map(|way| way.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&change), ids(&lacking));
    }

    /// Only the change says that the key is used in place of the plan, which stays signed
    /// in, and only on the key's way.
    #[test]
    fn only_the_change_says_that_the_key_replaces_the_plan_and_signs_nothing_out() {
        let about = |need: &Need, id: &str| {
            need.ways
                .iter()
                .find(|way| way.id == id)
                .and_then(|way| way.about.clone())
                .unwrap_or_default()
        };
        let (lacking, change) = (need(), change());
        assert!(!about(&lacking, anthropic::WAY).contains(USED_INSTEAD));
        assert!(about(&change, anthropic::WAY).ends_with(USED_INSTEAD));
        assert!(USED_INSTEAD.contains("stays signed in"));
        for (a, b) in lacking.ways.iter().zip(&change.ways) {
            if a.id != anthropic::WAY {
                assert_eq!(a, b);
            }
        }
    }
}
