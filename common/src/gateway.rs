//! A company's own gateway as a way to give a harness its sign-in (PROTOCOL.md §4.1,
//! Needs). The gateway speaks the Anthropic Messages API, and the harness adds
//! `/v1/messages` to the address that its person types. What drivers share is the check of
//! that address.

use std::net::IpAddr;

/// The way's id in every plan.
pub const WAY: &str = "gateway";

/// `given` as the gateway's base address, without a `/` at its end, for `harness`, which
/// adds `/v1/messages` to it. As Claude Desktop does, it is `https`, or `http` only on this
/// computer (`localhost`, a loopback IP address). It has no user, no query, no fragment
/// and no `/v1` at its end. An error says in one line what to change.
pub fn address(given: &str, harness: &str) -> Result<String, String> {
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
        return Err(format!(
            "Type the address without /v1 at its end. {harness} adds /v1/messages to it."
        ));
    }
    Ok(given.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_only_on_this_computer() {
        for ok in [
            "http://localhost:4000",
            "http://127.0.0.1:4000/x",
            "http://[::1]:4000",
            "https://gw.example.com/anthropic/",
        ] {
            assert!(address(ok, "omp").is_ok(), "{ok}");
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
            "https://gw.example.com#x",
        ] {
            assert!(address(no, "omp").is_err(), "{no}");
        }
    }

    #[test]
    fn the_address_loses_its_last_slash() {
        assert_eq!(
            address("https://gw.example.com/anthropic/", "omp").unwrap(),
            "https://gw.example.com/anthropic"
        );
    }

    #[test]
    fn the_refusal_names_the_harness() {
        let error = address("https://gw.example.com/v1", "Claude Code").unwrap_err();
        assert!(error.contains("Claude Code adds /v1/messages"), "{error}");
    }
}
