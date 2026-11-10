//! One Cursor chat as Cursor keeps it: `store.db`, a SQLite file. Its `blobs` table holds
//! protobuf and JSON blobs by the hex of their sha256. Its `meta` row `0` is the hex of a
//! JSON object that names the newest root blob (`latestRootBlobId`). The root lists the
//! JSON messages that the model reads and the turns; each turn lists its steps. A blob
//! never changes: a change writes new blobs and a new root. The driver opens the file
//! read-only and never writes.

use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

/// The largest blob the driver reads; a larger one is read as missing.
const BLOB_MAX: usize = steery_driver_common::lines::RECORD;

/// The byte of a WAL index that every SQLite connection to the file holds a shared lock
/// on while it is open: `UNIX_SHM_DMS` of SQLite's `os_unix.c`, `(22 + 8) * 4 + 8`.
const SHM_DMS: libc::off_t = 128;

/// Another process has the store at `path` open: it holds SQLite's lock on the WAL index.
/// A lock of this process's own is never reported, so the driver's reads do not count.
pub fn held(path: &Path) -> bool {
    let mut shm = path.as_os_str().to_owned();
    shm.push("-shm");
    let Ok(file) = std::fs::File::open(shm) else {
        return false;
    };
    // SAFETY: an all-zero `flock` is a valid value of the C struct.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = libc::F_WRLCK as _;
    lock.l_whence = libc::SEEK_SET as _;
    lock.l_start = SHM_DMS;
    lock.l_len = 1;
    // SAFETY: the descriptor is open for the call, and `lock` outlives it.
    let asked = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut lock) };
    asked == 0 && i32::from(lock.l_type) != i32::from(libc::F_UNLCK)
}

/// An open chat.
pub struct Chat {
    db: Connection,
}

impl Chat {
    /// Opens `path` read-only. Where the log's shared-memory index stands beside the file,
    /// the newest writes can be in the write-ahead log: read through them. Without that
    /// index nobody writes, and a read that takes no lock needs no file of its own.
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut shm = path.as_os_str().to_owned();
        shm.push("-shm");
        let mode = if Path::new(&shm).exists() {
            "mode=ro"
        } else {
            "immutable=1"
        };
        let uri = format!("file:{}?{mode}", escaped(path)?);
        let db = Connection::open_with_flags(
            uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|why| why.to_string())?;
        db.busy_timeout(Duration::from_secs(1))
            .map_err(|why| why.to_string())?;
        Ok(Self { db })
    }

    /// The JSON of the `meta` row `0`. None before Cursor writes it: a chat it only began.
    fn meta(&self) -> Result<Option<Value>, String> {
        let made: bool = self
            .db
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
                [],
                |row| row.get(0),
            )
            .map_err(|why| why.to_string())?;
        if !made {
            return Ok(None);
        }
        let hex: Option<String> = self
            .db
            .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|why| why.to_string())?;
        let Some(hex) = hex else {
            return Ok(None);
        };
        let bytes = unhex(&hex).ok_or("its meta row is not hex")?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|why| why.to_string())
    }

    /// The blob whose sha256 is `id`, where the file holds it and it is no larger than
    /// [`BLOB_MAX`].
    pub fn blob(&self, id: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.db
            .query_row(
                "SELECT data FROM blobs WHERE id = ?1 AND length(data) <= ?2",
                rusqlite::params![hex(id), BLOB_MAX as i64],
                |row| row.get(0),
            )
            .optional()
            .map_err(|why| why.to_string())
    }

    /// The newest root, as the meta row names it. A chat that Cursor only began has no
    /// meta row yet, then names the empty id until its first root is written (Cursor
    /// 2026.11.05 `index.js`): it has no turns.
    pub fn root(&self) -> Result<Root, String> {
        let Some(meta) = self.meta()? else {
            return Ok(Root::default());
        };
        let id = meta
            .get("latestRootBlobId")
            .and_then(Value::as_str)
            .and_then(unhex)
            .ok_or("its meta row names no root")?;
        if id.is_empty() {
            return Ok(Root::default());
        }
        let bytes = self.blob(&id)?.ok_or("its root blob is missing")?;
        Ok(Root::read(&bytes))
    }

    /// The JSON message that blob `id` holds.
    pub fn message(&self, id: &[u8]) -> Result<Option<Value>, String> {
        Ok(self
            .blob(id)?
            .and_then(|bytes| serde_json::from_slice(&bytes).ok()))
    }
}

/// `agent.v1.ConversationStateStructure`, of what the driver reads.
#[derive(Debug, Default)]
pub struct Root {
    /// The JSON messages that the model reads now, oldest first, by blob id.
    pub messages: Vec<Vec<u8>>,
    /// The turns, oldest first, by blob id.
    pub turns: Vec<Vec<u8>>,
    /// A call that Cursor started and that has not returned.
    pub pending: bool,
    /// The summaries that replaced earlier messages, by blob id. Each one keeps the
    /// messages that it replaced.
    pub archives: Vec<Vec<u8>>,
    /// The workspace's `file://` URIs.
    pub workspaces: Vec<String>,
}

impl Root {
    fn read(bytes: &[u8]) -> Self {
        let mut root = Self::default();
        for (number, field) in fields(bytes) {
            match (number, field) {
                (1, Field::Bytes(id)) => root.messages.push(id.to_vec()),
                (8, Field::Bytes(id)) => root.turns.push(id.to_vec()),
                (4, _) => root.pending = true,
                (13, Field::Bytes(id)) => root.archives.push(id.to_vec()),
                (9, Field::Bytes(uri)) => {
                    root.workspaces
                        .extend(std::str::from_utf8(uri).ok().map(str::to_owned));
                }
                _ => {}
            }
        }
        root
    }
}

/// `agent.v1.ConversationSummaryArchive`: the blob ids of the messages that a summary
/// replaced.
pub fn archived(bytes: &[u8]) -> Vec<Vec<u8>> {
    fields(bytes)
        .filter_map(|(number, field)| match (number, field) {
            (1, Field::Bytes(id)) => Some(id.to_vec()),
            _ => None,
        })
        .collect()
}

/// `agent.v1.AgentConversationTurnStructure`: one turn that a person's words started.
#[derive(Debug, Default)]
pub struct Turn {
    /// The blob of the person's message.
    pub user: Vec<u8>,
    /// The blobs of its steps, in order.
    pub steps: Vec<Vec<u8>>,
    /// Cursor's id for the request.
    pub request: Option<String>,
}

impl Turn {
    /// The turn that blob `bytes` holds; none where it is no turn of the agent (a person's
    /// own shell command).
    pub fn read(bytes: &[u8]) -> Option<Self> {
        let inner = fields(bytes).find_map(|(number, field)| match (number, field) {
            (1, Field::Bytes(inner)) => Some(inner),
            _ => None,
        })?;
        let mut turn = Self::default();
        for (number, field) in fields(inner) {
            match (number, field) {
                (1, Field::Bytes(id)) => turn.user = id.to_vec(),
                (2, Field::Bytes(id)) => turn.steps.push(id.to_vec()),
                (3, Field::Bytes(id)) => turn.request = string(id),
                _ => {}
            }
        }
        Some(turn)
    }
}

/// `agent.v1.UserMessage`: what the person typed.
#[derive(Debug, Default)]
pub struct User {
    pub text: String,
    pub id: Option<String>,
    pub at: Option<u64>,
}

impl User {
    pub fn read(bytes: &[u8]) -> Self {
        let mut user = Self::default();
        for (number, field) in fields(bytes) {
            match (number, field) {
                (1, Field::Bytes(text)) => user.text = string(text).unwrap_or_default(),
                (2, Field::Bytes(id)) => user.id = string(id),
                (25 | 26, Field::Int(at)) => user.at = user.at.or(Some(at)),
                _ => {}
            }
        }
        user
    }
}

/// `agent.v1.ConversationStep`: one step of a turn.
#[derive(Debug)]
pub enum Step {
    /// The agent's words: when they began and, while more steps follow, when they ended.
    Said {
        text: String,
        began: Option<u64>,
        ended: Option<u64>,
    },
    /// Its reasoning.
    Thought {
        text: String,
        began: Option<u64>,
        ended: Option<u64>,
    },
    /// A call, which Cursor writes once it returned.
    Call(Call),
}

/// `agent.v1.ToolCall`.
#[derive(Debug, Default)]
pub struct Call {
    pub id: Option<String>,
    /// The number of its field of the `tool` oneof: which tool it is.
    pub kind: u32,
    pub began: Option<u64>,
    pub ended: Option<u64>,
    /// Whether it succeeded; none where it holds no result.
    pub ok: Option<bool>,
    /// An MCP call's server and tool (`mcp_tool_call`'s `args`).
    pub mcp: Option<(String, String)>,
}

/// `ToolCall`'s fields beside its `tool` oneof.
const CALL_FIELDS: [u32; 4] = [54, 57, 59, 60];
/// `ToolCall.mcp_tool_call`.
const MCP: u32 = 15;

impl Step {
    pub fn read(bytes: &[u8]) -> Option<Self> {
        let (number, inner) = fields(bytes).find_map(|(number, field)| match field {
            Field::Bytes(inner) => Some((number, inner)),
            Field::Int(_) => None,
        })?;
        match number {
            1 | 3 => {
                let (mut text, mut times) = (String::new(), Vec::new());
                for (number, field) in fields(inner) {
                    match (number, field) {
                        (1, Field::Bytes(said)) => text = string(said).unwrap_or_default(),
                        (_, Field::Int(at)) if number >= 2 => times.push((number, at)),
                        _ => {}
                    }
                }
                // Words stamp when they began (2) and ended (3); a thought says its length (2),
                // then when it began (3) and ended (4).
                let at = |n: u32| times.iter().find(|(number, _)| *number == n).map(|t| t.1);
                Some(if number == 1 {
                    Step::Said {
                        text,
                        began: at(2),
                        ended: at(3),
                    }
                } else {
                    Step::Thought {
                        text,
                        began: at(3),
                        ended: at(4),
                    }
                })
            }
            2 => {
                let mut call = Call::default();
                for (number, field) in fields(inner) {
                    match (number, field) {
                        (57, Field::Bytes(id)) => call.id = string(id),
                        (59, Field::Int(at)) => call.began = Some(at),
                        (60, Field::Int(at)) => call.ended = Some(at),
                        (number, Field::Bytes(tool)) if !CALL_FIELDS.contains(&number) => {
                            call.kind = number;
                            call.ok = outcome(number, tool);
                            if number == MCP {
                                call.mcp = mcp(tool);
                            }
                        }
                        _ => {}
                    }
                }
                Some(Step::Call(call))
            }
            _ => None,
        }
    }
}

/// Whether a tool's call (`{args = 1, result = 2}`) succeeded: its result's oneof is
/// `success` (1), and an MCP success is no error.
fn outcome(kind: u32, tool: &[u8]) -> Option<bool> {
    let result = fields(tool).find_map(|(number, field)| match (number, field) {
        (2, Field::Bytes(result)) => Some(result),
        _ => None,
    })?;
    let (number, success) = fields(result).find_map(|(number, field)| match field {
        Field::Bytes(inner) => Some((number, inner)),
        Field::Int(_) => None,
    })?;
    if number != 1 {
        return Some(false);
    }
    // `McpSuccess.is_error`.
    let failed = kind == MCP
        && fields(success).any(|(number, field)| matches!((number, field), (2, Field::Int(1))));
    Some(!failed)
}

/// `McpArgs`: the server (`provider_identifier`, 4) and the tool (`tool_name`, 5).
fn mcp(tool: &[u8]) -> Option<(String, String)> {
    let args = fields(tool).find_map(|(number, field)| match (number, field) {
        (1, Field::Bytes(args)) => Some(args),
        _ => None,
    })?;
    let text = |n: u32| {
        fields(args).find_map(|(number, field)| match (number, field) {
            (number, Field::Bytes(text)) if number == n => string(text),
            _ => None,
        })
    };
    Some((text(4)?, text(5)?))
}

/// One protobuf field's value: a varint, or a length-delimited run of bytes. Fixed-width
/// values are skipped.
#[derive(Debug, Clone, Copy)]
pub enum Field<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

/// The fields of a protobuf message in order. A malformed field ends them.
pub fn fields(bytes: &[u8]) -> impl Iterator<Item = (u32, Field<'_>)> {
    let mut at = 0;
    std::iter::from_fn(move || {
        loop {
            let key = varint(bytes, &mut at)?;
            let number = u32::try_from(key >> 3).ok()?;
            match key & 7 {
                0 => return Some((number, Field::Int(varint(bytes, &mut at)?))),
                2 => {
                    let len = usize::try_from(varint(bytes, &mut at)?).ok()?;
                    let end = at.checked_add(len).filter(|end| *end <= bytes.len())?;
                    let value = &bytes[at..end];
                    at = end;
                    return Some((number, Field::Bytes(value)));
                }
                1 => at = at.checked_add(8).filter(|end| *end <= bytes.len())?,
                5 => at = at.checked_add(4).filter(|end| *end <= bytes.len())?,
                _ => return None,
            }
        }
    })
}

fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn string(bytes: &[u8]) -> Option<String> {
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(text.get(at..at + 2)?, 16).ok())
        .collect()
}

/// `path` as the path of a SQLite URI: `?`, `#` and `%` escaped.
fn escaped(path: &Path) -> Result<String, String> {
    let text = path.to_str().ok_or("its path is not UTF-8")?;
    Ok(text
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23"))
}
