//! The one need Claude Code has (PROTOCOL.md §4.1, Needs): a sign-in, where it is found and
//! `claude auth status` says it is not signed in. Its person gives it through Claude Code's
//! own `claude auth login`, run unmodified on a terminal of the driver's, with an Anthropic
//! API key, which the runtime keeps and sets as `ANTHROPIC_API_KEY` on every host, or with a
//! company's own gateway ([`steery_driver_common::claude`], which says how each works).
//! Claude Code keeps what its sign-in gives; the driver reads no token and keeps none.

use steery_driver_common::login::Waiting;
use steery_driver_common::{anthropic, claude, gateway};
use steery_sdk::wire::harness::{Give, Given, Need};
use steery_sdk::{Error, refusal};

/// The need, by its id in every plan.
pub const NEED: &str = "sign-in";

/// The need, where Claude Code is found here and not signed in.
pub fn need() -> Need {
    Need {
        id: NEED.into(),
        title: "Sign Claude Code in?".into(),
        line: "Claude Code is not signed in on this host. It cannot run a turn here.".into(),
        ways: vec![
            claude::plan_way(
                "Claude Code's own sign-in. This host keeps it. Each new host asks again.".into(),
            ),
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
            claude::gateway_way(
                "For a company that sends Claude through its own gateway, as Claude Desktop \
                 can. Every host uses this address and key."
                    .into(),
            ),
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
                claude::AUTH_TOKEN,
                claude::BASE_URL,
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
        true => &[claude::BASE_URL, claude::AUTH_TOKEN],
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
            gateway::WAY => {
                self.0.end();
                let unset: Option<&[&str]> = unsets.then_some(&[KEY_MARKER][..]);
                claude::gateway_given(&give.values, "Claude Code", unset).await
            }
            claude::PLAN_WAY => {
                self.0.end();
                let binary =
                    crate::binary().ok_or_else(|| gone("Claude Code is not installed here"))?;
                claude::start(&binary, &self.0).await
            }
            claude::PLAN_RETURN => {
                claude::returned(&self.0, &give.values).await?;
                Ok(signed_in(unsets))
            }
            way => Err(gone(format!("Claude Code has no sign-in way {way}"))),
        }
    }
}

fn gone(line: impl Into<String>) -> Error {
    Error::refused(refusal::NOT_FOUND, line)
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

    /// A gateway on this computer that takes every key, so a way's `done` is the gateway
    /// way's own and no test reaches a network: its address.
    fn gateway_taking_keys() -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let _ = stream.read(&mut [0u8; 2048]);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n{}");
            }
        });
        address
    }

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
        let url = gateway_taking_keys();
        let bearer = [("url", url.as_str()), ("key", "k")];
        assert_eq!(
            unset(gateway::WAY, &bearer, true).await,
            ["ANTHROPIC_API_KEY", KEY_MARKER]
        );
        let x_api_key = [&bearer[..], &[("header", "x-api-key")]].concat();
        assert_eq!(
            unset(gateway::WAY, &x_api_key, true).await,
            ["ANTHROPIC_AUTH_TOKEN", KEY_MARKER]
        );
        assert!(unset(gateway::WAY, &bearer, false).await.is_empty());
        assert!(
            unset(gateway::WAY, &x_api_key, false)
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
