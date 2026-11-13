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
//! is not unset. Only a host not signed in asks for this way, so no key of an earlier way
//! is kept beside it.

use std::collections::BTreeMap;
use std::net::IpAddr;

use steery_driver_common::anthropic;
use steery_sdk::wire::harness::{Field, Given, Way};
use steery_sdk::{Error, refusal};

/// The way's id in every plan.
pub const WAY: &str = "gateway";
/// The variable that names the gateway's address.
const BASE_URL: &str = "ANTHROPIC_BASE_URL";
/// The variable whose key Claude Code sends as `Authorization: Bearer`.
const AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";

/// The way's fields, by name.
const ADDRESS: &str = "url";
const KEY: &str = "key";
const HEADER: &str = "header";

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

/// What `values` give by [`way`]: the address and the key as Claude Code's variables.
pub fn give(values: &BTreeMap<String, String>) -> Result<Given, Error> {
    let value = |name: &str| values.get(name).map_or("", |value| value.trim());
    let address = address(value(ADDRESS)).map_err(refused)?;
    let key = value(KEY);
    if key.is_empty() {
        return Err(refused("Type the key of your gateway"));
    }
    let header = header(value(HEADER)).map_err(refused)?;
    let variable = match header {
        Header::Bearer => AUTH_TOKEN,
        Header::XApiKey => anthropic::VARIABLE,
    };
    Ok(Given::Done {
        line: format!("Claude Code uses your company's gateway at {address}"),
        env: BTreeMap::from([
            (BASE_URL.to_owned(), address),
            (variable.to_owned(), key.to_owned()),
        ]),
    })
}

/// Whether Claude Code here sends its turns to a gateway: `ANTHROPIC_BASE_URL` is set.
pub fn used() -> bool {
    std::env::var_os(BASE_URL).is_some_and(|url| !url.is_empty())
}

/// `given` as the gateway's base address, without a `/` at its end. Claude Code adds
/// `/v1/messages` to it. As Claude Desktop does, it is `https`, or `http` only on this
/// computer (`localhost`, a loopback IP address).
fn address(given: &str) -> Result<String, String> {
    let given = given.trim_end_matches('/');
    if given.is_empty() {
        return Err("Type the address of your gateway".into());
    }
    let Some((scheme, rest)) = given.split_once("://") else {
        return Err(format!(
            "A gateway address starts with https://, not {given}"
        ));
    };
    if rest.contains(|c: char| c.is_whitespace() || c == '?' || c == '#') {
        return Err("A gateway address has no spaces, no ? and no #".into());
    }
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    if authority.contains('@') {
        return Err("Type the key in its own field, not in the address".into());
    }
    let host = match authority.strip_prefix('[') {
        Some(inside) => inside.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    if host.is_empty() {
        return Err(format!("{given} names no host"));
    }
    match scheme.to_ascii_lowercase().as_str() {
        "https" => {}
        "http"
            if host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()) => {}
        "http" => {
            return Err(
                "A gateway address starts with https://. Only localhost may use http://".into(),
            );
        }
        _ => {
            return Err(format!(
                "A gateway address starts with https://, not {scheme}://"
            ));
        }
    }
    if path.to_ascii_lowercase().ends_with("/v1") {
        return Err(
            "Type the address without /v1 at its end. Claude Code adds /v1/messages to it.".into(),
        );
    }
    Ok(given.to_owned())
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

    fn given(pairs: &[(&str, &str)]) -> Result<BTreeMap<String, String>, String> {
        let values = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        match give(&values) {
            Ok(Given::Done { env, .. }) => Ok(env),
            Ok(Given::Next(way)) => Err(format!("a next step {}", way.id)),
            Err(error) => Err(error.to_string()),
        }
    }

    #[test]
    fn a_bearer_key_is_the_auth_token_alone() {
        let env = given(&[
            (ADDRESS, " https://gw.example.com/anthropic/ "),
            (KEY, "tok"),
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
    fn an_x_api_key_is_the_api_key_alone() {
        let env = given(&[
            (ADDRESS, "https://gw.example.com"),
            (KEY, "k"),
            (HEADER, "X-Api-Key"),
        ])
        .unwrap();
        assert_eq!(env[anthropic::VARIABLE], "k");
        assert!(!env.contains_key(AUTH_TOKEN));
    }

    #[test]
    fn http_only_on_this_computer() {
        for ok in [
            "http://localhost:4000",
            "http://127.0.0.1:4000/x",
            "http://[::1]:4000",
        ] {
            assert!(address(ok).is_ok(), "{ok}");
        }
        for no in [
            "http://gw.example.com",
            "http://localhost.example.com",
            "http://127.example.com",
            "ftp://gw.example.com",
            "gw.example.com",
            "https://",
            "https://user:pw@gw.example.com",
            "https://gw.example.com/v1",
            "https://gw.example.com/v1/",
            "https://gw.example.com?x=1",
        ] {
            assert!(address(no).is_err(), "{no}");
        }
    }

    #[test]
    fn a_missing_key_or_unknown_header_is_refused() {
        assert!(given(&[(ADDRESS, "https://gw.example.com")]).is_err());
        assert!(
            given(&[
                (ADDRESS, "https://gw.example.com"),
                (KEY, "k"),
                (HEADER, "basic")
            ])
            .is_err()
        );
    }
}
