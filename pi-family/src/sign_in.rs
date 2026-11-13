//! The one need the harness has (PROTOCOL.md §4.1, Needs): a model provider signed in, where
//! it lists no model. Its person gives it with an Anthropic API key, which the runtime keeps
//! and sets as `ANTHROPIC_API_KEY` on every host, or, in omp, through omp's own `omp login
//! <provider>`, run unmodified on a terminal of the driver's
//! ([`steery_driver_common::login`]); omp keeps what that gives, the driver nothing. Or, in
//! omp, with a company's own gateway: omp reads its address from `ANTHROPIC_BASE_URL` and
//! its key from `ANTHROPIC_API_KEY`, and the runtime sets both on every host. pi has no
//! login command (its `/login` is typed inside it), and it reads a gateway's address only
//! from the `baseUrl` of its `models.json`, which the driver does not write. So pi has the
//! key alone.
//!
//! omp's and pi's own Claude sign-in is none of its ways: it signs a third-party app in to a
//! Claude plan, which Anthropic's terms forbid.

use steery_driver_common::login::{self, Login, Waiting};
use steery_driver_common::program::helper;
use steery_driver_common::{anthropic, gateway};
use steery_sdk::wire::harness::{Field, Give, Given, Need, NeedLink, Way};
use steery_sdk::{Error, refusal};

use crate::Harness;

/// The need, by its id in every plan.
pub const NEED: &str = "sign-in";
/// The field a return fills, or its person pastes into.
const ADDRESS: &str = "address";
/// The variable that names the gateway's address in omp.
const BASE_URL: &str = "ANTHROPIC_BASE_URL";
/// The fields of the gateway way, by name.
const GATEWAY_URL: &str = "url";
const GATEWAY_KEY: &str = "key";

/// How long `omp login` has to print its link: within 3 s each (18.12.6), a device code's
/// request to its vendor among them.
const LINK_WITHIN: std::time::Duration = std::time::Duration::from_secs(15);

/// How an omp provider's sign-in comes back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Returns {
    /// The browser returns to an address: a loopback one omp listens on, or one of an app
    /// (`vscode://`, `zcode://`) whose address its person pastes; omp reads an address or
    /// a code on its prompt.
    Address,
    /// omp asks the vendor until its person signed in on its page, typing the code omp
    /// shows, where it shows one.
    Code,
}

/// An OAuth provider `omp login` signs in to, by its id there.
struct Provider {
    id: &'static str,
    label: &'static str,
    returns: Returns,
    /// A question omp asks before its link, answered with Enter (its default).
    asks_first: Option<&'static str>,
}

/// omp's OAuth providers but Anthropic's, as omp 18.12.6 signs in to each. omp lists its
/// providers' ids and names (RPC `get_login_providers`, the `omp login` picker) but not how
/// each signs in: most of them only read an API key on omp's prompt. So the list is the
/// driver's. Left out: `perplexity` and `snowflake`, which ask an email or an account
/// first. `github-copilot` asks a GitHub Enterprise domain first, answered with Enter:
/// github.com.
const PROVIDERS: [Provider; 17] = [
    address("openai-codex", "ChatGPT Plus/Pro (Codex)"),
    address("google-gemini-cli", "Google Cloud Code Assist (Gemini CLI)"),
    address("google-antigravity", "Google Antigravity"),
    address("openrouter", "OpenRouter"),
    address("devin", "Devin"),
    address("stencil", "Stencil"),
    address("gitlab-duo", "GitLab Duo"),
    address("gitlab-duo-agent", "GitLab Duo Agent"),
    address("zai-coding-plan", "Z.AI GLM Coding Plan"),
    code("openai-codex-device", "ChatGPT Plus/Pro (Codex) by a code"),
    Provider {
        id: "github-copilot",
        label: "GitHub Copilot",
        returns: Returns::Code,
        asks_first: Some("GitHub Enterprise"),
    },
    code("cursor", "Cursor"),
    code("kimi-code", "Kimi Code"),
    code("xai-oauth", "xAI Grok (SuperGrok or X Premium+)"),
    code("muse-code", "Muse Code"),
    code("factory-droid", "Factory Droid"),
    code("kilo", "Kilo Gateway"),
];

const fn address(id: &'static str, label: &'static str) -> Provider {
    Provider {
        id,
        label,
        returns: Returns::Address,
        asks_first: None,
    }
}

const fn code(id: &'static str, label: &'static str) -> Provider {
    Provider {
        id,
        label,
        returns: Returns::Code,
        asks_first: None,
    }
}

impl Provider {
    /// The step its started sign-in asks next.
    fn step(&self) -> String {
        match self.returns {
            Returns::Address => format!("{}-return", self.id),
            Returns::Code => format!("{}-code", self.id),
        }
    }
}

/// The need, where the harness is found here and lists no model: in omp a way per OAuth
/// provider, those whose return the Steery app catches first, then the key.
pub fn need(harness: Harness) -> Need {
    let label = harness.label();
    let key = anthropic::way(format!(
        "Your Claude Console account pays for each token, on every host. {label} then runs \
         Claude models through Anthropic's API."
    ));
    match harness {
        Harness::Pi => Need {
            id: NEED.into(),
            title: format!("Give {label} an Anthropic API key?"),
            line: format!(
                "{label} has no model provider signed in on this host. It cannot run a turn here."
            ),
            ways: vec![key],
        },
        Harness::Omp => Need {
            id: NEED.into(),
            title: format!("Sign {label} in to a model provider?"),
            line: format!(
                "{label} has no model provider signed in on this host. It cannot run a turn here."
            ),
            ways: PROVIDERS
                .iter()
                .map(|provider| Way {
                    id: provider.id.into(),
                    label: format!("Sign in with {}", provider.label),
                    about: Some(
                        "omp's own sign-in. This host keeps it. Each new host asks again.".into(),
                    ),
                    link: None,
                    fields: Vec::new(),
                    ends_at: None,
                })
                .chain([key, gateway_way()])
                .collect(),
        },
    }
}

/// The need that a person asked to give again, where the harness lists a model already: the
/// same ways, in a question about a change.
pub fn change(harness: Harness) -> Need {
    let label = harness.label();
    Need {
        title: match harness {
            Harness::Pi => format!("Change how {label} gets its Anthropic API key?"),
            Harness::Omp => format!("Change how {label} signs in to a model provider?"),
        },
        line: format!(
            "{label} has a model provider signed in on this host. Pick a new way to sign in."
        ),
        ..need(harness)
    }
}

/// The sign-in need of the plan, where the harness is found: the one that lacks where
/// `lacks`; else, where the person `ask`ed to give it again, the sign-in as a change.
pub fn sign_in_need(harness: Harness, lacks: bool, ask: bool) -> Option<Need> {
    match (lacks, ask) {
        (true, _) => Some(need(harness)),
        (false, true) => Some(change(harness)),
        (false, false) => None,
    }
}

/// The way of a company's own gateway, for omp. omp sends the key in two headers at once
/// (`Authorization: Bearer` and `x-api-key`), so the way has no field for the header.
fn gateway_way() -> Way {
    Way {
        id: gateway::WAY.into(),
        label: "Use your company's Claude gateway".into(),
        about: Some(
            "For a company that sends Claude through its own gateway. Every host uses this \
             address and key. omp sends the key in the Authorization and x-api-key headers."
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
        ],
        ends_at: None,
    }
}

/// What `values` give by [`gateway_way`]: the address and the key as omp's variables.
fn gateway_give(values: &std::collections::BTreeMap<String, String>) -> Result<Given, Error> {
    let value = |name: &str| values.get(name).map_or("", |value| value.trim());
    let address = gateway::address(value(GATEWAY_URL), "omp").map_err(refused)?;
    let key = value(GATEWAY_KEY);
    if key.is_empty() {
        return Err(refused("Type the key of your gateway"));
    }
    Ok(Given::Done {
        line: format!("omp uses your company's gateway at {address}"),
        env: std::collections::BTreeMap::from([
            (BASE_URL.to_owned(), address),
            (anthropic::VARIABLE.to_owned(), key.to_owned()),
        ]),
        // It sets both variables omp reads: no other way's variable is left to unset.
        unset: Vec::new(),
    })
}

/// The `done` of a provider's sign-in. omp keeps what that gives itself: no variable is
/// given or unset.
fn signed_in(label: &str) -> Given {
    Given::Done {
        line: format!("{label} is signed in on this host"),
        env: Default::default(),
        unset: Vec::new(),
    }
}

/// The sign-in started here and waiting for its person.
#[derive(Default, Clone)]
pub struct SignIns(Waiting);

impl SignIns {
    /// `needs/give`. `unsets` is whether the driver picked protocol 15 or newer, where a
    /// `done` names the variables of another way that it outranks.
    pub async fn give(&self, harness: Harness, give: Give, unsets: bool) -> Result<Given, Error> {
        let label = harness.label();
        if give.need != NEED {
            return Err(gone(format!("{label} has no need {} here", give.need)));
        }
        if give.way == anthropic::WAY {
            self.0.end();
            let line = format!("{label} runs Claude with your Anthropic API key");
            let unset: &[&str] = match (unsets, harness) {
                (true, Harness::Omp) => &[BASE_URL],
                _ => &[],
            };
            return anthropic::give(&give.values, &line, unset);
        }
        if give.way == gateway::WAY && harness == Harness::Omp {
            self.0.end();
            return gateway_give(&give.values);
        }
        if harness == Harness::Omp {
            if let Some(provider) = PROVIDERS.iter().find(|provider| provider.id == give.way) {
                return self.start(provider).await;
            }
            if self.0.waits_on(&give.way) {
                let given = give.values.get(ADDRESS).map_or("", String::as_str);
                self.0.give(&give.way, given, label).await?;
                return Ok(signed_in(label));
            }
        }
        Err(gone(format!(
            "{label} has no need {} with the way {} here",
            give.need, give.way
        )))
    }

    /// Start `omp login <provider>` and answer the step its link is. A sign-in started
    /// before is ended.
    async fn start(&self, provider: &Provider) -> Result<Given, Error> {
        self.0.end();
        let binary = Harness::Omp
            .binary()
            .ok_or_else(|| gone("omp is not installed here"))?;
        let mut command = helper(&binary);
        command.args(["login", provider.id]);
        let address = provider.returns == Returns::Address;
        let mut running = Login::start(command, provider.step(), address)
            .map_err(|e| refused(format!("omp login did not start: {e}")))?;
        if let Some(question) = provider.asks_first {
            running
                .shows(LINK_WITHIN, |shown| shown.contains(question).then_some(()))
                .await
                .ok_or_else(|| unprinted(&running))?;
            running
                .type_line("")
                .map_err(|e| refused(format!("omp login did not take the answer: {e}")))?;
        }
        // omp prints its link on the line after this one, then what to do with it, and
        // then that it waits (GitHub Copilot's code is its last line).
        let printed = running
            .shows(LINK_WITHIN, |shown| {
                let rest = &shown[shown.find("Open this URL in your browser:")?..];
                let url = login::link(rest)?;
                let code = login::after(rest, "Enter code:");
                (code.is_some() || rest.contains("Waiting for")).then_some((url, code))
            })
            .await;
        let (url, code) = printed.ok_or_else(|| unprinted(&running))?;
        let returns = running.link(&url);
        let ends_at = self.0.keep(running);
        let name = provider.label;
        Ok(Given::Next(match provider.returns {
            Returns::Address => Way {
                id: provider.step(),
                label: format!("Sign in to {name}"),
                about: Some(
                    "Sign in on its page. If the page ends on an address that does not load, \
                     paste that address here. If the page shows a code, paste the code here."
                        .into(),
                ),
                link: Some(NeedLink {
                    label: format!("Sign in to {name}"),
                    url,
                    fills: returns.then(|| ADDRESS.into()),
                }),
                fields: vec![Field {
                    name: ADDRESS.into(),
                    label: "Address or code".into(),
                    about: Some(
                        "The address where your browser ended, or the code that its page shows. \
                         Leave it empty when you are signed in."
                            .into(),
                    ),
                    secret: true,
                }],
                ends_at: Some(ends_at),
            },
            Returns::Code => Way {
                id: provider.step(),
                label: match &code {
                    Some(code) => format!("Enter code {code} for {name}"),
                    None => format!("Sign in to {name}"),
                },
                about: Some(match &code {
                    Some(code) => {
                        format!("Open the page, sign in, and enter {code}. Then continue.")
                    }
                    None => "Open the page and sign in. Then continue.".into(),
                }),
                link: Some(NeedLink {
                    label: "Open the sign-in page".into(),
                    url,
                    fills: None,
                }),
                fields: Vec::new(),
                ends_at: Some(ends_at),
            },
        }))
    }
}

/// Why a started login gave no link: its own last line, where it printed one.
fn unprinted(running: &Login) -> Error {
    refused(running.last_line().unwrap_or_else(|| {
        format!(
            "omp login printed no sign-in link within {} s",
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
    use std::collections::BTreeMap;

    use super::*;

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn env(pairs: &[(&str, &str)]) -> Result<BTreeMap<String, String>, String> {
        match gateway_give(&values(pairs)) {
            Ok(Given::Done { env, .. }) => Ok(env),
            Ok(Given::Next(way)) => Err(format!("a next step {}", way.id)),
            Err(error) => Err(error.to_string()),
        }
    }

    #[test]
    fn the_gateway_is_the_base_url_and_the_api_key() {
        let env = env(&[
            (GATEWAY_URL, " https://gw.example.com/anthropic/ "),
            (GATEWAY_KEY, "tok"),
        ])
        .unwrap();
        assert_eq!(
            env,
            BTreeMap::from([
                (
                    BASE_URL.to_owned(),
                    "https://gw.example.com/anthropic".to_owned()
                ),
                (anthropic::VARIABLE.to_owned(), "tok".to_owned()),
            ])
        );
    }

    #[test]
    fn a_bad_address_or_a_missing_key_is_refused() {
        let key = (GATEWAY_KEY, "k");
        for no in [
            "http://gw.example.com",
            "https://gw.example.com/v1",
            "https://user:pw@gw.example.com",
            "https://gw.example.com?x=1",
            "",
        ] {
            assert!(env(&[(GATEWAY_URL, no), key]).is_err(), "{no}");
        }
        assert!(env(&[(GATEWAY_URL, "http://localhost:4000")]).is_err());
        assert!(env(&[(GATEWAY_URL, "http://localhost:4000"), key]).is_ok());
    }

    #[test]
    fn only_omp_offers_the_gateway() {
        let ids =
            |harness| -> Vec<String> { need(harness).ways.into_iter().map(|way| way.id).collect() };
        assert!(ids(Harness::Omp).iter().any(|id| id == gateway::WAY));
        assert_eq!(ids(Harness::Pi), [anthropic::WAY]);
    }

    const KEY: &[(&str, &str)] = &[("key", "sk-ant-api03-abc")];
    const GATEWAY: &[(&str, &str)] = &[(GATEWAY_URL, "https://gw.example.com"), (GATEWAY_KEY, "k")];

    fn give_of(way: &str, pairs: &[(&str, &str)]) -> Give {
        Give {
            need: NEED.into(),
            way: way.into(),
            values: values(pairs),
        }
    }

    /// What a way's `done` unsets, given where the driver picked 15 (`unsets`) or not.
    async fn unset(
        harness: Harness,
        way: &str,
        pairs: &[(&str, &str)],
        unsets: bool,
    ) -> Vec<String> {
        match SignIns::default()
            .give(harness, give_of(way, pairs), unsets)
            .await
        {
            Ok(Given::Done { unset, .. }) => unset,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn pi_refuses_the_gateway_and_omp_gives_it() {
        let signs = SignIns::default();
        let give = give_of(gateway::WAY, GATEWAY);
        assert!(signs.give(Harness::Pi, give.clone(), true).await.is_err());
        let given = signs.give(Harness::Omp, give, true).await.unwrap();
        assert!(
            matches!(given, Given::Done { env, .. } if env[BASE_URL] == "https://gw.example.com")
        );
    }

    /// An API key outranks a gateway that omp kept before, so omp unsets its address; pi
    /// has no gateway. The gateway sets both variables of omp and unsets none.
    #[tokio::test]
    async fn only_the_omp_api_key_unsets_the_gateway_address_from_protocol_15() {
        assert_eq!(
            unset(Harness::Omp, anthropic::WAY, KEY, true).await,
            [BASE_URL]
        );
        assert!(
            unset(Harness::Pi, anthropic::WAY, KEY, true)
                .await
                .is_empty()
        );
        assert!(
            unset(Harness::Omp, anthropic::WAY, KEY, false)
                .await
                .is_empty()
        );
        assert!(
            unset(Harness::Omp, gateway::WAY, GATEWAY, true)
                .await
                .is_empty()
        );
        assert!(
            unset(Harness::Omp, gateway::WAY, GATEWAY, false)
                .await
                .is_empty()
        );
    }

    /// A provider's sign-in is kept by omp itself: no variable is unset.
    #[test]
    fn a_provider_sign_in_unsets_nothing() {
        let given = signed_in("omp");
        assert!(
            matches!(given, Given::Done { env, unset, .. } if env.is_empty() && unset.is_empty())
        );
    }

    /// A harness that lists a model plans the sign-in only where the person asked, as a
    /// change; one that lists none plans the need that lacks, asked or not.
    #[test]
    fn the_plan_has_the_sign_in_as_a_change_only_where_asked() {
        for harness in [Harness::Omp, Harness::Pi] {
            let asked = sign_in_need(harness, false, true).expect("asked");
            assert_eq!(asked.id, NEED);
            assert_eq!(asked.title, change(harness).title);
            assert!(sign_in_need(harness, false, false).is_none());
            for ask in [false, true] {
                let lacking = sign_in_need(harness, true, ask).expect("lacks");
                assert_eq!(lacking.title, need(harness).title);
            }
        }
    }

    /// Protocol 14 ignores `ask`; protocol 15 reads it.
    #[test]
    fn only_protocol_15_reads_the_ask() {
        let ask = [NEED.to_owned()];
        let asks = steery_driver_common::program::asks;
        assert!(asks(15, &ask, NEED));
        assert!(!asks(14, &ask, NEED));
        assert!(!asks(15, &[], NEED));
    }

    #[test]
    fn a_change_keeps_the_ways_and_says_what_is_there() {
        for harness in [Harness::Omp, Harness::Pi] {
            let (lacking, change) = (need(harness), change(harness));
            assert_eq!(change.id, lacking.id);
            assert_eq!(change.ways, lacking.ways);
            assert_eq!(
                change.line,
                format!(
                    "{} has a model provider signed in on this host. Pick a new way to sign in.",
                    harness.label()
                )
            );
            assert_ne!(change.title, lacking.title);
        }
    }
}
