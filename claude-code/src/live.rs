//! Claude Code's own door into a live conversation: the inbox socket of the session its
//! registry names in it.
//!
//! One connection carries one `{"type":"auth","token":…}` line, where the session
//! published a peer token, and one `{"type":"user","message":{"role":"user","content":…}}`
//! line, and the session takes the message as a person's: a turn starts if it is idle,
//! and a running one reads it between tool calls. Delivered by default; a session that
//! bypasses permission prompts holds it for its person, and a message never answers a
//! permission prompt. The socket says nothing back. The session keeps its own model.

use std::path::Path;
use std::time::Duration;

use serde_json::json;
use steery_sdk::wire::harness::{Said, Say, SayMode};
use steery_sdk::{Error, refusal};
use tokio::io::AsyncWriteExt;

use crate::registry;
use crate::says::Says;
use crate::transcripts::Store;

/// How long the socket has to take the two lines. The harness closes a connection that
/// has not sent a complete line in 30 seconds; these are ready before it opens.
const POST_WITHIN: Duration = Duration::from_secs(8);

/// `say`: the words into the inbox of the session somebody is in. Claude Code has one
/// inbox and no way to say when its words go in: a running turn reads them between tool
/// calls, so `steer` and `queue` both go in there. The socket says nothing back, so words
/// handed over are unconfirmed and the runtime looks for them with `receipt/find`. Words
/// that may go in only while nothing runs (`immediate`) are refused `harness-busy` while
/// the session's registry entry says anything but `idle`; it is read before, so a turn its
/// person begins in between may still take them. A say with an `id` is given to `says`
/// first, whose transcript then names the turn that took its words.
pub async fn say(store: &Store, says: &Says, say: Say) -> Result<Said, Error> {
    let dir = registry::dir(store.root());
    let live = registry::live(&dir, &say.external_id)
        .ok_or_else(|| Error::refused(refusal::NO_DOOR, "no Claude Code session holds it now"))?;
    let idle = live.status.as_deref() == Some("idle");
    if say.mode == SayMode::Immediate && !idle {
        return Err(Error::refused(
            refusal::HARNESS_BUSY,
            match live.status.as_deref() {
                Some(status) => format!("the Claude Code session in it is {status}"),
                None => "the Claude Code session in it says nothing about a turn".to_owned(),
            },
        ));
    }
    let socket = live.socket.ok_or_else(|| {
        Error::refused(
            refusal::NO_DOOR,
            "the Claude Code session in it does not accept messages",
        )
    })?;
    if let Some(id) = say.id.as_deref() {
        let transcript = store.transcript(&say.external_id).ok();
        says.give(&say.external_id, id, &say.words, transcript.as_deref());
    }
    let posted = post(
        &socket,
        registry::peer_token(&dir, live.pid),
        &say.words,
        POST_WITHIN,
    )
    .await;
    if let (Err(Error::Refused { word, .. }), Some(id)) = (&posted, say.id.as_deref())
        && word == refusal::NO_DOOR
    {
        says.ungive(&say.external_id, id);
    }
    posted.map(|()| Said { took: false })
}

/// One message into a session's inbox: the auth line where a token is known, then the
/// message, then the connection is closed the way the harness's own client closes it.
/// Refused `no-door` where nothing was written, `no-answer` where it was and the socket
/// failed or ran out of time after.
async fn post(
    socket: &Path,
    token: Option<String>,
    text: &str,
    within: Duration,
) -> Result<(), Error> {
    let mut written = false;
    let posted = tokio::time::timeout(within, async {
        let mut stream = tokio::net::UnixStream::connect(socket).await?;
        let mut lines = String::new();
        if let Some(token) = token {
            lines.push_str(&json!({"type": "auth", "token": token}).to_string());
            lines.push('\n');
        }
        lines.push_str(
            &json!({"type": "user", "message": {"role": "user", "content": text}}).to_string(),
        );
        lines.push('\n');
        written = true;
        stream.write_all(lines.as_bytes()).await?;
        stream.flush().await?;
        // The harness's own client waits a moment before ending, so the last line is read
        // before the close is.
        tokio::time::sleep(Duration::from_millis(300)).await;
        stream.shutdown().await
    })
    .await;
    match posted {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(Error::refused(
            if written {
                refusal::NO_ANSWER
            } else {
                refusal::NO_DOOR
            },
            format!("Claude Code's inbox {}: {error}", socket.display()),
        )),
        Err(_) => Err(Error::refused(
            if written {
                refusal::NO_ANSWER
            } else {
                refusal::NO_DOOR
            },
            format!(
                "Claude Code's inbox {} did not answer in time",
                socket.display()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inbox_timeout_after_write_has_unknown_outcome() {
        use tokio::io::AsyncReadExt;
        let dir = crate::registry::tests::Scratch::new("inbox");
        let socket = dir.path().join("s.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let received = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 1024];
            let read = stream.read(&mut bytes).await.unwrap();
            assert!(String::from_utf8_lossy(&bytes[..read]).contains("delivery-marker"));
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        let outcome = post(&socket, None, "delivery-marker", Duration::from_millis(50)).await;
        assert!(matches!(
            outcome,
            Err(Error::Refused { ref word, .. }) if word == refusal::NO_ANSWER
        ));
        received.await.unwrap();
        let gone = post(
            &dir.path().join("none.sock"),
            None,
            "x",
            Duration::from_millis(50),
        )
        .await;
        assert!(matches!(
            gone,
            Err(Error::Refused { ref word, .. }) if word == refusal::NO_DOOR
        ));
    }

    #[tokio::test]
    async fn the_inbox_reads_an_auth_line_then_the_message() {
        use tokio::io::AsyncReadExt;
        let dir = crate::registry::tests::Scratch::new("inbox-lines");
        let socket = dir.path().join("s.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let received = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut said = String::new();
            stream.read_to_string(&mut said).await.unwrap();
            said
        });
        post(&socket, Some("token-41".into()), "go", POST_WITHIN)
            .await
            .unwrap();
        let said = received.await.unwrap();
        let lines: Vec<serde_json::Value> = said
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            lines,
            [
                json!({"type": "auth", "token": "token-41"}),
                json!({"type": "user", "message": {"role": "user", "content": "go"}})
            ]
        );
    }
}
