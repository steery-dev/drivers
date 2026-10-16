//! Claude Code's own door into a live conversation: the inbox socket of the session its
//! registry names in it, and a one-shot fork of the conversation for its recap.
//!
//! One connection carries one `{"type":"auth","token":…}` line, where the session
//! published a peer token, and one `{"type":"user","message":{"role":"user","content":…}}`
//! line, and the session takes the message as a person's: a turn starts if it is idle,
//! and a running one reads it between tool calls. Delivered by default; a session that
//! bypasses permission prompts holds it for its person, and a message never answers a
//! permission prompt. The socket says nothing back, so words handed over are unconfirmed
//! and the runtime looks for them with `receipt/find`. The session keeps its own model:
//! the inbox reads a message as words and never as a command (`skipSlashCommands` in
//! Claude Code 2.1.300), so nothing sent there switches it.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::json;
use steery_sdk::wire::harness::{Recap, Said, Say, SayMode, Text};
use steery_sdk::{Error, refusal};
use tokio::io::AsyncWriteExt;

use crate::registry;
use crate::transcripts::Store;

/// How long the socket has to take the two lines. The harness closes a connection that
/// has not sent a complete line in 30 seconds; these are ready before it opens.
const POST_WITHIN: Duration = Duration::from_secs(8);

/// How long the conversation's own recap may take: under the forty seconds the runtime
/// waits for it.
const RECAP_WITHIN: Duration = Duration::from_secs(35);

/// `say`: the words into the inbox of the session somebody is in. Claude Code's inbox
/// checks nothing idle, so words that may go in only while nothing runs never go through
/// it.
pub async fn say(root: &Path, say: Say) -> Result<Said, Error> {
    if say.mode == SayMode::Immediate {
        return Err(Error::refused(
            refusal::REFUSED,
            "Claude Code's inbox would queue them: it checks nothing idle",
        ));
    }
    let dir = registry::dir(root);
    let live = registry::live(&dir, &say.external_id)
        .ok_or_else(|| Error::refused(refusal::NO_DOOR, "no Claude Code session is in it now"))?;
    let socket = live.socket.ok_or_else(|| {
        Error::refused(
            refusal::NO_DOOR,
            "the Claude Code session in it has messaging off",
        )
    })?;
    post(
        &socket,
        registry::peer_token(&dir, live.pid),
        &say.words,
        POST_WITHIN,
    )
    .await?;
    Ok(Said { took: false })
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
                "Claude Code's inbox {} took no answer in time",
                socket.display()
            ),
        )),
    }
}

/// `recap`: what the conversation's own model writes on its own history when asked, in a
/// one-shot fork of it that keeps nothing: `claude --print --resume <id> --fork-session
/// --no-session-persistence`, the asked words on standard input. The fork resumes on the
/// conversation's own model and the system prompt Claude Code recorded for it; it reaches
/// no tool and no MCP server, and none of the machine's customisations (`--safe-mode`:
/// Steery's own hooks among them, so the fork is never seen as a conversation). Claude
/// Code asks its provider once (`CLAUDE_CODE_MAX_RETRIES=0`), so a refusal is said in its
/// own words within the bound rather than waited out. Nothing is written into the
/// conversation, and a turn running in it goes on. Refused `no-answer` with Claude Code's
/// own line where it wrote none.
pub async fn recap(store: &Store, recap: Recap) -> Result<Text, Error> {
    let unanswered = |why: String| Error::refused(refusal::NO_ANSWER, why);
    let binary =
        crate::binary().ok_or_else(|| unanswered("claude is not installed here".into()))?;
    let session = store.session(&recap.external_id).map_err(|_| {
        unanswered(format!(
            "Claude Code has no conversation {} here",
            recap.external_id
        ))
    })?;
    // Claude Code resumes a conversation by its id from any folder; its own folder, where
    // it is still there, keeps what the fork reads of its place the same.
    let cwd = session
        .cwd
        .filter(|cwd| cwd.is_dir())
        .unwrap_or_else(|| store.root().to_owned());
    let mut command = steery_driver_common::program::helper(&binary);
    command
        .args(["--print", "--output-format", "text", "--resume"])
        .arg(&recap.external_id)
        .args([
            "--fork-session",
            "--no-session-persistence",
            "--safe-mode",
            "--tools",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
        ])
        .env(crate::drive::RETRIES, "0")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A conversation filed under another configuration is resumed from there.
    if let Some(root) = session
        .transcripts
        .first()
        .and_then(|filed| crate::drive::config_root(filed))
        .filter(|root| *root != store.root())
    {
        command.env("CLAUDE_CONFIG_DIR", root);
    }
    let mut child = command
        .spawn()
        .map_err(|e| unanswered(format!("starting claude: {e}")))?;
    let mut stdin = child.stdin.take().expect("its standard input is piped");
    let asked = recap.asked.into_bytes();
    let ran = tokio::time::timeout(RECAP_WITHIN, async move {
        // Written beside the wait: a pipe that fills before Claude Code reads it would
        // hold both ends.
        let write = async move {
            let _ = stdin.write_all(&asked).await;
            drop(stdin);
        };
        let (_, output) = tokio::join!(write, child.wait_with_output());
        output
    })
    .await
    .map_err(|_| {
        unanswered(format!(
            "Claude Code wrote no recap within {} seconds",
            RECAP_WITHIN.as_secs()
        ))
    })?
    .map_err(|e| unanswered(format!("reading claude: {e}")))?;
    let text = String::from_utf8_lossy(&ran.stdout).trim().to_owned();
    if !ran.status.success() {
        let said = last_line(&text).or_else(|| last_line(&String::from_utf8_lossy(&ran.stderr)));
        return Err(unanswered(match said {
            Some(said) => format!("Claude Code wrote no recap: {said}"),
            None => format!("Claude Code wrote no recap: it exited {}", ran.status),
        }));
    }
    if text.is_empty() {
        return Err(unanswered("Claude Code wrote no recap".into()));
    }
    Ok(Text { text })
}

/// The last line of `text` that holds anything.
fn last_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
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
