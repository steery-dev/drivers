//! Claude Code's own ways to sign in, for a harness that signs in to Claude as Claude Code
//! does (PROTOCOL.md §4.1, Needs): Claude Code itself, and a harness that reads Claude
//! Code's variables and calls Claude through it on a plan. Each driver names the harness in
//! its lines and adds the variables of its own that a `done` unsets.
//!
//! **A Claude plan** is Claude Code's own `claude auth login`, run unmodified on a terminal of
//! the driver's ([`crate::login`]). It opens its loopback link
//! (`redirect_uri=http://localhost:<port>/callback`) through `$BROWSER` and prints a manual one
//! whose page shows a code (`redirect_uri=https://platform.claude.com/oauth/code/callback`;
//! both Claude Code 2.1.317 on macOS). The driver's `$BROWSER` records the link it is given,
//! and the card shows that one: the Steery app catches its return and fills the field, and the
//! driver fetches that address inside this host, where Claude Code's own listener completes
//! the sign-in. Claude Code keeps what its sign-in gives; the driver reads no token and keeps
//! none.
//!
//! **A company's own gateway** is given as Claude Desktop's gateway configuration gives one
//! (https://claude.com/docs/third-party/claude-desktop/gateway: `inferenceGatewayBaseUrl`,
//! `inferenceGatewayApiKey`, `inferenceGatewayAuthScheme` `bearer` or `x-api-key`). The
//! gateway speaks the Anthropic Messages API. The runtime keeps the address and the key and
//! sets them on every host (https://code.claude.com/docs/en/llm-gateway): `ANTHROPIC_BASE_URL`,
//! and the key as `ANTHROPIC_AUTH_TOKEN` (sent as `Authorization: Bearer`) or as
//! `ANTHROPIC_API_KEY` (sent as `x-api-key`). No other variable is given: Claude Code 2.1.73
//! sends an empty `ANTHROPIC_AUTH_TOKEN` as an empty `Authorization: Bearer`, so a variable
//! given empty is not unset. A key of another way may be kept beside it, as the gateway's
//! address may be beside a key: `unset` (protocol 15) names the variable of the other header.
//!
//! The key is tried before it is given ([`crate::gateway::check`]): one `GET` of `/v1/models`,
//! which costs no tokens, in the header the person named, from this host, where the harness
//! will send it. A `401` or `403` refuses the answer in the gateway's own words, so the
//! person's card asks again with that line and the key is kept nowhere. A gateway that does
//! not answer in time, a failed link, or an answer that says nothing of the key (a `404`, a
//! `503`) keeps the key, and the line of the `done` says it was not checked: a person whose
//! gateway is fine but slow is not stopped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use steery_sdk::wire::harness::{Field, Given, NeedLink, Way};
use steery_sdk::{Error, refusal};

use crate::gateway::{self, Checked};
use crate::login::{self, Login, Waiting};
use crate::{anthropic, program};

/// The way of Claude Code's own sign-in, and the step it asks next once started.
pub const PLAN_WAY: &str = "claude-sign-in";
pub const PLAN_RETURN: &str = "claude-sign-in-return";
/// The field that step's return fills, or its person pastes into.
const ADDRESS: &str = "address";

/// The variable that names the gateway's address.
pub const BASE_URL: &str = "ANTHROPIC_BASE_URL";
/// The variable whose key Claude Code sends as `Authorization: Bearer`.
pub const AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";

/// The gateway way's fields, by name.
const GATEWAY_URL: &str = "url";
const GATEWAY_KEY: &str = "key";
const GATEWAY_HEADER: &str = "header";

/// What a gateway's key is tried on: the list of models, which every gateway of the Messages
/// API that Claude Code reaches serves and which costs no tokens.
const CHECKED_PATH: &str = "/v1/models";
/// The `anthropic-version` that Claude Code sends the gateway.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// How long `claude auth login` has to call `$BROWSER` or print its link: it did both at
/// once (2.1.317).
const LINK_WITHIN: Duration = Duration::from_secs(15);
/// How long `$BROWSER` may still be called once the manual link is printed.
const BROWSER_AFTER: Duration = Duration::from_secs(3);

/// The way of Claude Code's own sign-in, `about` what it signs in.
pub fn plan_way(about: String) -> Way {
    Way {
        id: PLAN_WAY.into(),
        label: "Sign in with your Claude plan".into(),
        about: Some(about),
        link: None,
        fields: Vec::new(),
        ends_at: None,
    }
}

/// Start Claude Code's `claude auth login` (`binary`) with the driver's `$BROWSER`, kept in
/// `waiting`, and answer the step its link is: the loopback link it gave `$BROWSER`, else the
/// manual one it printed. A sign-in started before is ended.
pub async fn start(binary: &Path, waiting: &Waiting) -> Result<Given, Error> {
    waiting.end();
    let browser = Browser::new()
        .map_err(|e| refused(format!("cannot make a $BROWSER for Claude Code: {e}")))?;
    let mut command = program::helper(binary);
    command
        .args(["auth", "login"])
        .env("BROWSER", browser.program());
    let mut running = Login::start(command, PLAN_RETURN.into(), true)
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
    let ends_at = waiting.keep(running);
    Ok(Given::Next(Way {
        id: PLAN_RETURN.into(),
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

/// The return step of the sign-in kept in `waiting`, `values` what its person gave: done once
/// Claude Code's login ended well.
pub async fn returned(waiting: &Waiting, values: &BTreeMap<String, String>) -> Result<(), Error> {
    let given = values.get(ADDRESS).map_or("", String::as_str);
    waiting.give(PLAN_RETURN, given, "Claude Code").await
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

/// The gateway's way, `about` who uses it.
pub fn gateway_way(about: String) -> Way {
    Way {
        id: gateway::WAY.into(),
        label: "Use your company's Claude gateway".into(),
        about: Some(about),
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

/// What `values` give by [`gateway_way`]: the address and the key as Claude Code's variables,
/// once the gateway has not refused the key, for `harness` (as its lines name it). `unset`
/// is `None` where the driver picked protocol 14 or older; else the variables of the driver's
/// own that the gateway outranks, unset beside the variable of the other header.
pub async fn gateway_given(
    values: &BTreeMap<String, String>,
    harness: &str,
    unset: Option<&[&str]>,
) -> Result<Given, Error> {
    let asked = Asked::read(values, harness)?;
    let checked = asked.check().await;
    asked.done(&checked, harness, unset)
}

/// Whether the harness here sends its turns to a gateway: `ANTHROPIC_BASE_URL` is set.
pub fn gateway_used() -> bool {
    std::env::var_os(BASE_URL).is_some_and(|url| !url.is_empty())
}

/// How the key goes to the gateway: Claude Desktop's `inferenceGatewayAuthScheme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Header {
    Bearer,
    XApiKey,
}

/// What a person typed for the gateway's way, as far as it can be read before the gateway is
/// asked.
struct Asked {
    address: String,
    key: String,
    header: Header,
}

impl Asked {
    fn read(values: &BTreeMap<String, String>, harness: &str) -> Result<Self, Error> {
        let value = |name: &str| values.get(name).map_or("", |value| value.trim());
        let address = gateway::address(value(GATEWAY_URL), harness).map_err(refused)?;
        let key = value(GATEWAY_KEY);
        if key.is_empty() {
            return Err(refused("Type the key of your gateway"));
        }
        if key.chars().any(char::is_control) {
            return Err(refused("The key of your gateway has no line break"));
        }
        let header = header(value(GATEWAY_HEADER)).map_err(refused)?;
        Ok(Self {
            address,
            key: key.to_owned(),
            header,
        })
    }

    /// The variable that holds the key, and the variable of the other header.
    fn variables(&self) -> (&'static str, &'static str) {
        match self.header {
            Header::Bearer => (AUTH_TOKEN, anthropic::VARIABLE),
            Header::XApiKey => (anthropic::VARIABLE, AUTH_TOKEN),
        }
    }

    /// One `GET /v1/models` of the gateway with the key in its header, as Claude Code sends it
    /// (`Authorization: Bearer`, or `x-api-key`).
    async fn check(&self) -> Checked {
        let key = match self.header {
            Header::Bearer => ("Authorization", format!("Bearer {}", self.key)),
            Header::XApiKey => ("x-api-key", self.key.clone()),
        };
        gateway::check(
            &self.address,
            CHECKED_PATH,
            &[
                (key.0, key.1.as_str()),
                ("anthropic-version", ANTHROPIC_VERSION),
            ],
            &self.key,
        )
        .await
    }

    /// What the gateway's answer to the key makes of the answer: a refusal says the gateway's
    /// words and gives nothing, and the key is kept nowhere; else the key is given, and where
    /// the gateway said nothing of it, the line says that it was not checked.
    fn done(&self, checked: &Checked, harness: &str, unset: Option<&[&str]>) -> Result<Given, Error> {
        let address = &self.address;
        let line = match checked {
            Checked::Refused { status, said } => {
                return Err(refused(match said {
                    Some(said) => format!(
                        "The gateway at {address} refused your key (HTTP {status}): {}. Check the key and the key header.",
                        said.trim_end_matches('.')
                    ),
                    None => format!(
                        "The gateway at {address} refused your key (HTTP {status}). Check the key and the key header."
                    ),
                }));
            }
            Checked::Taken => format!("{harness} uses your company's gateway at {address}"),
            Checked::Unchecked(why) => format!(
                "{harness} uses your company's gateway at {address}. Steery did not check the key: {why}."
            ),
        };
        let (variable, other) = self.variables();
        Ok(Given::Done {
            line,
            env: BTreeMap::from([
                (BASE_URL.to_owned(), self.address.clone()),
                (variable.to_owned(), self.key.clone()),
            ]),
            unset: match unset {
                Some(also) => std::iter::once(other)
                    .chain(also.iter().copied())
                    .map(str::to_owned)
                    .collect(),
                None => Vec::new(),
            },
        })
    }
}

/// The header that `given` names: empty or `bearer` (Claude Desktop's default), or
/// `x-api-key`.
fn header(given: &str) -> Result<Header, String> {
    match given.to_ascii_lowercase().as_str() {
        "" | "bearer" | "authorization" | "authorization: bearer" => Ok(Header::Bearer),
        "x-api-key" => Ok(Header::XApiKey),
        _ => Err(format!(
            "The key header is empty, bearer or x-api-key, not {given}"
        )),
    }
}

fn refused(line: impl Into<String>) -> Error {
    Error::refused(refusal::REFUSED, line)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HARNESS: &str = "Claude Code";
    /// A variable of a driver's own that a gateway outranks.
    const MARKER: &str = "CLAUDE_KEY_FROM_STEERY";

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// What the gateway's `checked` makes of the answer, with no request made.
    fn done(pairs: &[(&str, &str)], checked: &Checked) -> Result<Given, Error> {
        Asked::read(&values(pairs), HARNESS)?.done(checked, HARNESS, Some(&[MARKER]))
    }

    /// What the answer keeps once the gateway took the key.
    fn given(pairs: &[(&str, &str)]) -> Result<BTreeMap<String, String>, String> {
        match done(pairs, &Checked::Taken) {
            Ok(Given::Done { env, .. }) => Ok(env),
            Ok(Given::Next(way)) => Err(format!("a next step {}", way.id)),
            Err(error) => Err(error.to_string()),
        }
    }

    /// A gateway on this computer that answers its first request with `reply`, whole, and
    /// hands the head of that request to the test.
    fn serving(reply: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buf).unwrap();
                if read == 0 {
                    break;
                }
                head.extend_from_slice(&buf[..read]);
            }
            tx.send(String::from_utf8_lossy(&head).into_owned())
                .unwrap();
            stream.write_all(reply.as_bytes()).unwrap();
        });
        (address, rx)
    }

    const SECRET: &str = "gw-secret-key-0123456789";

    #[test]
    fn a_bearer_key_is_the_auth_token_alone() {
        let env = given(&[
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
                (AUTH_TOKEN.to_owned(), "tok".to_owned()),
            ])
        );
    }

    #[test]
    fn the_other_header_s_variable_is_unset_only_from_protocol_15() {
        let asked = |header: &str| -> Asked {
            Asked::read(
                &values(&[
                    (GATEWAY_URL, "https://gw.example.com"),
                    (GATEWAY_KEY, "k"),
                    (GATEWAY_HEADER, header),
                ]),
                HARNESS,
            )
            .unwrap()
        };
        let unset = |header: &str, unset: Option<&[&str]>| match asked(header).done(
            &Checked::Taken,
            HARNESS,
            unset,
        ) {
            Ok(Given::Done { unset, .. }) => unset,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            unset("", Some(&[MARKER])),
            [anthropic::VARIABLE, MARKER]
        );
        assert_eq!(unset("x-api-key", Some(&[MARKER])), [AUTH_TOKEN, MARKER]);
        assert_eq!(unset("", Some(&[])), [anthropic::VARIABLE]);
        assert!(unset("", None).is_empty());
        assert!(unset("x-api-key", None).is_empty());
    }

    #[test]
    fn an_x_api_key_is_the_api_key_alone() {
        let env = given(&[
            (GATEWAY_URL, "https://gw.example.com"),
            (GATEWAY_KEY, "k"),
            (GATEWAY_HEADER, "X-Api-Key"),
        ])
        .unwrap();
        assert_eq!(env[anthropic::VARIABLE], "k");
        assert!(!env.contains_key(AUTH_TOKEN));
    }

    #[test]
    fn a_bad_address_is_refused() {
        for no in [
            "http://gw.example.com",
            "https://gw.example.com/v1",
            "https://user:pw@gw.example.com",
        ] {
            assert!(
                given(&[(GATEWAY_URL, no), (GATEWAY_KEY, "k")]).is_err(),
                "{no}"
            );
        }
    }

    #[test]
    fn a_missing_key_or_unknown_header_is_refused() {
        assert!(given(&[(GATEWAY_URL, "https://gw.example.com")]).is_err());
        assert!(
            given(&[
                (GATEWAY_URL, "https://gw.example.com"),
                (GATEWAY_KEY, "k"),
                (GATEWAY_HEADER, "basic")
            ])
            .is_err()
        );
        assert!(
            given(&[(GATEWAY_URL, "https://gw.example.com"), (GATEWAY_KEY, "a\nb"),]).is_err(),
            "a key with a line break is never sent"
        );
    }

    /// The gateway at `proxy.steery.dev` answered a made-up key `401 {"error":"Invalid API
    /// key"}` on 2026-11-16, and the key was kept: a refusal gives nothing, and says the
    /// gateway's words.
    #[tokio::test]
    async fn a_refused_key_is_refused_in_the_gateways_words_and_given_nowhere() {
        let (address, seen) = serving(
            "HTTP/1.1 401 Unauthorized\r\nconnection: close\r\n\r\n{\"error\":\"Invalid API key\"}",
        );
        let asked = values(&[(GATEWAY_URL, address.as_str()), (GATEWAY_KEY, SECRET)]);
        let Err(Error::Refused { word, line }) =
            gateway_given(&asked, HARNESS, Some(&[MARKER])).await
        else {
            panic!("a refused key was given");
        };
        assert_eq!(word, refusal::REFUSED);
        let line = line.expect("a line");
        assert!(
            line.contains("refused your key (HTTP 401): Invalid API key."),
            "{line}"
        );
        assert!(!line.contains(SECRET), "{line}");
        // One `GET /v1/models` with the key where Claude Code sends it.
        let head = seen.recv().unwrap().to_ascii_lowercase();
        assert!(head.starts_with("get /v1/models http/1.1"), "{head}");
        assert!(
            head.contains(&format!("authorization: bearer {SECRET}")),
            "{head}"
        );
    }

    #[tokio::test]
    async fn an_x_api_key_is_tried_in_its_own_header() {
        let (address, seen) = serving("HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n{\"data\":[]}");
        let asked = values(&[
            (GATEWAY_URL, address.as_str()),
            (GATEWAY_KEY, SECRET),
            (GATEWAY_HEADER, "x-api-key"),
        ]);
        let Ok(Given::Done { line, env, .. }) =
            gateway_given(&asked, HARNESS, Some(&[MARKER])).await
        else {
            panic!("a key the gateway took was refused");
        };
        assert_eq!(
            line,
            format!("Claude Code uses your company's gateway at {address}")
        );
        assert_eq!(env[anthropic::VARIABLE], SECRET);
        let head = seen.recv().unwrap().to_ascii_lowercase();
        assert!(head.contains(&format!("x-api-key: {SECRET}")), "{head}");
        assert!(!head.contains("authorization:"), "{head}");
    }

    #[tokio::test]
    async fn a_gateway_that_cannot_be_asked_keeps_the_key_and_says_it_was_not_checked() {
        // Nothing listens on a port that was just free.
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let asked = values(&[(GATEWAY_URL, closed.as_str()), (GATEWAY_KEY, SECRET)]);
        let Ok(Given::Done { line, env, .. }) =
            gateway_given(&asked, HARNESS, Some(&[MARKER])).await
        else {
            panic!("a gateway that was not there refused the key");
        };
        assert!(line.contains("Steery did not check the key: "), "{line}");
        assert!(!line.contains(SECRET), "{line}");
        assert_eq!(env[AUTH_TOKEN], SECRET);
    }
}
