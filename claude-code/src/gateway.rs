//! A company's own gateway as a way to give Claude Code its sign-in (PROTOCOL.md §4.1,
//! Needs), as Claude Desktop's gateway configuration gives one
//! (https://claude.com/docs/third-party/claude-desktop/gateway: `inferenceGatewayBaseUrl`,
//! `inferenceGatewayApiKey`, `inferenceGatewayAuthScheme` `bearer` or `x-api-key`). The
//! gateway speaks the Anthropic Messages API. The runtime keeps the address and the key and
//! sets them for Claude Code on every host
//! (https://code.claude.com/docs/en/llm-gateway): `ANTHROPIC_BASE_URL`, and the key as
//! `ANTHROPIC_AUTH_TOKEN` (sent as `Authorization: Bearer`) or as `ANTHROPIC_API_KEY` (sent
//! as `x-api-key`). No other variable is given: Claude Code 2.1.73 sends an empty
//! `ANTHROPIC_AUTH_TOKEN` as an empty `Authorization: Bearer`, so a variable given empty
//! is not unset. A key of another way may be kept beside it, as the gateway's address may
//! be beside a key: `unset` (protocol 15) names the variable of the other header.
//!
//! The key is tried before it is given ([`steery_driver_common::gateway::check`]): one `GET`
//! of `/v1/models`, which costs no tokens, in the header the person named, from this host,
//! where Claude Code will send it. A `401` or `403` refuses the answer in the gateway's own
//! words, so the person's card asks again with that line and the key is kept nowhere. A
//! gateway that does not answer in time, a failed link, or an answer that says nothing of
//! the key (a `404`, a `503`) keeps the key, and the line of the `done` says it was not
//! checked: a person whose gateway is fine but slow is not stopped.

use std::collections::BTreeMap;

use steery_driver_common::gateway::Checked;
use steery_driver_common::{anthropic, gateway};
use steery_sdk::wire::harness::{Field, Given, Way};
use steery_sdk::{Error, refusal};

/// The way's id in every plan.
pub const WAY: &str = gateway::WAY;
/// The variable that names the gateway's address.
pub const BASE_URL: &str = "ANTHROPIC_BASE_URL";
/// The variable whose key Claude Code sends as `Authorization: Bearer`.
pub const AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";

/// The way's fields, by name.
const ADDRESS: &str = "url";
const KEY: &str = "key";
const HEADER: &str = "header";

/// What the key is tried on: the list of models, which every gateway of the Messages API
/// that Claude Code reaches serves and which costs no tokens.
const CHECKED_PATH: &str = "/v1/models";
/// The `anthropic-version` that Claude Code sends the gateway.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// How the key goes to the gateway: Claude Desktop's `inferenceGatewayAuthScheme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Header {
    Bearer,
    XApiKey,
}

/// The way.
pub fn way() -> Way {
    Way {
        id: WAY.into(),
        label: "Use your company's Claude gateway".into(),
        about: Some(
            "For a company that sends Claude through its own gateway, as Claude Desktop can. \
             Every host uses this address and key."
                .into(),
        ),
        link: None,
        fields: vec![
            Field {
                name: ADDRESS.into(),
                label: "Gateway address".into(),
                about: Some("For example https://llm-gateway.example.com".into()),
                secret: false,
            },
            Field {
                name: KEY.into(),
                label: "Gateway key".into(),
                about: Some(
                    "The key or token of your gateway. Type any word if it asks for none.".into(),
                ),
                secret: true,
            },
            Field {
                name: HEADER.into(),
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

/// What `values` give by [`way`]: the address and the key as Claude Code's variables, once
/// the gateway has not refused the key ([`Asked::done`]). With `unsets` (the driver picked
/// protocol 15 or newer), the variable of the other header, which an earlier gateway may
/// have given, and the marker of a key given through Steery
/// ([`crate::sign_in::KEY_MARKER`]) are unset.
pub async fn give(values: &BTreeMap<String, String>, unsets: bool) -> Result<Given, Error> {
    let asked = Asked::read(values)?;
    let checked = asked.check().await;
    asked.done(&checked, unsets)
}

/// What a person typed for the way, as far as it can be read before the gateway is asked.
struct Asked {
    address: String,
    key: String,
    header: Header,
}

impl Asked {
    fn read(values: &BTreeMap<String, String>) -> Result<Self, Error> {
        let value = |name: &str| values.get(name).map_or("", |value| value.trim());
        let address = gateway::address(value(ADDRESS), "Claude Code").map_err(refused)?;
        let key = value(KEY);
        if key.is_empty() {
            return Err(refused("Type the key of your gateway"));
        }
        if key.chars().any(char::is_control) {
            return Err(refused("The key of your gateway has no line break"));
        }
        let header = header(value(HEADER)).map_err(refused)?;
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
    fn done(&self, checked: &Checked, unsets: bool) -> Result<Given, Error> {
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
            Checked::Taken => format!("Claude Code uses your company's gateway at {address}"),
            Checked::Unchecked(why) => format!(
                "Claude Code uses your company's gateway at {address}. Steery did not check the key: {why}."
            ),
        };
        let (variable, other) = self.variables();
        Ok(Given::Done {
            line,
            env: BTreeMap::from([
                (BASE_URL.to_owned(), self.address.clone()),
                (variable.to_owned(), self.key.clone()),
            ]),
            unset: if unsets {
                vec![other.to_owned(), crate::sign_in::KEY_MARKER.to_owned()]
            } else {
                Vec::new()
            },
        })
    }
}

/// Whether Claude Code here sends its turns to a gateway: `ANTHROPIC_BASE_URL` is set.
pub fn used() -> bool {
    std::env::var_os(BASE_URL).is_some_and(|url| !url.is_empty())
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

    fn values(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// What the gateway's `checked` makes of the answer, with no request made.
    fn done(pairs: &[(&str, &str)], checked: &Checked) -> Result<Given, Error> {
        Asked::read(&values(pairs))?.done(checked, true)
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
            (ADDRESS, " https://gw.example.com/anthropic/ "),
            (super::KEY, "tok"),
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
        let pairs = |header: &str| -> Asked {
            Asked::read(&values(&[
                (ADDRESS, "https://gw.example.com"),
                (super::KEY, "k"),
                (HEADER, header),
            ]))
            .unwrap()
        };
        let unset = |header: &str, unsets: bool| match pairs(header).done(&Checked::Taken, unsets) {
            Ok(Given::Done { unset, .. }) => unset,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            unset("", true),
            [anthropic::VARIABLE, crate::sign_in::KEY_MARKER]
        );
        assert_eq!(
            unset("x-api-key", true),
            [AUTH_TOKEN, crate::sign_in::KEY_MARKER]
        );
        assert!(unset("", false).is_empty());
        assert!(unset("x-api-key", false).is_empty());
    }

    #[test]
    fn an_x_api_key_is_the_api_key_alone() {
        let env = given(&[
            (ADDRESS, "https://gw.example.com"),
            (super::KEY, "k"),
            (HEADER, "X-Api-Key"),
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
            assert!(given(&[(ADDRESS, no), (super::KEY, "k")]).is_err(), "{no}");
        }
    }

    #[test]
    fn a_missing_key_or_unknown_header_is_refused() {
        assert!(given(&[(ADDRESS, "https://gw.example.com")]).is_err());
        assert!(
            given(&[
                (ADDRESS, "https://gw.example.com"),
                (super::KEY, "k"),
                (HEADER, "basic")
            ])
            .is_err()
        );
        assert!(
            given(&[(ADDRESS, "https://gw.example.com"), (super::KEY, "a\nb"),]).is_err(),
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
        let asked = values(&[(ADDRESS, address.as_str()), (KEY, SECRET)]);
        let Err(Error::Refused { word, line }) = give(&asked, true).await else {
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
            (ADDRESS, address.as_str()),
            (KEY, SECRET),
            (HEADER, "x-api-key"),
        ]);
        let Ok(Given::Done { line, env, .. }) = give(&asked, true).await else {
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
        let asked = values(&[(ADDRESS, closed.as_str()), (KEY, SECRET)]);
        let Ok(Given::Done { line, env, .. }) = give(&asked, true).await else {
            panic!("a gateway that was not there refused the key");
        };
        assert!(line.contains("Steery did not check the key: "), "{line}");
        assert!(!line.contains(SECRET), "{line}");
        assert_eq!(env[AUTH_TOKEN], SECRET);
    }
}
