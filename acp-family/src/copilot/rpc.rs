//! JSON-RPC 2.0 in `Content-Length` frames, as Copilot's own server and its SDK speak it,
//! over any byte stream: the standard input and output of `copilot --headless --stdio`, or
//! the loopback port of a Copilot started with `--ui-server`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// The largest frame the driver reads.
const FRAME_AT_MOST: usize = 4 << 20;

type Answer = Result<Value, String>;
type Waiting = Arc<Mutex<HashMap<u64, oneshot::Sender<Answer>>>>;
type Writer = Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;

/// One connection to a Copilot server: requests by id, and its notifications on the
/// receiver [`open`] gives. When the server closes the connection, every request still
/// waiting is answered that it ended, and the receiver ends.
pub struct Client {
    out: Writer,
    next: AtomicU64,
    waiting: Waiting,
    ended: Arc<AtomicBool>,
}

/// A client over `read` and `write`, and the server's notifications in the order it sent
/// them.
pub fn open(
    read: impl AsyncRead + Send + Unpin + 'static,
    write: impl AsyncWrite + Send + Unpin + 'static,
) -> (Client, mpsc::UnboundedReceiver<Value>) {
    let (notes, received) = mpsc::unbounded_channel();
    let out: Writer = Arc::new(tokio::sync::Mutex::new(Box::new(write)));
    let waiting: Waiting = Arc::default();
    let ended = Arc::new(AtomicBool::new(false));
    tokio::spawn(read_frames(
        BufReader::new(read),
        notes,
        Arc::clone(&out),
        Arc::clone(&waiting),
        Arc::clone(&ended),
    ));
    let client = Client {
        out,
        next: AtomicU64::new(1),
        waiting,
        ended,
    };
    (client, received)
}

impl Client {
    /// Whether the server closed the connection.
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Relaxed)
    }

    /// `method` with `params`: its result, else the server's error, or why it did not
    /// answer within `within`.
    pub async fn ask(&self, method: &str, params: Value, within: Duration) -> Answer {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        lock(&self.waiting).insert(id, answer);
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(why) = send(&self.out, &request).await {
            lock(&self.waiting).remove(&id);
            return Err(why);
        }
        match tokio::time::timeout(within, answered).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(format!(
                "Copilot's server closed before it answered {method}"
            )),
            Err(_) => {
                lock(&self.waiting).remove(&id);
                Err(format!(
                    "Copilot did not answer {method} within {} s",
                    within.as_secs()
                ))
            }
        }
    }
}

async fn send(out: &Writer, message: &Value) -> Result<(), String> {
    let body = message.to_string();
    let mut out = out.lock().await;
    let gone = |_| "Copilot's server closed the connection".to_owned();
    out.write_all(format!("Content-Length: {}\r\n\r\n{body}", body.len()).as_bytes())
        .await
        .map_err(gone)?;
    out.flush().await.map_err(gone)
}

/// Every frame the server sends: an answer goes to its request and a notification to
/// `notes`. A request of the server's own is refused at once: this client told the server
/// it handles nothing, and a request left unanswered would hold the server's turn.
async fn read_frames(
    mut read: impl AsyncBufReadExt + Unpin,
    notes: mpsc::UnboundedSender<Value>,
    out: Writer,
    waiting: Waiting,
    ended: Arc<AtomicBool>,
) {
    while let Some(frame) = frame(&mut read).await {
        match (frame.get("id"), frame.get("method")) {
            (Some(id), Some(_)) => {
                let refused = json!({"jsonrpc": "2.0", "id": id, "error": {
                    "code": -32601, "message": "this client handles no request"}});
                if send(&out, &refused).await.is_err() {
                    break;
                }
            }
            (None, Some(_)) => {
                let _ = notes.send(frame);
            }
            (Some(id), None) => {
                let Some(answer) = id.as_u64().and_then(|id| lock(&waiting).remove(&id)) else {
                    continue;
                };
                let _ = answer.send(
                    match (frame.get("result"), frame.pointer("/error/message")) {
                        (Some(result), _) => Ok(result.clone()),
                        (None, Some(Value::String(error))) => Err(format!("Copilot: {error}")),
                        _ => Err("Copilot answered with neither a result nor an error".into()),
                    },
                );
            }
            (None, None) => {}
        }
    }
    // Dropping the senders tells every request still waiting that the connection ended.
    ended.store(true, Ordering::Relaxed);
    lock(&waiting).clear();
}

/// The next frame's JSON; none at the end of the stream. A frame that is not JSON is
/// skipped.
async fn frame(read: &mut (impl AsyncBufReadExt + Unpin)) -> Option<Value> {
    loop {
        let mut length = None;
        loop {
            let mut line = String::new();
            if read.read_line(&mut line).await.ok()? == 0 {
                return None;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse::<usize>().ok();
            }
        }
        let length = length.filter(|length| *length <= FRAME_AT_MOST)?;
        let mut body = vec![0; length];
        read.read_exact(&mut body).await.ok()?;
        if let Ok(frame) = serde_json::from_slice(&body) {
            return Some(frame);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
