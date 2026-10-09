//! A client of `codex app-server`: JSON-RPC 2.0, one message per line, on the server's
//! standard input and output. The Codex app and its editor extensions run every
//! conversation through it; this driver speaks it to start, steer and stop turns, to list
//! models and to look at a thread's turns.
//!
//! A request the app server makes of its client (an approval, a question) is refused on
//! the spot: no person sits at this client, and Codex's own hooks have already put that
//! moment to Steery.

use std::collections::VecDeque;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Join, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// Why a call to the app server came to nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failed {
    /// The app server could not be started or spoken to.
    Io(String),
    /// It answered the request with an error: its own words.
    Rpc {
        method: String,
        code: Option<i64>,
        message: String,
    },
    /// It ended before it answered.
    Ended,
    /// It wrote something that is not JSON.
    Garbled(String),
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(why) => write!(f, "codex app-server: {why}"),
            Self::Rpc {
                method, message, ..
            } => write!(f, "{method}: {message}"),
            Self::Ended => f.write_str("codex app-server ended before it answered"),
            Self::Garbled(why) => write!(f, "codex app-server wrote no JSON: {why}"),
        }
    }
}

impl From<std::io::Error> for Failed {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

/// How long the app server has to exit once its standard input closes, so the
/// conversation it wrote is whole on disk, before it is killed.
pub(crate) const CLOSED_WITHIN: Duration = Duration::from_secs(10);

/// A running `codex app-server`, initialized.
pub(crate) struct AppServer {
    child: Child,
    wire: Wire,
    next: u64,
    /// Notifications read while waiting on a response, for a loop that waits on more:
    /// `turn/completed` may arrive while a steer is on the wire.
    pending: VecDeque<Value>,
    /// Every line the app server sends is written verbatim to this process's standard
    /// output first (a turn's output, which `turn/read` reads).
    echo: bool,
    /// The home the app server says it serves.
    home: Option<PathBuf>,
}

/// How messages travel: lines on an app server's own standard input and output, or
/// WebSocket messages on a running app server's control socket, whose bytes `codex
/// app-server proxy` carries over its standard input and output.
enum Wire {
    Lines {
        input: ChildStdin,
        output: Lines<BufReader<ChildStdout>>,
    },
    Socket(Box<WebSocketStream<Join<ChildStdout, ChildStdin>>>),
}

impl AppServer {
    /// Start `command app-server` and initialize it. `command` carries the binary and the
    /// environment the caller wants ([`crate::home::helper`] for a look that writes no
    /// conversation); its standard input and output are taken here.
    pub(crate) async fn open(command: tokio::process::Command, echo: bool) -> Result<Self, Failed> {
        Self::spawn(command, false, echo).await
    }

    /// The app server already running for this home, reached on its control socket
    /// through `codex app-server proxy`, which finds the socket itself.
    pub(crate) async fn running(command: tokio::process::Command) -> Result<Self, Failed> {
        Self::spawn(command, true, false).await
    }

    async fn spawn(
        mut command: tokio::process::Command,
        socket: bool,
        echo: bool,
    ) -> Result<Self, Failed> {
        command.arg("app-server");
        if socket {
            command.arg("proxy");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let input = child.stdin.take().ok_or(Failed::Ended)?;
        let output = child.stdout.take().ok_or(Failed::Ended)?;
        let wire = match socket {
            false => Wire::Lines {
                input,
                output: BufReader::new(output).lines(),
            },
            true => {
                let (stream, _) = tokio::time::timeout(
                    Duration::from_secs(5),
                    tokio_tungstenite::client_async(
                        "ws://localhost/",
                        tokio::io::join(output, input),
                    ),
                )
                .await
                .map_err(|_| Failed::Io("the control socket did not answer".into()))?
                .map_err(|e| Failed::Io(e.to_string()))?;
                Wire::Socket(Box::new(stream))
            }
        };
        let mut server = Self {
            child,
            wire,
            next: 0,
            pending: VecDeque::new(),
            echo,
            home: None,
        };
        let initialized = server
            .call(
                "initialize",
                json!({
                    "clientInfo": {"name": "steery", "title": "Steery", "version": env!("CARGO_PKG_VERSION")},
                    "capabilities": {"experimentalApi": true},
                }),
            )
            .await?;
        server.home = initialized
            .get("codexHome")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        server.notify("initialized", None).await?;
        Ok(server)
    }

    /// The home the app server says it serves, where it said one.
    pub(crate) fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// A request, and its result once the response to it arrives. Notifications that
    /// arrive first are kept for [`AppServer::recv`].
    pub(crate) async fn call(&mut self, method: &str, params: Value) -> Result<Value, Failed> {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        loop {
            let message = self.read().await?;
            if message.get("method").is_some() {
                if message.get("id").is_none() {
                    self.pending.push_back(message);
                }
                continue;
            }
            if message.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(Failed::Rpc {
                    method: method.to_owned(),
                    code: error.get("code").and_then(Value::as_i64),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("refused")
                        .to_owned(),
                });
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// A notification to the app server.
    pub(crate) async fn notify(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), Failed> {
        let mut message = json!({"jsonrpc": "2.0", "method": method});
        if let Some(params) = params {
            message["params"] = params;
        }
        self.send(&message).await
    }

    /// The next message the app server sent that no call took: one kept while a call
    /// waited, else the next line. Safe to cancel between messages.
    pub(crate) async fn recv(&mut self) -> Result<Value, Failed> {
        match self.pending.pop_front() {
            Some(message) => Ok(message),
            None => self.read().await,
        }
    }

    /// Close its standard input and give it [`CLOSED_WITHIN`] to exit, so what it wrote
    /// is whole on disk; kill it after.
    pub(crate) async fn close(self) {
        let Self {
            mut child, wire, ..
        } = self;
        match wire {
            Wire::Lines { input, .. } => drop(input),
            Wire::Socket(mut socket) => {
                let _ = socket.close(None).await;
            }
        }
        if tokio::time::timeout(CLOSED_WITHIN, child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }

    /// The next message, echoed where asked, a request of the app server's own refused.
    async fn read(&mut self) -> Result<Value, Failed> {
        let line = match &mut self.wire {
            Wire::Lines { output, .. } => output.next_line().await?.ok_or(Failed::Ended)?,
            Wire::Socket(socket) => loop {
                match socket.next().await.ok_or(Failed::Ended)? {
                    Ok(Message::Text(text)) => break text.to_string(),
                    Ok(Message::Ping(bytes)) => socket
                        .send(Message::Pong(bytes))
                        .await
                        .map_err(|e| Failed::Io(e.to_string()))?,
                    Ok(Message::Close(_)) => return Err(Failed::Ended),
                    Ok(_) => {}
                    Err(e) => return Err(Failed::Io(e.to_string())),
                }
            },
        };
        if self.echo {
            let mut out = std::io::stdout().lock();
            writeln!(out, "{line}")?;
            out.flush()?;
        }
        let message: Value =
            serde_json::from_str(&line).map_err(|e| Failed::Garbled(e.to_string()))?;
        if let (Some(id), Some(_)) = (message.get("id"), message.get("method")) {
            self.send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no person is at this client"}}))
                .await?;
        }
        Ok(message)
    }

    async fn send(&mut self, message: &Value) -> Result<(), Failed> {
        match &mut self.wire {
            Wire::Lines { input, .. } => {
                let mut line =
                    serde_json::to_vec(message).map_err(|e| Failed::Io(e.to_string()))?;
                line.push(b'\n');
                input.write_all(&line).await?;
                input.flush().await?;
            }
            Wire::Socket(socket) => socket
                .send(Message::Text(message.to_string().into()))
                .await
                .map_err(|e| Failed::Io(e.to_string()))?,
        }
        Ok(())
    }
}

/// Words as the app server takes a person's input.
pub(crate) fn said(words: &str) -> Value {
    json!([{"type": "text", "text": words}])
}
