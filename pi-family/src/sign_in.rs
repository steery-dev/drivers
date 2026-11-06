//! The one need the harness has (PROTOCOL.md §4.1, Needs): a model provider signed in, where
//! it lists no model. Its person gives it with an Anthropic API key, which the runtime keeps
//! and sets as `ANTHROPIC_API_KEY` on every host, or, in omp, through omp's own `omp login
//! <provider>`, run unmodified on a terminal of the driver's
//! ([`steery_driver_common::login`]); omp keeps what that gives, the driver nothing. pi has
//! no login command (its `/login` is typed inside it), so pi has the key alone.
//!
//! omp's and pi's own Claude sign-in is none of its ways: it signs a third-party app in to a
//! Claude plan, which Anthropic's terms forbid.

use steery_driver_common::anthropic;
use steery_driver_common::login::{self, Login, Waiting};
use steery_driver_common::program::helper;
use steery_sdk::wire::harness::{Field, Give, Given, Need, NeedLink, Way};
use steery_sdk::{Error, refusal};

use crate::Harness;

/// The need, by its id in every plan.
pub const NEED: &str = "sign-in";
/// The field a return fills, or its person pastes into.
const ADDRESS: &str = "address";

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
                .chain([key])
                .collect(),
        },
    }
}

/// The sign-in started here and waiting for its person.
#[derive(Default, Clone)]
pub struct SignIns(Waiting);

impl SignIns {
    /// `needs/give`.
    pub async fn give(&self, harness: Harness, give: Give) -> Result<Given, Error> {
        let label = harness.label();
        if give.need != NEED {
            return Err(gone(format!("{label} has no need {} here", give.need)));
        }
        if give.way == anthropic::WAY {
            self.0.end();
            let line = format!("{label} runs Claude with your Anthropic API key");
            return anthropic::give(&give.values, &line);
        }
        if harness == Harness::Omp {
            if let Some(provider) = PROVIDERS.iter().find(|provider| provider.id == give.way) {
                return self.start(provider).await;
            }
            if self.0.waits_on(&give.way) {
                let given = give.values.get(ADDRESS).map_or("", String::as_str);
                self.0.give(&give.way, given, label).await?;
                return Ok(Given::Done {
                    line: format!("{label} is signed in on this host"),
                    env: Default::default(),
                });
            }
        }
        Err(gone(format!("{label} has no need {} with the way {} here", give.need, give.way)))
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
                    Some(code) => format!("Open the page, sign in, and enter {code}. Then continue."),
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
        format!("omp login printed no sign-in link within {} s", LINK_WITHIN.as_secs())
    }))
}

fn gone(line: impl Into<String>) -> Error {
    Error::refused(refusal::NOT_FOUND, line)
}

fn refused(line: impl Into<String>) -> Error {
    Error::refused(refusal::REFUSED, line)
}
