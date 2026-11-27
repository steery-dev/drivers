//! The one home of Claude access (API.md §6.0, §6.1). The source is read once, from Claude
//! Code's own variables, and never falls back to another:
//!
//! - `gateway`: `ANTHROPIC_BASE_URL` with `ANTHROPIC_AUTH_TOKEN` (sent as `Authorization:
//!   Bearer`) or `ANTHROPIC_API_KEY` (sent as `x-api-key`).
//! - `key`: `ANTHROPIC_API_KEY` alone, to `https://api.anthropic.com`.
//! - `plan`: neither, and `claude` on `PATH`: a plan signs in only inside Claude Code.
//! - `none`.
//!
//! A gateway or a key streams one Messages call per message ([`crate::messages`]); a plan
//! runs one warm `claude` per drawer ([`crate::code`]). Both read the same stream events
//! ([`Reading`]), and a failed call says its kind, its service and its own words
//! ([`Failure`]).

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Map, Value};
use steery_driver_common::{anthropic, claude, program};
use steery_sdk::wire::harness::FailureKind;

use crate::{code, messages};

/// The notice where no source is here.
pub const NO_SIGN_IN: &str = "No Claude sign-in. Use one: set ANTHROPIC_API_KEY; or set \
ANTHROPIC_BASE_URL and ANTHROPIC_AUTH_TOKEN for a Claude gateway; or sign in to Claude Code \
with `claude auth login`.";
/// How long the check has.
pub const CHECK: Duration = Duration::from_secs(8);
/// A call that sends nothing back by then fails, in its service's words: never a silent hang.
pub const FIRST_BYTE: Duration = Duration::from_secs(20);
/// The Claude API, where a key alone goes.
const API: &str = "https://api.anthropic.com";
/// The service a plan's failure names.
pub const CLAUDE: &str = "claude";

/// Where the person's Claude comes from.
pub enum Source {
    Direct(Direct),
    Plan(PathBuf),
    None,
}

/// A gateway or a key: the base URL and the key, in its header.
#[derive(Clone)]
pub struct Direct {
    gateway: bool,
    pub base: String,
    bearer: bool,
    key: String,
}

impl Source {
    /// The source of this process's environment.
    pub fn read() -> Self {
        let var = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let base = var(claude::BASE_URL)
            .map(|base| base.trim_end_matches('/').to_owned())
            .filter(|base| !base.is_empty());
        let token = var(claude::AUTH_TOKEN);
        let key = var(anthropic::VARIABLE);
        let direct = match (base, token, key) {
            (Some(base), Some(token), _) => Some((true, base, true, token)),
            (Some(base), None, Some(key)) => Some((true, base, false, key)),
            (None, _, Some(key)) => Some((false, API.to_owned(), false, key)),
            _ => None,
        };
        match direct {
            Some((gateway, base, bearer, key)) => Self::Direct(Direct {
                gateway,
                base,
                bearer,
                key,
            }),
            None => program::on_path(CLAUDE).map_or(Self::None, Self::Plan),
        }
    }

    /// Where calls go, as a failure names it: the base URL, or `claude`.
    pub fn service(&self) -> &str {
        match self {
            Self::Direct(direct) => &direct.base,
            Self::Plan(_) | Self::None => CLAUDE,
        }
    }

    /// The check: none where Claude answers, else the notice in plain words. A gateway or a
    /// key: `GET <base>/v1/models` with the key within [`CHECK`]; a plan: `claude auth
    /// status` says `loggedIn`.
    pub async fn check(&self) -> Option<String> {
        match self {
            Self::None => Some(NO_SIGN_IN.into()),
            Self::Plan(claude) => (!code::signed_in(claude).await).then(|| NO_SIGN_IN.into()),
            Self::Direct(direct) => {
                let direct = direct.clone();
                tokio::task::spawn_blocking(move || messages::check(&direct))
                    .await
                    .unwrap_or_else(|e| Some(format!("the sign-in check failed: {e}")))
            }
        }
    }
}

impl Direct {
    /// How a line names it.
    pub fn place(&self) -> String {
        match self.gateway {
            true => format!("Your Claude gateway at {}", self.base),
            false => format!("The Claude API at {}", self.base),
        }
    }

    /// `request` with the key in its header.
    pub fn authorize<B>(&self, request: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        match self.bearer {
            true => request.header("authorization", format!("Bearer {}", self.key)),
            false => request.header("x-api-key", self.key.as_str()),
        }
    }
}

/// A failed Claude call: its kind, its service (a base URL, or `claude`), and its words.
#[derive(Debug, Clone)]
pub struct Failure {
    pub kind: FailureKind,
    pub service: String,
    pub line: String,
}

impl Failure {
    pub fn other(service: &str, line: String) -> Self {
        Self {
            kind: FailureKind::Other,
            service: service.to_owned(),
            line,
        }
    }
}

/// The kind from the HTTP status, else from the words: Claude Code says only words.
pub fn kind_of(status: Option<u16>, words: &str) -> FailureKind {
    let words = words.to_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|needle| words.contains(needle));
    if matches!(status, Some(401 | 403))
        || number(&words, "401")
        || number(&words, "403")
        || any(&["authenticat", "/login", "not logged in", "oauth", "invalid api key"])
    {
        return FailureKind::SignIn;
    }
    if status == Some(429)
        || number(&words, "429")
        || rate_limit(&words)
        || any(&["usage limit", "limit reached"])
    {
        return FailureKind::RateLimit;
    }
    if any(&[
        "prompt is too long",
        "context window",
        "context length",
        "too many tokens",
        "too many input tokens",
    ]) {
        return FailureKind::ContextFull;
    }
    FailureKind::Other
}

/// Whether `digits` stand as a word of their own in `text`.
fn number(text: &str, digits: &str) -> bool {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    text.match_indices(digits).any(|(at, _)| {
        !word(text[..at].chars().next_back()) && !word(text[at + digits.len()..].chars().next())
    })
}

/// `rate`, at most one character, then `limit`.
fn rate_limit(text: &str) -> bool {
    text.match_indices("rate").any(|(at, _)| {
        let rest = &text[at + 4..];
        rest.starts_with("limit")
            || rest
                .chars()
                .next()
                .is_some_and(|c| rest[c.len_utf8()..].starts_with("limit"))
    })
}

/// An error answer in the API's own words: its `error.message` (or `error`, where a gateway
/// sends a string), else its text, else the status's reason.
pub fn api_words(body: &str, reason: Option<&str>) -> String {
    let text = body.trim();
    let said = serde_json::from_str::<Value>(text).ok().and_then(|json| {
        let error = json.get("error")?;
        error
            .as_str()
            .or_else(|| error.get("message")?.as_str())
            .map(str::to_owned)
    });
    let words = said.unwrap_or_else(|| text.to_owned());
    let words = match words.is_empty() {
        true => reason.unwrap_or("no words").to_owned(),
        false => words,
    };
    let cut: String = words.chars().take(300).collect();
    format!(
        "{}.",
        cut.trim_end_matches(|c: char| c == '.' || c.is_whitespace())
    )
}

/// One model call's tokens. `input` counts every input token, cached or not; `cached` is the
/// part read from the cache.
#[derive(Debug, Clone, Copy)]
pub struct Usage {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
}

/// The Messages API stream events of one call, read in order: its text (a newline between
/// text blocks), and at its stop the usage. The same events arrive directly and inside
/// Claude Code's `stream_event`.
pub struct Reading {
    service: String,
    blocks: u32,
    usage: Map<String, Value>,
    pub spent: Option<Usage>,
}

impl Reading {
    pub fn new(service: &str) -> Self {
        Self {
            service: service.to_owned(),
            blocks: 0,
            usage: Map::new(),
            spent: None,
        }
    }

    /// One event: its text to `text`; a stream's `error` is the call's failure.
    pub fn event(&mut self, event: &Value, text: &mut dyn FnMut(&str)) -> Result<(), Failure> {
        let merge = |usage: &mut Map<String, Value>, more: &Value| {
            if let Some(more) = more.as_object() {
                usage.extend(more.iter().map(|(k, v)| (k.clone(), v.clone())));
            }
        };
        match event["type"].as_str() {
            Some("message_start") => merge(&mut self.usage, &event["message"]["usage"]),
            Some("content_block_start") if event["content_block"]["type"] == "text" => {
                if self.blocks > 0 {
                    text("\n");
                }
                self.blocks += 1;
            }
            Some("content_block_delta") if event["delta"]["type"] == "text_delta" => {
                if let Some(delta) = event["delta"]["text"].as_str()
                    && !delta.is_empty()
                {
                    text(delta);
                }
            }
            Some("message_delta") => merge(&mut self.usage, &event["usage"]),
            Some("message_stop") => {
                let n = |key: &str| self.usage.get(key).and_then(Value::as_u64).unwrap_or(0);
                self.spent = Some(Usage {
                    input: n("input_tokens")
                        + n("cache_creation_input_tokens")
                        + n("cache_read_input_tokens"),
                    cached: n("cache_read_input_tokens"),
                    output: n("output_tokens"),
                });
            }
            Some("error") => {
                let words = event["error"]["message"]
                    .as_str()
                    .unwrap_or("the stream failed");
                let kind = match event["error"]["type"].as_str() {
                    Some("authentication_error" | "permission_error") => FailureKind::SignIn,
                    Some("rate_limit_error") => FailureKind::RateLimit,
                    _ => kind_of(None, words),
                };
                return Err(Failure {
                    kind,
                    service: self.service.clone(),
                    line: words.to_owned(),
                });
            }
            _ => {}
        }
        Ok(())
    }
}

/// One drawer's line to Claude: a streamed Messages call per message on a gateway or a key;
/// on a plan, its own warm `claude`, started as the line opens.
pub enum Line {
    Direct {
        direct: Direct,
        model: String,
        system: &'static str,
    },
    Plan(Box<code::Code>),
}

impl Line {
    /// The line of the drawer whose model and prompt these are, in `cwd`.
    pub fn open(
        source: &Source,
        model: String,
        system: &'static str,
        cwd: &std::path::Path,
    ) -> Result<Self, Failure> {
        match source {
            Source::Direct(direct) => Ok(Self::Direct {
                direct: direct.clone(),
                model,
                system,
            }),
            Source::Plan(claude) => code::Code::start(claude, &model, system, cwd)
                .map(|code| Self::Plan(Box::new(code))),
            Source::None => Err(Failure {
                kind: FailureKind::SignIn,
                service: CLAUDE.into(),
                line: NO_SIGN_IN.into(),
            }),
        }
    }

    /// One message: its text to `text` as it streams; the call's tokens, where it said them.
    pub async fn ask(
        &mut self,
        message: &str,
        text: &mut dyn FnMut(&str),
    ) -> Result<Option<Usage>, Failure> {
        match self {
            Self::Direct {
                direct,
                model,
                system,
            } => messages::stream(direct, model, system, message, text).await,
            Self::Plan(code) => code.ask(message, text).await,
        }
    }
}
