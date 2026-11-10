//! Amp's threads, read through Amp's own CLI: `amp threads list --json` names the threads
//! made on this host (its installation id), and `amp threads export <id>` gives one thread
//! whole. Amp keeps no transcript on this host, so a session names none.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use steery_sdk::wire::harness::{List, Listed, Locate, Located, Session};
use steery_sdk::{Error, refusal};

use crate::home::{self, Failed};

/// The most threads one listing asks Amp for; a listing that fills it is not complete.
const LIMIT: usize = 1000;
/// How long `amp threads list` has, inside `sessions/list`'s 30 s.
const LIST_WITHIN: Duration = Duration::from_secs(15);
/// How long `amp threads export` has, inside `sessions/read`'s 10 s.
pub(crate) const EXPORT_WITHIN: Duration = Duration::from_secs(8);
/// How long the exports of one listing have together, after the list itself.
const EXPORTS_WITHIN: Duration = Duration::from_secs(10);
/// The most threads one listing exports, and how many at once.
const EXPORTS: usize = 50;
const AT_ONCE: usize = 8;
/// The most bytes one export may print.
pub(crate) const EXPORT_CAP: usize = 64 << 20;
/// The most bytes the list may print.
const LIST_CAP: usize = 16 << 20;
/// The client Amp names in a thread that `amp -x` (execute mode) ran with nobody at it.
const HEADLESS: &str = "CLI Execute Mode";

/// One thread as `amp threads list --json` names it.
#[derive(Debug, Deserialize)]
struct Listing {
    id: String,
    #[serde(default)]
    title: Option<String>,
    updated: String,
    #[serde(default)]
    tree: Option<String>,
}

/// What a thread is, once exported: a session, or a run nobody was at.
#[derive(Clone)]
enum Kind {
    Session(Box<Session>),
    Excluded,
}

/// Amp's threads on this host, and what each thread was at the time it was last listed:
/// a thread is exported again only once Amp says it changed.
#[derive(Default, Clone)]
pub(crate) struct Threads {
    kinds: Arc<Mutex<HashMap<String, (String, Kind)>>>,
}

impl Threads {
    /// `sessions/list`: the threads made on this host updated since `seen_since`. A thread
    /// not exported within this call is left for the next, and the listing is not complete.
    pub async fn list(&self, home: &Path, asked: List) -> Listed {
        let mut listed = Listed {
            sessions: Vec::new(),
            excluded: Vec::new(),
            complete: false,
            cursor: None,
            after: None,
            gone: Vec::new(),
        };
        let Some(binary) = home::binary() else {
            return listed;
        };
        // Amp never ran here: no thread was made on this host.
        let Some(installation) = home::installation(home) else {
            listed.complete = true;
            return listed;
        };
        let limit = LIMIT.to_string();
        let args = [
            "threads",
            "list",
            "--json",
            "--include-archived",
            "--limit",
            &limit,
            "--installation-id",
            &installation,
        ];
        let Ok(ran) = home::run(&binary, &args, None, &[], LIST_WITHIN, LIST_CAP).await else {
            return listed;
        };
        let Some(threads) = ran
            .success
            .then(|| serde_json::from_slice::<Vec<Listing>>(&ran.stdout).ok())
            .flatten()
        else {
            return listed;
        };
        let full = threads.len() >= LIMIT;
        let seen_since = asked.seen_since.unwrap_or(0);
        let threads: Vec<(Listing, u64)> = threads
            .into_iter()
            .filter_map(|thread| {
                let updated = millis(&thread.updated)?;
                (updated >= seen_since).then_some((thread, updated))
            })
            .collect();
        let unread: Vec<(String, String)> = {
            let kinds = self.kinds.lock().unwrap_or_else(|e| e.into_inner());
            threads
                .iter()
                .filter(|(thread, _)| {
                    kinds.get(&thread.id).is_none_or(|(updated, _)| *updated != thread.updated)
                })
                .map(|(thread, _)| (thread.id.clone(), thread.updated.clone()))
                .collect()
        };
        let read_all = self.export_all(&binary, &unread).await;
        let kinds = self.kinds.lock().unwrap_or_else(|e| e.into_inner());
        for (thread, updated) in &threads {
            let Some((at, kind)) = kinds.get(&thread.id) else {
                continue;
            };
            if *at != thread.updated {
                continue;
            }
            match kind {
                Kind::Excluded => listed.excluded.push(thread.id.clone()),
                Kind::Session(session) => {
                    let mut session = (**session).clone();
                    session.updated_at = *updated;
                    session.title = thread.title.clone().or(session.title);
                    session.cwd = session.cwd.or_else(|| thread.tree.as_deref().and_then(file_path));
                    listed.sessions.push(session);
                }
            }
        }
        listed.complete = !full && read_all;
        listed
    }

    /// Export each of `ids` (at most [`EXPORTS`], [`AT_ONCE`] at a time, within
    /// [`EXPORTS_WITHIN`]) and keep what each is, under the `updated` the list gave it.
    /// Whether every one was read.
    async fn export_all(&self, binary: &Path, ids: &[(String, String)]) -> bool {
        let gate = Arc::new(tokio::sync::Semaphore::new(AT_ONCE));
        let mut running = tokio::task::JoinSet::new();
        for (id, updated) in ids.iter().take(EXPORTS) {
            let (binary, id, updated, gate, kinds) = (
                binary.to_owned(),
                id.clone(),
                updated.clone(),
                Arc::clone(&gate),
                Arc::clone(&self.kinds),
            );
            running.spawn(async move {
                let _held = gate.acquire_owned().await;
                let Ok(thread) = export(&binary, &id).await else {
                    return false;
                };
                let kind = kind(&thread);
                kinds
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(id, (updated, kind));
                true
            });
        }
        let mut all = ids.len() <= EXPORTS;
        let joined = tokio::time::timeout(EXPORTS_WITHIN, async {
            while let Some(done) = running.join_next().await {
                all &= done.unwrap_or(false);
            }
        })
        .await;
        all && joined.is_ok()
    }

    /// `sessions/locate`: one thread, by its id, however old.
    pub async fn locate(&self, locate: Locate) -> Result<Located, Error> {
        let binary = home::binary().ok_or_else(|| {
            Error::refused(refusal::NOT_FOUND, "Amp is not installed here, so no thread is")
        })?;
        let thread = export(&binary, &locate.external_id).await?;
        Ok(Located {
            session: session(&thread),
        })
    }
}

/// What `thread`, exported, is.
fn kind(thread: &Value) -> Kind {
    if headless(thread) {
        Kind::Excluded
    } else {
        Kind::Session(Box::new(session(thread)))
    }
}

/// Whether nobody was at `thread`: `amp -x` ran it.
fn headless(thread: &Value) -> bool {
    thread.pointer("/env/initial/platform/client").and_then(Value::as_str) == Some(HEADLESS)
}

/// `thread`, exported, as a session.
fn session(thread: &Value) -> Session {
    let created = thread.get("created").and_then(Value::as_u64);
    Session {
        external_id: text(thread, "id").unwrap_or_default().to_owned(),
        cwd: thread
            .pointer("/env/initial/trees/0/uri")
            .and_then(Value::as_str)
            .and_then(file_path),
        title: text(thread, "title").filter(|t| !t.is_empty()).map(str::to_owned),
        created_at: created,
        updated_at: text(thread, "updatedAt").and_then(millis).or(created).unwrap_or(0),
        transcripts: Vec::new(),
        parent: None,
        app: false,
        sub_agents: Vec::new(),
    }
}

/// `amp threads export <id>`, read. `not-found` where Amp says the thread does not exist
/// (it is not this account's, or it is gone).
pub(crate) async fn export(binary: &Path, id: &str) -> Result<Value, Error> {
    if !id.starts_with("T-") || id.starts_with("T--") {
        return Err(Error::refused(refusal::NOT_FOUND, format!("{id} is not an Amp thread id")));
    }
    let ran = home::run(
        binary,
        &["threads", "export", id],
        None,
        &[],
        EXPORT_WITHIN,
        EXPORT_CAP,
    )
    .await
    .map_err(|failed| match failed {
        Failed::Over(_) => Error::refused(
            refusal::REFUSED,
            format!("{failed}: the driver does not read a thread this large"),
        ),
        failed => Error::refused(refusal::REFUSED, failed.to_string()),
    })?;
    if !ran.success {
        let line = ran.line().unwrap_or("amp threads export failed").to_owned();
        let word = if line.contains("does not exist") {
            refusal::NOT_FOUND
        } else {
            refusal::REFUSED
        };
        return Err(Error::refused(word, line));
    }
    serde_json::from_slice(&ran.stdout).map_err(|e| {
        Error::refused(refusal::REFUSED, format!("amp threads export printed no thread: {e}"))
    })
}

/// The path a `file://` URI names.
fn file_path(uri: &str) -> Option<PathBuf> {
    let path = uri.strip_prefix("file://")?;
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = path
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    let path = PathBuf::from(String::from_utf8(out).ok()?);
    path.is_absolute().then_some(path)
}

/// An RFC 3339 time in milliseconds since 1970.
pub(crate) fn millis(at: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc3339(at).ok()?;
    u64::try_from(at.timestamp_millis()).ok()
}

pub(crate) fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}
