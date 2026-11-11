//! A Copilot window that Steery can reach: a Copilot started with `--ui-server` serves its
//! own JSON-RPC on a loopback port beside its terminal, and its SDK's
//! `getForegroundSessionId` names the conversation it shows ("TUI+server mode",
//! `copilot-sdk/client.d.ts`, 1.0.109). The window is found from the in-use lock that it
//! writes in the conversation's folder: its process, then the loopback ports that process
//! listens on.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};

use super::{drive, rpc};

/// How long a window has to accept the connection and name its conversation.
const WITHIN: Duration = Duration::from_secs(3);

/// A connection to the window that shows a conversation. Its notifications are dropped:
/// the driver reads the event log instead.
pub struct Window {
    pub client: rpc::Client,
    pub pid: u32,
    pub port: u16,
}

/// The window that shows conversation `id`, whose folder is `folder`; none where no Copilot
/// that holds it serves a port, or none of them shows it.
pub async fn find(folder: &Path, id: &str) -> Option<Window> {
    let holders = drive::locks(folder);
    if holders.is_empty() {
        return None;
    }
    let pids: Vec<u32> = holders.iter().map(|(pid, _)| *pid).collect();
    let ports = tokio::task::spawn_blocking(move || ports(&pids))
        .await
        .ok()?;
    for (pid, port) in ports {
        if let Some(window) = open(pid, port, id).await {
            return Some(window);
        }
    }
    None
}

/// The loopback TCP ports each of `pids` listens on.
fn ports(pids: &[u32]) -> Vec<(u32, u16)> {
    let Ok(listening) = listeners::get_all() else {
        return Vec::new();
    };
    let mut ports: Vec<(u32, u16)> = listening
        .into_iter()
        .filter(|l| {
            pids.contains(&l.process.pid)
                && l.protocol == listeners::Protocol::TCP
                && l.state == listeners::SocketState::Listen
                && l.socket.ip().is_loopback()
        })
        .map(|l| (l.process.pid, l.socket.port()))
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// The window at `port` of process `pid`, where it shows conversation `id`.
async fn open(pid: u32, port: u16, id: &str) -> Option<Window> {
    let stream = tokio::time::timeout(WITHIN, tokio::net::TcpStream::connect(("127.0.0.1", port)))
        .await
        .ok()?
        .ok()?;
    let (read, write) = stream.into_split();
    let (client, _notes) = rpc::open(read, write);
    let shown = client
        .ask("session.getForeground", json!({}), WITHIN)
        .await
        .ok()?;
    (shown.get("sessionId").and_then(Value::as_str) == Some(id)).then_some(Window {
        client,
        pid,
        port,
    })
}
