//! Claude Code's own door into a live conversation: the inbox socket of the session its
//! registry names in it, and a recap its own model writes ([`recap`]).
//!
//! One connection carries one `{"type":"auth","token":…}` line, where the session
//! published a peer token, and one `{"type":"user","message":{"role":"user","content":…}}`
//! line, and the session takes the message as a person's: a turn starts if it is idle,
//! and a running one reads it between tool calls. Delivered by default; a session that
//! bypasses permission prompts holds it for its person, and a message never answers a
//! permission prompt. The socket says nothing back, so words handed over are unconfirmed
//! and the runtime looks for them with `receipt/find`. The session keeps its own model.

use std::path::Path;
use std::time::Duration;

use serde_json::json;
use steery_sdk::wire::harness::{
    Event, Read, Reader, ReaderTools, Recap, Said, Say, SayMode, Text, Voice,
};
use steery_sdk::{Error, refusal};
use tokio::io::AsyncWriteExt;

use crate::registry;
use crate::transcripts::Store;

/// How long the conversation's own model has to write a recap: inside the 40 seconds the
/// runtime waits for one.
const RECAP_WITHIN: Duration = Duration::from_secs(35);

/// The most of a conversation's words a recap is written from, its newest kept: well
/// inside the smallest window Claude Code gives a model.
const RECAP_READS: usize = 400_000;

/// The most of one call's result a recap is written from.
const RESULT_READS: usize = 400;

/// How long the socket has to take the two lines. The harness closes a connection that
/// has not sent a complete line in 30 seconds; these are ready before it opens.
const POST_WITHIN: Duration = Duration::from_secs(8);

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

/// `recap`: the conversation's own model, as its newest answer names it, run once as
/// `reader` runs Claude Code ([`crate::drive::reader`]: no tool, no MCP server, nothing of
/// the machine's own, nothing kept), reads the conversation's words as its transcript
/// holds them, its newest kept where they are long, and answers what is asked.
pub async fn recap(store: &Store, recap: Recap) -> Result<Text, Error> {
    let transcript = store
        .transcript(&recap.external_id)
        .map_err(|_| Error::refused(refusal::NO_ANSWER, "Claude Code has no such conversation here"))?;
    let now = crate::context::now(
        &crate::context::newest_first(&transcript),
        &crate::context::Window::default(),
    );
    let model = now.model.ok_or_else(|| {
        Error::refused(
            refusal::MODEL_MISSING,
            "the conversation's model has not answered in it yet",
        )
    })?;
    let thinking = now
        .thinking
        .filter(|level| crate::turn::efforts(&model).contains(&level.as_str()));
    let words = conversation(store, &recap.external_id)?;
    let plan = crate::drive::reader(Reader {
        system: None,
        model: Some(model),
        thinking,
        tools: ReaderTools::None,
        cwd: std::env::temp_dir(),
        asked: String::new(),
    })?;
    let (binary, args) = plan
        .argv
        .split_first()
        .ok_or_else(|| Error::refused(refusal::NO_ANSWER, "no command to run"))?;
    let mut command = steery_driver_common::program::helper(Path::new(binary));
    command
        .args(args)
        .current_dir(&plan.cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let unanswered = |why: String| Error::refused(refusal::NO_ANSWER, why);
    let mut child = command
        .spawn()
        .map_err(|e| unanswered(format!("starting {binary}: {e}")))?;
    let asked = format!(
        "This is a conversation with an agent, as its transcript holds it:\n\n{words}\n\n\
         Do not carry on with its work. {}",
        recap.asked
    );
    let written = tokio::time::timeout(RECAP_WITHIN, async {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(asked.as_bytes()).await?;
        }
        child.wait_with_output().await
    })
    .await
    .map_err(|_| unanswered("the conversation's model wrote no recap in time".into()))?
    .map_err(|e| unanswered(format!("running {binary}: {e}")))?;
    let text = String::from_utf8_lossy(&written.stdout).trim().to_owned();
    if !written.status.success() || text.is_empty() {
        return Err(unanswered(format!(
            "the conversation's model wrote no recap: {}",
            String::from_utf8_lossy(&written.stderr).trim()
        )));
    }
    Ok(Text { text })
}

/// What was said in a conversation, one paragraph per voice: the person's words, the
/// agent's, the harness's summaries, and the start of each call's result, the newest
/// [`RECAP_READS`] bytes of it.
fn conversation(store: &Store, id: &str) -> Result<String, Error> {
    let mut said = String::new();
    let mut cursor = None;
    loop {
        let read = store.read(Read {
            external_id: id.to_owned(),
            sub_agent: None,
            cursor,
            tail: None,
        })?;
        for event in read.events {
            let Event::Words { voice, text, .. } = event else {
                continue;
            };
            let (who, text) = match voice {
                Voice::Person => ("Person", text.as_str()),
                Voice::Agent => ("Agent", text.as_str()),
                Voice::Summary => ("Summary of what came before", text.as_str()),
                Voice::Tool => ("Call result", cut(&text, RESULT_READS)),
                _ => continue,
            };
            said.push_str(&format!("{who}: {text}\n\n"));
        }
        if !read.more {
            break;
        }
        cursor = Some(read.cursor);
    }
    let start = said.len().saturating_sub(RECAP_READS);
    let start = (start..=said.len())
        .find(|at| said.is_char_boundary(*at))
        .unwrap_or(said.len());
    Ok(said.split_off(start))
}

/// The first `most` bytes of `text`, at a character's edge.
fn cut(text: &str, most: usize) -> &str {
    let end = (0..=most.min(text.len()))
        .rev()
        .find(|at| text.is_char_boundary(*at))
        .unwrap_or(0);
    &text[..end]
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
