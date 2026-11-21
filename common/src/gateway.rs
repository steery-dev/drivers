//! A company's own gateway as a way to give a harness its sign-in (PROTOCOL.md §4.1,
//! Needs). The gateway speaks the Anthropic Messages API, and the harness adds
//! `/v1/messages` to the address that its person types. What drivers share is the check of
//! that address ([`address`]) and the check of the key that goes with it ([`check`]).
//!
//! **The key is tried once, where the driver runs.** [`check`] sends one request that costs no
//! tokens (`GET /v1/models` for Claude Code) with the person's key in the header that the
//! person named, through `curl`, which every host that installs a harness has. The key goes to
//! `curl` on its standard input, never on a command line that `ps` lists. A `401` or a `403`
//! is a refusal with the gateway's own words ([`Checked::Refused`]). A gateway that does not
//! answer in time, a link that fails, a status that says nothing of the key (a `404` where
//! the gateway has no such path, a `503`), or no `curl`, learns nothing of the key
//! ([`Checked::Unchecked`]): the driver keeps the key and says that it did not check it, so a
//! person whose gateway is fine but slow is not stopped.

use std::net::IpAddr;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The way's id in every plan.
pub const WAY: &str = "gateway";

/// How long a check has in all: the link, the gateway's answer and the read of it. A
/// `needs/give` has 30 s, so a slow gateway leaves the rest of the answer its time.
pub const CHECK: Duration = Duration::from_secs(8);
/// How much of those a link may take.
const LINK: Duration = Duration::from_secs(5);
/// How long past its bound `curl` has to end by itself before it is killed.
const GRACE: Duration = Duration::from_secs(2);
/// The most of a gateway's answer that is read: its head and the start of its body.
const READ: u64 = 16 * 1024;
/// The longest line of a gateway's own words that is kept, in characters.
const SAID: usize = 160;

/// What one request to a gateway told of a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Checked {
    /// The gateway took the key: it answered with a success.
    Taken,
    /// The gateway refused the key (`401` or `403`). `said` is its own words, as one plain
    /// line with the key cut out of them, where its answer had any.
    Refused { status: u16, said: Option<String> },
    /// Nothing was learned of the key. Why, in one line.
    Unchecked(String),
}

/// Try `key` once on the gateway at `address` (as [`address`] checked it): `GET` of `path`
/// (`/v1/models`) with `headers`, the key among them, within [`CHECK`]. Never on a command
/// line: the key goes to `curl` on its standard input, and no word of what comes back keeps it.
pub async fn check(address: &str, path: &str, headers: &[(&str, &str)], key: &str) -> Checked {
    check_within(address, path, headers, key, CHECK).await
}

/// [`check`] within `within`.
pub async fn check_within(
    address: &str,
    path: &str,
    headers: &[(&str, &str)],
    key: &str,
    within: Duration,
) -> Checked {
    let Some(curl) = crate::program::on_path("curl") else {
        return Checked::Unchecked("`curl` is not on PATH here".into());
    };
    let Some(config) = config(&format!("{address}{path}"), headers) else {
        return Checked::Unchecked("the key or a header has a line break".into());
    };
    let ran = tokio::time::timeout(within + GRACE, run(&curl, &config, within)).await;
    match ran {
        Ok(Ok((status, body))) => judged(status, &body, key),
        Ok(Err(why)) => Checked::Unchecked(cut(&why, key)),
        Err(_) => Checked::Unchecked(timed_out(within)),
    }
}

fn timed_out(within: Duration) -> String {
    format!(
        "the gateway did not answer within {} s",
        within.as_secs().max(1)
    )
}

/// `curl`'s configuration for one `GET`: its address and each header, one to a line, quoted.
/// None where a word holds a control character, which could end its line.
fn config(url: &str, headers: &[(&str, &str)]) -> Option<String> {
    let mut text = format!("url = {}\n", quoted(url)?);
    for (name, value) in headers {
        text.push_str(&format!(
            "header = {}\n",
            quoted(&format!("{name}: {value}"))?
        ));
    }
    Some(text)
}

/// `word` as one double-quoted word of a `curl` configuration, where `\` and `"` are escaped.
fn quoted(word: &str) -> Option<String> {
    if word.chars().any(char::is_control) {
        return None;
    }
    Some(format!(
        "\"{}\"",
        word.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// Run `curl` on `config` for at most `within`: the status and the start of the body of the
/// gateway's answer, or why there is none.
async fn run(curl: &Path, config: &str, within: Duration) -> Result<(u16, String), String> {
    let mut command = tokio::process::Command::new(curl);
    command
        // First, so no `.curlrc` of the person changes what is sent.
        .arg("--disable")
        .args(["--silent", "--show-error", "--include"])
        .args(["--proto", "=http,https"])
        .arg("--connect-timeout")
        .arg(format!("{:.1}", LINK.min(within).as_secs_f64()))
        .arg("--max-time")
        .arg(format!("{:.1}", within.as_secs_f64()))
        .args(["--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| format!("`curl` did not start: {e}"))?;
    let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("`curl` gave no pipes".into());
    };
    stdin
        .write_all(config.as_bytes())
        .await
        .map_err(|e| format!("`curl` was not given its request: {e}"))?;
    drop(stdin);
    let mut out = Vec::new();
    let _ = stdout.take(READ).read_to_end(&mut out).await;
    // An answer past what is read is not waited for: its head is read already.
    if out.len() as u64 >= READ {
        let _ = child.kill().await;
    }
    let ended = child.wait().await.ok().and_then(|status| status.code());
    if let Some(answer) = parsed(&out) {
        return Ok(answer);
    }
    let mut said = Vec::new();
    let _ = (&mut stderr).take(READ).read_to_end(&mut said).await;
    Err(failed(ended, &String::from_utf8_lossy(&said), within))
}

/// Why `curl` gave no answer: what it exited with, and the last line it said.
fn failed(code: Option<i32>, said: &str, within: Duration) -> String {
    match code {
        Some(6) => "its address did not resolve".into(),
        Some(7) => "it could not connect to the gateway".into(),
        Some(28) => timed_out(within),
        _ => {
            let line = said
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty())
                .map_or("", |line| {
                    line.strip_prefix("curl: ").map_or(line, |line| {
                        line.split_once(") ").map_or(line, |(_, rest)| rest)
                    })
                });
            match line.is_empty() {
                true => "the gateway gave no answer".into(),
                false => format!("the gateway gave no answer: {line}"),
            }
        }
    }
}

/// The status and the body of the answer that `curl --include` printed: the head of a
/// proxy's `CONNECT` and each `1xx` are passed over.
fn parsed(out: &[u8]) -> Option<(u16, String)> {
    let text = String::from_utf8_lossy(out);
    let mut rest: &str = &text;
    loop {
        if !rest.starts_with("HTTP/") {
            return None;
        }
        let (head, after) = rest
            .split_once("\r\n\r\n")
            .or_else(|| rest.split_once("\n\n"))?;
        let line = head.lines().next()?;
        let status: u16 = line.split_whitespace().nth(1)?.parse().ok()?;
        if status / 100 == 1 || line.to_ascii_lowercase().contains("connection established") {
            rest = after;
            continue;
        }
        return Some((status, after.to_owned()));
    }
}

/// What the status says of the key: a success takes it, `401` and `403` refuse it, and
/// anything else says nothing of it.
fn judged(status: u16, body: &str, key: &str) -> Checked {
    match status {
        200..=299 => Checked::Taken,
        401 | 403 => Checked::Refused {
            status,
            said: said(body, key),
        },
        _ => Checked::Unchecked(format!("the gateway answered {status}")),
    }
}

/// The gateway's own words in `body`: its `error` (a text, or an object with a `message`), its
/// `message`, `detail` or `error_description`; else, where the body is plain text and not a
/// page, its first line. One line, with `key` cut out, and at most [`SAID`] characters.
fn said(body: &str, key: &str) -> Option<String> {
    let body = body.trim();
    let words = match serde_json::from_str::<serde_json::Value>(body) {
        Ok(json) => words_of(&json),
        Err(_) => (!body.starts_with('<')).then(|| body.to_owned()),
    }?;
    let first = words.lines().map(str::trim).find(|line| !line.is_empty())?;
    Some(cut(first, key)).filter(|line| !line.is_empty())
}

fn words_of(json: &serde_json::Value) -> Option<String> {
    let text = |value: &serde_json::Value| value.as_str().map(str::to_owned);
    json.get("error")
        .and_then(|error| text(error).or_else(|| error.get("message").and_then(text)))
        .or_else(|| {
            ["message", "detail", "error_description"]
                .iter()
                .find_map(|name| json.get(name).and_then(text))
        })
}

/// `line` without `key`, cut to [`SAID`] characters. A gateway can say the key it refused.
fn cut(line: &str, key: &str) -> String {
    let line = match key.is_empty() {
        true => line.to_owned(),
        false => line.replace(key, "…"),
    };
    match line.chars().count() > SAID {
        true => format!("{}…", line.chars().take(SAID - 1).collect::<String>()),
        false => line,
    }
}

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
            let _ = tx.send(String::from_utf8_lossy(&head).into_owned());
            if reply.is_empty() {
                // A gateway that is slow: it takes the request and says nothing for a while.
                std::thread::sleep(Duration::from_secs(4));
            } else {
                stream.write_all(reply.as_bytes()).unwrap();
            }
        });
        (address, rx)
    }

    const KEY: &str = "sk-gw-secret-0123456789";

    async fn tried(address: &str) -> Checked {
        let bearer = format!("Bearer {KEY}");
        check_within(
            address,
            "/v1/models",
            &[
                ("Authorization", &bearer),
                ("anthropic-version", "2023-06-01"),
            ],
            KEY,
            Duration::from_secs(3),
        )
        .await
    }

    #[tokio::test]
    async fn a_refused_key_is_refused_in_the_gateways_words_without_the_key() {
        let (address, seen) = serving(
            "HTTP/1.1 401 Unauthorized\r\nconnection: close\r\n\r\n\
             {\"error\":\"Invalid API key sk-gw-secret-0123456789\"}",
        );
        assert_eq!(
            tried(&address).await,
            Checked::Refused {
                status: 401,
                said: Some("Invalid API key …".into())
            }
        );
        // One `GET` that costs no tokens, with the key in the header that was named.
        let head = seen.recv().unwrap().to_ascii_lowercase();
        assert!(head.starts_with("get /v1/models http/1.1"), "{head}");
        assert!(
            head.contains(&format!("authorization: bearer {KEY}")),
            "{head}"
        );
        assert!(head.contains("anthropic-version: 2023-06-01"), "{head}");
    }

    #[tokio::test]
    async fn an_error_object_and_a_403_are_read_as_the_gateways_words() {
        let (address, _) = serving(
            "HTTP/1.1 403 Forbidden\r\nconnection: close\r\n\r\n\
             {\"error\":{\"type\":\"authentication_error\",\"message\":\"invalid x-api-key\"}}",
        );
        assert_eq!(
            tried(&address).await,
            Checked::Refused {
                status: 403,
                said: Some("invalid x-api-key".into())
            }
        );
        let (address, _) =
            serving("HTTP/1.1 401 Unauthorized\r\nconnection: close\r\n\r\n<html>no</html>");
        assert_eq!(
            tried(&address).await,
            Checked::Refused {
                status: 401,
                said: None
            },
            "a page says nothing the person can read"
        );
    }

    #[tokio::test]
    async fn a_success_takes_the_key() {
        let (address, _) = serving(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"data\":[]}",
        );
        assert_eq!(tried(&address).await, Checked::Taken);
    }

    #[tokio::test]
    async fn an_answer_that_says_nothing_of_the_key_is_unchecked() {
        for (reply, status) in [
            (
                "HTTP/1.1 503 Unavailable\r\nconnection: close\r\n\r\n",
                "503",
            ),
            ("HTTP/1.1 404 Not Found\r\nconnection: close\r\n\r\n", "404"),
        ] {
            let (address, _) = serving(reply);
            assert_eq!(
                tried(&address).await,
                Checked::Unchecked(format!("the gateway answered {status}"))
            );
        }
    }

    #[tokio::test]
    async fn a_gateway_that_is_not_there_or_is_slow_is_unchecked() {
        // Nothing listens on a port that was just free.
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let Checked::Unchecked(why) = tried(&closed).await else {
            panic!("a closed port refused a key it never saw");
        };
        assert!(why.contains("connect"), "{why}");
        let (slow, _) = serving("");
        let began = std::time::Instant::now();
        let checked = check_within(
            &slow,
            "/v1/models",
            &[("x-api-key", KEY)],
            KEY,
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(
            checked,
            Checked::Unchecked("the gateway did not answer within 1 s".into())
        );
        assert!(began.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn a_key_with_a_line_break_is_never_sent() {
        let checked = check(
            "http://127.0.0.1:9",
            "/v1/models",
            &[("x-api-key", "a\nurl = x")],
            "a",
        )
        .await;
        assert!(matches!(checked, Checked::Unchecked(_)), "{checked:?}");
    }

    #[test]
    fn the_words_are_one_line_cut_to_its_bound() {
        let long = format!("{{\"message\":\"{}\\nsecond\"}}", "w".repeat(400));
        let line = said(&long, "k").unwrap();
        assert_eq!(line.chars().count(), SAID);
        assert!(line.ends_with('…') && !line.contains("second"));
        assert_eq!(
            said("plain words\nmore", "k").as_deref(),
            Some("plain words")
        );
        assert_eq!(said("", "k"), None);
    }
}
