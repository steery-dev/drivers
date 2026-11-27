//! A gateway's or a key's side of Claude access (API.md §6.0, §6.1): the check, `GET
//! <base>/v1/models`, and one streamed `POST <base>/v1/messages` per message: `stream: true`,
//! `anthropic-version: 2023-06-01`, the prompt as one system block cached `ephemeral`,
//! thinking off, `max_tokens` 8,192.
//!
//! The call blocks on its own thread and hands each server-sent event to the drawer. One
//! that sends no event within [`FIRST_BYTE`] fails; an abort drops the drawer's side, and
//! the thread ends at its next event or with the process.

use std::io::BufRead as _;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::claude::{CHECK, Direct, FIRST_BYTE, Failure, Reading, Usage, api_words, kind_of};

const VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 8192;

fn agent(within: Option<std::time::Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(within)
        .build()
        .into()
}

/// The check: none where the key is taken, else the notice.
pub fn check(direct: &Direct) -> Option<String> {
    let request = agent(Some(CHECK))
        .get(format!("{}/v1/models", direct.base))
        .header("anthropic-version", VERSION);
    let mut response = match direct.authorize(request).call() {
        Ok(response) => response,
        Err(e) => {
            return Some(format!(
                "{} did not answer within {} s: {e}",
                direct.place(),
                CHECK.as_secs()
            ));
        }
    };
    let status = response.status();
    if status.is_success() {
        return None;
    }
    let body = response.body_mut().read_to_string().unwrap_or_default();
    let words = api_words(&body, status.canonical_reason());
    Some(match status.as_u16() {
        401 | 403 => format!(
            "{} refused the key (HTTP {}): {words}",
            direct.place(),
            status.as_u16()
        ),
        code => format!(
            "{} failed the sign-in check (HTTP {code}): {words}",
            direct.place()
        ),
    })
}

/// One message to `model` under `system`: its text to `text` as it streams, and its tokens.
pub async fn stream(
    direct: &Direct,
    model: &str,
    system: &str,
    message: &str,
    text: &mut dyn FnMut(&str),
) -> Result<Option<Usage>, Failure> {
    let body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "stream": true,
        "system": [{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}],
        "messages": [{"role": "user", "content": message}],
        "thinking": {"type": "disabled"},
    })
    .to_string();
    let (events, mut said) = mpsc::unbounded_channel();
    let call = direct.clone();
    std::thread::spawn(move || post(&call, &body, &events));
    let mut reading = Reading::new(&direct.base);
    let mut next = tokio::time::timeout(FIRST_BYTE, said.recv())
        .await
        .map_err(|_| {
            Failure::other(
                &direct.base,
                format!(
                    "{} sent nothing in {} s.",
                    direct.place(),
                    FIRST_BYTE.as_secs()
                ),
            )
        })?;
    loop {
        match next {
            None => return Ok(reading.spent),
            Some(Err(failure)) => return Err(failure),
            Some(Ok(event)) => reading.event(&event, text)?,
        }
        next = said.recv().await;
    }
}

/// The call itself: each `data:` event to `events`, or the failure.
fn post(direct: &Direct, body: &str, events: &mpsc::UnboundedSender<Result<Value, Failure>>) {
    let failed = |line: String| {
        let _ = events.send(Err(Failure::other(&direct.base, line)));
    };
    let request = agent(None)
        .post(format!("{}/v1/messages", direct.base))
        .header("anthropic-version", VERSION)
        .header("content-type", "application/json");
    let response = match direct.authorize(request).send(body) {
        Ok(response) => response,
        Err(e) => return failed(format!("{} did not answer: {e}", direct.place())),
    };
    let status = response.status();
    let mut answer = response.into_body();
    if !status.is_success() {
        let words = api_words(
            &answer.read_to_string().unwrap_or_default(),
            status.canonical_reason(),
        );
        let _ = events.send(Err(Failure {
            kind: kind_of(Some(status.as_u16()), &words),
            service: direct.base.clone(),
            line: format!(
                "{} answered HTTP {}: {words}",
                direct.place(),
                status.as_u16()
            ),
        }));
        return;
    }
    for line in std::io::BufReader::new(answer.into_reader()).lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => return failed(format!("{} cut its stream: {e}", direct.place())),
        };
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        if events.send(Ok(event)).is_err() {
            return;
        }
    }
}
