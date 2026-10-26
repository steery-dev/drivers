//! Claude Code's own way in, from its command line, its registry and its files: who holds a
//! conversation, whether it runs, how a turn starts and stops, how a person reopens it,
//! the models it runs, and its one-shot reader.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use steery_driver_common::receipt::{self, End};
use steery_sdk::wire::harness::{
    Act, Beyond, Door, Execution, Held, HeldBy, Holder, HolderStop, Launch, Look, Looked, Marked,
    Model, Models, ReadAs, Reader, ReaderPlan, ReaderTools, Receipt, ReceiptFind, ReceiptMark,
    Sample, Sampled, Start, Stdin, Stop, Stopped, Terminal, TerminalCommand, TurnState, Unlisted,
};
use steery_sdk::{Error, refusal};
use tokio::sync::Mutex;

use crate::agents;
use crate::registry::{self, Job};
use crate::transcripts::Store;
use crate::turn::{self, EFFORT, Turn};

/// How a Claude Code session in a terminal holds its conversation, as the line that asks
/// its person to let it be stopped says it.
const IN_A_TERMINAL: &str = "in a terminal";

/// What a person reads of a process holding a conversation that Claude Code's registry
/// does not name: Steery cannot speak to it or stop it.
const UNREACHED: &str = "Claude Code open where Steery can't reach: quit it";

/// What a terminal into a Claude Code background session says: the session holds the
/// conversation, and attaching to it starts nothing beside it.
const IN_BACKGROUND: &str =
    "Held by a Claude Code background session; it keeps running when you leave it";

/// How long one listing of `claude agents` answers `idle` for every conversation asked
/// about meanwhile: a sweep asks for each in turn, and each listing starts the harness.
const LISTING_KEPT: Duration = Duration::from_secs(2);

/// The longest `stop` waits for a background session to be gone after `claude stop`,
/// inside the runtime's bound for it.
const STOP_WITHIN: Duration = Duration::from_millis(2_500);

/// The Claude app takes a Claude Code conversation in at this address and opens it: the
/// one Claude Code's own `/desktop` opens.
fn app_url(id: &str) -> String {
    format!("claude://resume?session={id}")
}

/// Now, in milliseconds since 1970.
fn now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or_default()
}

/// `held`: the process Claude Code's registry names in the conversation holds it,
/// through the inbox it opens where it opens one, which keeps its own model. Each is
/// stopped exactly: a background session through Claude Code's own `claude stop`, which
/// keeps the conversation and asks nobody (its owner token); a terminal by the runtime's
/// stop ladder, once its person confirmed that very process. Claude Code opens its
/// transcript only to append to it, so the kernel says nothing between writes, and the
/// registry is what holds it. A process the registry does not name — a session with
/// messaging off, an app with it loaded — that holds or names the transcripts is
/// somewhere this driver cannot speak to or stop, and is never signalled.
pub fn held(root: &Path, store: &Store, held: Held) -> HeldBy {
    let dir = registry::dir(root);
    match registry::live(&dir, &held.external_id) {
        Some(live) => HeldBy::Holder(held_by(&held.external_id, live, registry::kernel::started)),
        None => HeldBy::Look(Looked {
            look: Look {
                transcripts: store
                    .transcript(&held.external_id)
                    .map(|file| vec![file])
                    .unwrap_or_default(),
                strays: false,
                beyond: vec![Beyond {
                    arg: None,
                    line: UNREACHED.into(),
                }],
            },
        }),
    }
}

/// [`held`], given the session the registry names in the conversation.
fn held_by(
    external_id: &str,
    live: registry::Live,
    started: impl Fn(u32) -> Option<u64>,
) -> Holder {
    let door = live.socket.is_some().then_some(Door::Keeps);
    let stop = match (u32::try_from(live.pid), live.job) {
        (Ok(pid), Some(job)) => match started(pid).and_then(|start| {
            Job {
                session: external_id.to_owned(),
                job,
                pid,
                start,
            }
            .token()
        }) {
            Some(token) => HolderStop::Owner(Some(token)),
            // A process whose start cannot be read is one ending.
            None => {
                HolderStop::Beyond("Claude Code background session is ending: try again".into())
            }
        },
        (Ok(pid), None) => match started(pid) {
            Some(process_start) => HolderStop::Process {
                pid,
                process_start,
                certain: true,
                how: IN_A_TERMINAL.into(),
            },
            None => HolderStop::Beyond(format!("Claude Code pid {pid} is ending: try again")),
        },
        (Err(_), _) => HolderStop::Beyond("Claude Code is in a process Steery can't name".into()),
    };
    Holder::Person { door, stop }
}

/// `claude agents --json` as last listed, shared by the `idle`s asked meanwhile.
#[derive(Default)]
pub struct Listing {
    last: Mutex<Option<Listed>>,
}

/// One listing: when it was read, when it was asked for (milliseconds since 1970), and
/// what it said.
struct Listed {
    read: Instant,
    asked: u64,
    said: Result<Vec<Value>, String>,
}

impl Listing {
    /// What `claude agents` lists, and when it was asked: one listing at a time, kept
    /// [`LISTING_KEPT`].
    async fn now(&self, binary: &Path) -> (u64, Result<Vec<Value>, String>) {
        let mut last = self.last.lock().await;
        if let Some(listed) = last.as_ref()
            && listed.read.elapsed() < LISTING_KEPT
        {
            return (listed.asked, listed.said.clone());
        }
        let asked = now();
        let said = agents::agents(binary).await;
        *last = Some(Listed {
            read: Instant::now(),
            asked,
            said: said.clone(),
        });
        (asked, said)
    }
}

/// `idle`: what `claude agents` lists and the registry names say of the conversation. A
/// conversation the listing names is what that listing says. One it does not list, whose
/// transcript nobody holds, is a conversation nobody is in (`if_unheld`). Anything else is
/// uncertain, and has no sample. Vacant where it runs nothing and no live session is
/// registered in it — a terminal killed mid-turn leaves exactly that; an idle registered
/// owner is somebody in it: its person may be reading the answer. A background session's
/// run began and ended where its job's timeline says ([`run_times`]); the model and effort
/// are its transcript's newest answer's.
pub async fn idle(root: &Path, store: &Store, listing: &Listing, held: Held) -> Sampled {
    let Some(binary) = crate::binary() else {
        return Sampled { sample: None };
    };
    let (observed_at, listed) = listing.now(&binary).await;
    let Ok(listed) = listed else {
        return Sampled { sample: None };
    };
    let id = &held.external_id;
    let wanted = HashSet::from([id.clone()]);
    let Ok(mut owners) = registry::strict(
        &registry::dir(root),
        &wanted,
        registry::kernel::exists,
        registry::kernel::started,
    ) else {
        return Sampled { sample: None };
    };
    let Some(mut found) = sample(&listed, id, owners.remove(id).as_ref(), observed_at) else {
        return Sampled { sample: None };
    };
    if let Some(job) = listed_once(&listed, id)
        .and_then(|entry| entry.get("id")?.as_str())
        .filter(|job| plain_job(job))
    {
        (found.started_at, found.ended_at) = run_times(root, job, found.state);
    }
    if let Ok(transcript) = store.transcript(id) {
        let now = crate::context::now(
            &crate::context::newest_first(&transcript),
            &crate::context::Window::default(),
        );
        found.model = now.model;
        found.thinking = now.thinking;
    }
    Sampled {
        sample: Some(found),
    }
}

/// The one row a listing has of conversation `id`.
fn listed_once<'a>(listed: &'a [Value], id: &str) -> Option<&'a Value> {
    let mut rows = listed
        .iter()
        .filter(|entry| entry.get("sessionId").and_then(Value::as_str) == Some(id));
    let first = rows.next()?;
    rows.next().is_none().then_some(first)
}

/// When a background session's latest run began and, where it no longer runs, ended, in
/// milliseconds: the times its job's timeline (`jobs/<job>/timeline.jsonl`, one `{at,
/// state}` row per change) gives the latest stretch of `working` rows and the row after.
fn run_times(root: &Path, job: &str, state: Execution) -> (Option<u64>, Option<u64>) {
    let Ok(timeline) = std::fs::read(root.join("jobs").join(job).join("timeline.jsonl")) else {
        return (None, None);
    };
    let rows: Vec<(u64, bool)> = timeline
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .filter_map(|row| {
            let at = crate::events::millis(row.get("at")?.as_str()?)?;
            Some((at, row.get("state")?.as_str()? == "working"))
        })
        .collect();
    let Some(last_working) = rows.iter().rposition(|(_, working)| *working) else {
        return (None, None);
    };
    let began = rows[..=last_working]
        .iter()
        .rev()
        .take_while(|(_, working)| *working)
        .last()
        .map(|(at, _)| *at);
    let ended = (state != Execution::Running)
        .then(|| rows.get(last_working + 1))
        .flatten()
        .map(|(at, _)| *at);
    (began, ended)
}

/// What a listing asked for at `observed_at` and the session the registry names say of
/// `id`: first as it stands whoever holds its transcript, then as it stands only where
/// nobody does. A turn that stopped on an error of Claude Code's own failed, in its words.
fn sample(
    listed: &[Value],
    id: &str,
    owner: Option<&registry::Strict>,
    observed_at: u64,
) -> Option<Sample> {
    let (running, if_unheld) = match agents::execution(listed, id, owner, false) {
        Some(running) => (running, false),
        None => (agents::execution(listed, id, owner, true)?, true),
    };
    let failed = (!running)
        .then(|| listed_once(listed, id).and_then(agents::end))
        .flatten()
        .filter(|ended| ended.state == TurnState::Failed);
    Some(Sample {
        state: match (running, &failed) {
            (true, _) => Execution::Running,
            (false, Some(_)) => Execution::Failed,
            (false, None) => Execution::Finished,
        },
        observed_at,
        started_at: None,
        ended_at: None,
        model: None,
        thinking: None,
        error: failed.and_then(|ended| ended.error),
        vacant: !running && owner.is_none(),
        if_unheld,
    })
}

/// `stop`: a background session's own `claude stop <job>`, which keeps the conversation,
/// then its registry entry and `claude agents` watched until it is gone, inside the
/// runtime's bound for a stop. A session in a terminal has no graceful stop of Claude
/// Code's own: the runtime's signals end it.
pub async fn stop(root: &Path, stop: Stop) -> Result<Stopped, Error> {
    let token = match stop.holder {
        Holder::Person {
            stop: HolderStop::Owner(Some(token)),
            ..
        } => token,
        Holder::Person {
            stop: HolderStop::Process { pid, .. },
            ..
        } => {
            return Ok(Stopped {
                stopped: false,
                proof: Some(format!(
                    "Claude Code in a terminal (pid {pid}) has no stop of its own; it is signalled"
                )),
            });
        }
        _ => {
            return Ok(Stopped {
                stopped: false,
                proof: None,
            });
        }
    };
    let job = Job::of(&token)
        .filter(|job| job.session == stop.external_id)
        .ok_or_else(|| Error::invalid("the owner token names no background session of it"))?;
    let deadline = tokio::time::Instant::now() + STOP_WITHIN;
    let dir = registry::dir(root);
    let refused = |line: String| Error::refused(refusal::NOT_STOPPED, line);
    let Some(before) = registry::strict_one(&dir, &job.session).map_err(refused)? else {
        return Ok(Stopped {
            stopped: false,
            proof: Some(format!(
                "background session {} is no longer registered",
                job.job
            )),
        });
    };
    if before.pid != job.pid
        || before.start.abs_diff(job.start) > steery_sdk::wire::process::SAME_START_MS
        || before.kind.as_deref() != Some("bg")
        || before.job.as_deref() != Some(&job.job)
    {
        return Ok(Stopped {
            stopped: false,
            proof: Some(format!(
                "background session {} no longer holds {}",
                job.job, job.session
            )),
        });
    }
    let binary = crate::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "claude is not installed here"))?;
    let left = || deadline.saturating_duration_since(tokio::time::Instant::now());
    let out = agents::said(&binary, &["stop", &job.job], left())
        .await
        .map_err(refused)?;
    if !out.status.success() {
        return Err(refused(format!(
            "`claude stop {}`: {}",
            job.job,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    loop {
        let registered = registry::strict_one(&dir, &job.session).map_err(refused)?;
        let listed = tokio::time::timeout(left(), agents::agents(&binary))
            .await
            .map_err(|_| refused("the background session still runs after `claude stop`".into()))?
            .map_err(refused)?;
        if registered.is_none() && !agents::active(&listed, &job.session) {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(refused(
                "the background session still runs after `claude stop`".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(Stopped {
        stopped: true,
        proof: Some(format!(
            "`claude stop {}` ended background session pid {} in {}",
            job.job, job.pid, job.session
        )),
    })
}

/// A point in a conversation's transcript: the file, its identity, the end of its last
/// whole record, and the background session that held it then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mark {
    path: PathBuf,
    dev: u64,
    ino: u64,
    bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
}

/// `receipt/mark`: where the conversation's newest transcript ends now, at its last whole
/// record, and the owner token of the background session that holds it, where one does.
pub fn mark(root: &Path, store: &Store, mark: ReceiptMark) -> Result<Marked, Error> {
    let session = store.session(&mark.external_id)?;
    let path =
        session.transcripts.last().cloned().ok_or_else(|| {
            Error::refused(refusal::SELECTION_UNSUPPORTED, "it has no transcript")
        })?;
    let end = receipt::end(&path).map_err(|e| {
        Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("{}: {e}", path.display()),
        )
    })?;
    let owner = registry::job(&registry::dir(root), &mark.external_id).and_then(|job| job.token());
    let mark = Mark {
        dev: end.dev,
        ino: end.ino,
        bytes: end.bytes,
        owner,
        path,
    };
    Ok(Marked {
        mark: serde_json::to_string(&mark).map_err(|e| Error::invalid(e.to_string()))?,
    })
}

/// `receipt/find`: the person's input carrying the delivery marker, among the whole
/// records the transcript gained past the mark. Two carrying it is refused.
pub fn find(find: ReceiptFind) -> Result<Receipt, Error> {
    let mark: Mark = serde_json::from_str(&find.mark)
        .map_err(|_| Error::invalid("the mark is no mark of this driver's"))?;
    let end = End {
        dev: mark.dev,
        ino: mark.ino,
        bytes: mark.bytes,
    };
    let found = receipt::found(&mark.path, &end, &find.delivery, input_carries)
        .map_err(|line| Error::refused(refusal::REFUSED, line))?;
    Ok(Receipt {
        found,
        owner: found.then_some(mark.owner).flatten(),
    })
}

/// Whether a record is a person's input whose text carries `marker` whole. Words sent
/// through a live session's inbox arrive as another session's message: `isMeta`, with a
/// `peer` origin. They are the input all the same.
fn input_carries(record: &Value, marker: &str) -> bool {
    let input = record.get("type").and_then(Value::as_str) == Some("user")
        && (record.get("isMeta").and_then(Value::as_bool) != Some(true)
            || record.pointer("/origin/kind").and_then(Value::as_str) == Some("peer"))
        && record.get("isSidechain").and_then(Value::as_bool) != Some(true)
        && record.get("isSynthetic").and_then(Value::as_bool) != Some(true);
    let Some(message) = record.get("message").filter(|_| input) else {
        return false;
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match message.get("content") {
        Some(Value::String(text)) => receipt::marked(text, marker),
        Some(Value::Array(parts)) => parts.iter().any(|part| {
            part.get("type").and_then(Value::as_str) == Some("text")
                && part
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| receipt::marked(text, marker))
        }),
        _ => false,
    }
}

/// Claude Code's model aliases, as its model configuration documents them
/// (https://code.claude.com/docs/en/model-config, "Model aliases"), each with what a person
/// reads and the model families it runs: an alias resolves to the newest model of its
/// family the account's provider serves, so the list never goes stale with a release.
/// `opus` first: it is what `default` resolves to on every account type the page names
/// ("Default model setting"). `default` itself is left out: it names no model. A fresh
/// Claude Code names none in its settings, so listing only that left a person Default alone
/// (Ali's first run, #product Slack thread 2026-10-19).
const ALIASES: [(&str, &str, &[&str]); 8] = [
    ("opus", "Opus", &["opus"]),
    ("sonnet", "Sonnet", &["sonnet"]),
    ("haiku", "Haiku", &["haiku"]),
    ("fable", "Fable", &["fable"]),
    (
        "best",
        "Best: Fable where it runs, else Opus",
        &["fable", "opus"],
    ),
    (
        "opusplan",
        "Opus to plan, Sonnet to work",
        &["opus", "sonnet"],
    ),
    ("opus[1m]", "Opus, 1M context", &["opus"]),
    ("sonnet[1m]", "Sonnet, 1M context", &["sonnet"]),
];

/// Where an administrator's managed files are (https://code.claude.com/docs/en/managed-settings,
/// "File-based"): `managed-settings.json`, then each `*.json` of `managed-settings.d/` in
/// alphabetical order, merged in that order, and `managed-mcp.json`.
pub(crate) fn managed_folder() -> &'static Path {
    if cfg!(target_os = "macos") {
        Path::new("/Library/Application Support/ClaudeCode")
    } else {
        Path::new("/etc/claude-code")
    }
}

/// The managed settings files, in the order Claude Code merges them.
pub(crate) fn managed_files() -> Vec<PathBuf> {
    let folder = managed_folder();
    let mut drop_ins: Vec<PathBuf> = std::fs::read_dir(folder.join("managed-settings.d"))
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "json")
                && !path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with('.'))
        })
        .collect();
    drop_ins.sort();
    let mut files = vec![folder.join("managed-settings.json")];
    files.extend(drop_ins);
    files
}

/// The models an administrator allows (`availableModels` in managed settings), the last
/// file that sets it winning; none where no file restricts them. A value that is not a list
/// allows none but Claude Code's default, as Claude Code enforces it.
fn allowed(files: &[PathBuf]) -> Option<Vec<String>> {
    files
        .iter()
        .filter_map(|file| {
            let value: Value = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
            let listed = value.get("availableModels")?;
            Some(
                listed
                    .as_array()
                    .map(|entries| {
                        entries
                            .iter()
                            .filter_map(|entry| entry.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        })
        .last()
}

/// Whether the allowlist entry `entry` lets `model` run, by the page's rule ("Restrict
/// model selection"): an entry is a family (`sonnet`), a version prefix
/// (`claude-sonnet-4-5`, also allowing later ids that extend it) or a full id. An alias is
/// allowed where any version of a family it runs is: Claude Code then runs the newest one
/// allowed.
fn permits(entry: &str, model: &str) -> bool {
    let model = model.strip_suffix("[1m]").unwrap_or(model);
    let families: &[&str] = ALIASES
        .iter()
        .find(|(alias, ..)| *alias == model)
        .map(|(_, _, families)| *families)
        .unwrap_or_default();
    let family = |family: &str| entry == family || entry.starts_with(&format!("claude-{family}"));
    if !families.is_empty() {
        return families.iter().any(|&one| family(one));
    }
    if entry.contains('-') {
        model == entry || model.starts_with(&format!("{entry}-"))
    } else {
        model.starts_with(&format!("claude-{entry}"))
    }
}

/// Whether Claude Code is logged in here, as `claude auth status` says it: its JSON's
/// `loggedIn`, else its exit (0 logged in, 1 not); none where it is not here or did not
/// answer within [`steery_driver_common::program::PROBE`]. `claude auth status --help`:
/// "Show authentication status". It writes nothing a login does not also write.
async fn signed_in() -> Option<bool> {
    let binary = crate::binary()?;
    let output = tokio::time::timeout(
        steery_driver_common::program::PROBE,
        steery_driver_common::program::helper(&binary)
            .args(["auth", "status", "--json"])
            .stdout(std::process::Stdio::piped())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let said = serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .and_then(|status| status.get("loggedIn")?.as_bool());
    said.or(match output.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    })
}

/// Whether an administrator's allowlist (`availableModels`, [`allowed`]) lets `model` run:
/// every model where none is set.
fn allows(allowed: Option<&[String]>, model: &str) -> bool {
    allowed.is_none_or(|allowed| allowed.iter().any(|entry| permits(entry, model)))
}

/// `models`: Claude Code names no models on its command line. It lists its documented
/// aliases ([`ALIASES`]), the one this machine chose in its own settings (`model` in
/// `settings.json`) first, each with the effort levels it takes ([`turn::efforts`]), any
/// other model it runs taking every level; only those an administrator's managed settings
/// allow. It says whether Claude Code is logged in ([`signed_in`]), so a login made in a
/// terminal lifts a sign-in failure on its own (Ali's first run, #product Slack thread
/// 2026-10-19), and watches the files a login, a model choice and a policy change rewrite.
pub async fn models(home: &Path, root: &Path) -> Models {
    let settings = root.join("settings.json");
    let thinking = |model: &str| -> Vec<String> {
        turn::efforts(model).iter().map(|&level| level.to_owned()).collect()
    };
    let chosen = std::fs::read(&settings)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("model")?.as_str().map(str::to_owned))
        .filter(|model| !model.is_empty() && model != "default");
    let managed = managed_files();
    let allowed = allowed(&managed);
    let mut models: Vec<Model> = ALIASES
        .iter()
        .map(|(alias, label, _)| Model {
            provider: None,
            model: (*alias).to_owned(),
            label: Some((*label).to_owned()),
            thinking: thinking(*alias),
            cost: None,
        })
        .collect();
    if let Some(chosen) = chosen {
        let first = match models.iter().position(|model| model.model == chosen) {
            Some(at) => models.remove(at),
            None => Model {
                provider: None,
                thinking: thinking(&chosen),
                model: chosen,
                label: None,
                cost: None,
            },
        };
        models.insert(0, first);
    }
    models.retain(|model| allows(allowed.as_deref(), &model.model));
    let mut watch = vec![
        settings,
        crate::state_file(home, root),
        // Where Claude Code keeps a login's credentials outside a keychain.
        root.join(".credentials.json"),
    ];
    watch.extend(managed);
    Models {
        models,
        unlisted: Some(Unlisted {
            thinking: EFFORT.map(String::from).to_vec(),
        }),
        watch,
        signed_in: signed_in().await,
    }
}

/// `start`: the plan of a turn this driver's own binary runs in its turn mode
/// ([`turn`]), the words on standard input. A continue or a fork names a conversation
/// Claude Code has here; a model is one an administrator's managed settings let run, as
/// `models` lists it or as any other model it runs; an effort is one of Claude's own
/// levels.
pub fn start(store: &Store, start: Start) -> Result<Launch, Error> {
    let binary = crate::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "claude is not installed here"))?;
    match (start.act, start.external_id.as_deref()) {
        (Act::Continue | Act::Fork, Some(id)) => {
            store.session(id)?;
        }
        (Act::Continue | Act::Fork, None) => {
            return Err(Error::invalid(
                "a continue or a fork names its conversation",
            ));
        }
        (Act::New, _) => {}
        (Act::Hand, _) => {
            return Err(Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "a hand over is no turn of Claude Code's",
            ));
        }
    }
    if let Some(thinking) = &start.thinking
        && !EFFORT.contains(&thinking.as_str())
    {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("Claude Code takes no effort {thinking}"),
        ));
    }
    if let Some(model) = &start.model
        && (model.trim().is_empty() || !allows(allowed(&managed_files()).as_deref(), model))
    {
        return Err(Error::refused(
            refusal::MODEL_MISSING,
            format!("Claude Code does not run {model} here"),
        ));
    }
    let me = std::env::current_exe()
        .map_err(|e| Error::refused(refusal::START_FAILED, format!("finding this driver: {e}")))?;
    let mut argv = vec![
        me.to_str()
            .ok_or_else(|| Error::refused(refusal::START_FAILED, "this driver's path is not text"))?
            .to_owned(),
        turn::MODE.to_owned(),
    ];
    argv.extend(
        Turn {
            binary,
            act: start.act,
            id: start.external_id,
            worktree: start.worktree,
            model: start.model,
            thinking: start.thinking,
            reads: start.reads,
        }
        .args()
        .map_err(|why| Error::refused(refusal::START_FAILED, why))?,
    );
    Ok(Launch {
        argv,
        env: BTreeMap::new(),
        cwd: start.cwd,
        stdin: Stdin::Words,
        inbox: false,
    })
}

/// `terminal`: `cd '<cwd>' && claude --resume <id>`, or `claude attach <job>` while a
/// background session holds it, with `CLAUDE_CONFIG_DIR='<root>'` before claude where
/// the conversation is filed under another configuration than Claude Code's default. A
/// conversation the Claude app started opens there too.
pub fn terminal(home: &Path, store: &Store, terminal: Terminal) -> Result<TerminalCommand, Error> {
    let id = &terminal.external_id;
    let unknown = || {
        Error::refused(
            refusal::NOT_FOUND,
            format!("Claude Code has no conversation {id} here"),
        )
    };
    if !uuid(id) {
        return Err(unknown());
    }
    let session = store.session(id)?;
    let filed = session.transcripts.first().ok_or_else(unknown)?;
    let root = config_root(filed).ok_or_else(unknown)?;
    if !terminal.cwd.is_absolute() {
        return Err(Error::invalid("the folder is no absolute path"));
    }
    let cwd = terminal
        .cwd
        .to_str()
        .ok_or_else(|| Error::invalid("the folder is not text"))?;
    let mut command = format!("cd {} && ", quoted(cwd));
    if root != crate::default_root(home) {
        let root = root
            .to_str()
            .ok_or_else(|| Error::invalid("Claude Code's configuration folder is not text"))?;
        command.push_str(&format!("CLAUDE_CONFIG_DIR={} ", quoted(root)));
    }
    let (args, line) = match &terminal.holder {
        Some(Holder::Person {
            stop: HolderStop::Owner(Some(token)),
            ..
        }) => match Job::of(token).filter(|job| plain_job(&job.job)) {
            Some(job) => (
                format!("attach {}", job.job),
                Some(IN_BACKGROUND.to_owned()),
            ),
            None => {
                return Err(Error::invalid(
                    "the owner token names no background session",
                ));
            }
        },
        Some(Holder::Person {
            stop: HolderStop::Process { pid, .. },
            ..
        }) => (
            format!("--resume {id}"),
            Some(format!(
                "Claude Code has it open already (pid {pid}); this opens a second session on it."
            )),
        ),
        Some(Holder::Person {
            stop: HolderStop::Beyond(how),
            ..
        }) => (format!("--resume {id}"), Some(how.clone())),
        _ => (format!("--resume {id}"), None),
    };
    command.push_str(&format!("claude {args}"));
    Ok(TerminalCommand {
        command,
        app: session.app.then(|| app_url(id)),
        line,
    })
}

/// The configuration root a transcript is filed under: `<root>/projects/<folder>/<id>.jsonl`.
fn config_root(transcript: &Path) -> Option<&Path> {
    let projects = transcript.parent()?.parent()?;
    (projects.file_name()? == "projects").then_some(())?;
    projects.parent()
}

/// Whether an id is a UUID as Claude Code names its conversations.
fn uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(at, byte)| match at {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// Whether a background session's short id is one plain word.
fn plain_job(job: &str) -> bool {
    !job.is_empty()
        && !job.starts_with('-')
        && job
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// One literal POSIX shell word.
pub(crate) fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// `reader`: `claude --print`, the words on standard input, on the model given, else the
/// one this machine chose in Claude Code's settings, at the `--effort` given, with nothing
/// of the machine's own: `--tools ""` disables every built-in tool (or `Read,Glob,Grep`
/// for a reader that looks, with `--permission-prompts none` refusing whatever would stop
/// to ask), `--strict-mcp-config` with an empty `--mcp-config` leaves no MCP server to reach,
/// `--safe-mode` drops memory files, skills, plugins, hooks — Steery's own among them,
/// so the run is never seen as a conversation — commands and agents, and
/// `--no-session-persistence` leaves nothing behind. The answer is the whole output.
pub fn reader(reader: Reader) -> Result<ReaderPlan, Error> {
    let binary = crate::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "claude is not installed here"))?;
    if !reader.cwd.is_absolute() {
        return Err(Error::invalid("the reader's folder is no absolute path"));
    }
    if let Some(thinking) = &reader.thinking
        && !EFFORT.contains(&thinking.as_str())
    {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("Claude Code takes no effort {thinking}"),
        ));
    }
    let mut argv: Vec<String> = vec![
        binary
            .to_str()
            .ok_or_else(|| Error::invalid("claude's path is not text"))?
            .to_owned(),
        "--print".into(),
        "--output-format".into(),
        "text".into(),
    ];
    if let Some(model) = reader.model {
        argv.extend(["--model".into(), model]);
    }
    if let Some(thinking) = reader.thinking {
        argv.extend(["--effort".into(), thinking]);
    }
    if let Some(system) = reader.system {
        argv.extend(["--system-prompt".into(), system]);
    }
    match reader.tools {
        ReaderTools::None => argv.extend(["--tools".into(), String::new()]),
        ReaderTools::Read => argv.extend(
            ["--tools", "Read,Glob,Grep", "--permission-prompts", "none"].map(String::from),
        ),
    }
    argv.extend(
        [
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--safe-mode",
            "--no-session-persistence",
        ]
        .map(String::from),
    );
    Ok(ReaderPlan {
        argv,
        env: BTreeMap::new(),
        cwd: reader.cwd,
        stdin: Stdin::Words,
        files: Vec::new(),
        read: ReadAs::Whole,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Live;
    use serde_json::json;

    /// The process Claude Code's registry names in a conversation holds it, through the
    /// inbox it opens where it opens one, which keeps its own model. Each is stopped
    /// exactly: a background session through `claude stop`, asking nobody; a terminal by
    /// the ladder, once its person confirmed that very process.
    #[test]
    fn who_holds_a_conversation_follows_its_registry() {
        let me = std::process::id();
        let session = |socket: Option<&str>, job: Option<&str>| Live {
            pid: i32::try_from(me).unwrap(),
            socket: socket.map(PathBuf::from),
            job: job.map(str::to_owned),
        };
        let started = |_| Some(1_790_000_000_000);
        assert!(
            matches!(
                held_by("s", session(Some("/tmp/cc-socks/41.sock"), None), started),
                Holder::Person {
                    door: Some(Door::Keeps),
                    stop: HolderStop::Process { certain: true, .. },
                }
            ),
            "through its inbox, and never resumed beside it"
        );
        let Holder::Person {
            door: None,
            stop: HolderStop::Process { pid, how, .. },
        } = held_by("s", session(None, None), started)
        else {
            panic!("in a terminal with no inbox: the ladder stops it once its person says so");
        };
        assert_eq!((pid, how.as_str()), (me, IN_A_TERMINAL));
        let Holder::Person {
            door: None,
            stop: HolderStop::Owner(Some(token)),
        } = held_by("s", session(None, Some("6920e989")), started)
        else {
            panic!("a background session is stopped by Claude Code's own `claude stop`");
        };
        assert_eq!(Job::of(&token).unwrap().job, "6920e989");
        assert!(matches!(
            held_by("s", session(None, Some("6920e989")), |_| None),
            Holder::Person {
                stop: HolderStop::Beyond(_),
                ..
            }
        ));
    }

    /// A listing that names nothing of a conversation nobody is registered in says it is
    /// idle only where nobody holds its transcript, and that nobody is in it.
    #[test]
    fn a_sample_stands_on_the_listing_and_the_registry() {
        let idle = sample(&[], "s", None, 7).expect("listed nowhere: nobody in it");
        assert_eq!(idle.state, Execution::Finished);
        assert!(idle.vacant && idle.if_unheld);
        assert_eq!(idle.observed_at, 7);
        let working = sample(
            &[json!({"sessionId": "s", "pid": 3, "state": "working"})],
            "s",
            None,
            7,
        )
        .unwrap();
        assert_eq!(working.state, Execution::Running);
        assert!(!working.vacant && !working.if_unheld);
        let owner = registry::Strict {
            pid: 3,
            start: 1,
            job: None,
            kind: Some("interactive".into()),
        };
        let done = sample(
            &[json!({"sessionId": "s", "pid": 3, "state": "done"})],
            "s",
            Some(&owner),
            7,
        )
        .unwrap();
        assert!(!done.vacant, "its person may be reading the answer");
        assert!(sample(&[], "s", Some(&owner), 7).is_none());
        let failed = sample(
            &[json!({"sessionId": "s", "state": "failed", "error": "Credit balance is too low"})],
            "s",
            None,
            7,
        )
        .unwrap();
        assert_eq!(
            (failed.state, failed.error.as_deref()),
            (Execution::Failed, Some("Credit balance is too low"))
        );
    }

    /// Only one person's input carrying the whole marker is a receipt; a marker quoted in
    /// an agent's words, a sub-agent's, or a longer name is none, and two are refused.
    #[test]
    fn a_receipt_is_one_input_carrying_the_whole_marker() {
        let user = |content: Value| {
            json!({"type": "user", "message": {"role": "user", "content": content}}).to_string()
        };
        let delta = |records: &[String]| format!("{}\n", records.join("\n"));
        let carried =
            |delta: &[u8], delivery: &str| receipt::carried(delta, delivery, input_carries);
        assert_eq!(
            carried(
                delta(&[user(json!("go steery-delivery:d1"))]).as_bytes(),
                "d1"
            ),
            Ok(true)
        );
        assert_eq!(
            carried(
                delta(&[user(
                    json!([{"type": "text", "text": "steery-delivery:d1."}])
                )])
                .as_bytes(),
                "d1"
            ),
            Ok(false),
            "followed by a dot it is a longer name"
        );
        assert_eq!(
            carried(
                delta(&[user(json!("steery-delivery:d10"))]).as_bytes(),
                "d1"
            ),
            Ok(false)
        );
        let peer = json!({"type": "user", "isMeta": true, "origin": {"kind": "peer"},
            "message": {"role": "user", "content": "steery-delivery:d1"}})
        .to_string();
        assert_eq!(carried(delta(&[peer]).as_bytes(), "d1"), Ok(true));
        let side = json!({"type": "user", "isSidechain": true,
            "message": {"role": "user", "content": "steery-delivery:d1"}})
        .to_string();
        let agent = json!({"type": "assistant",
            "message": {"role": "assistant", "content": "steery-delivery:d1"}})
        .to_string();
        assert_eq!(carried(delta(&[side, agent]).as_bytes(), "d1"), Ok(false));
        let twice = delta(&[
            user(json!("steery-delivery:d1")),
            user(json!("steery-delivery:d1")),
        ]);
        assert!(carried(twice.as_bytes(), "d1").is_err());
        assert_eq!(
            carried(user(json!("steery-delivery:d1")).as_bytes(), "d1"),
            Ok(false),
            "a record not yet ended is left for the next look"
        );
    }

    /// Every switch the reader claims is passed, and nothing that writes or skips a
    /// check is.
    #[test]
    fn claude_code_is_started_with_nothing_of_the_machines_own() {
        if crate::binary().is_none() {
            return;
        }
        for tools in [ReaderTools::None, ReaderTools::Read] {
            let plan = reader(Reader {
                system: Some("read".into()),
                model: Some("claude-haiku-4-5".into()),
                thinking: Some("low".into()),
                tools,
                cwd: PathBuf::from("/tmp"),
                asked: "q".into(),
            })
            .unwrap();
            let args = &plan.argv;
            for expected in [
                "--print",
                "--tools",
                "--strict-mcp-config",
                "--mcp-config",
                "--safe-mode",
                "--no-session-persistence",
                "--system-prompt",
                "--effort",
            ] {
                assert!(args.iter().any(|arg| arg == expected), "missing {expected}");
            }
            let at = |flag: &str| &args[args.iter().position(|arg| arg == flag).unwrap() + 1];
            assert_eq!(at("--model"), "claude-haiku-4-5");
            assert_eq!(at("--mcp-config"), r#"{"mcpServers":{}}"#);
            match tools {
                ReaderTools::None => assert_eq!(at("--tools"), "", "its \"no tools\""),
                ReaderTools::Read => assert_eq!(at("--tools"), "Read,Glob,Grep"),
            }
            for forbidden in [
                "--dangerously-skip-permissions",
                "--add-dir",
                "--allowedTools",
                "--allowed-tools",
                "--ide",
                "--chrome",
            ] {
                assert!(
                    !args.iter().any(|arg| arg == forbidden),
                    "passes {forbidden}"
                );
            }
            assert_eq!(plan.stdin, Stdin::Words);
        }
    }

    #[test]
    fn a_conversation_is_filed_under_its_configuration() {
        assert_eq!(
            config_root(Path::new("/h/.claude/projects/-w/x.jsonl")),
            Some(Path::new("/h/.claude"))
        );
        assert_eq!(config_root(Path::new("/h/.claude/other/-w/x.jsonl")), None);
        assert!(uuid("6920e989-aa9e-467c-9b64-569ba2ca6059"));
        assert!(!uuid("--fork-session-aa9e-467c-9b64-569ba2ca"));
        assert!(!plain_job("-w") && plain_job("6b44d2be"));
        assert_eq!(quoted("it's"), r"'it'\''s'");
    }

    /// What a person runs is what Claude Code gets: the folder entered, the configuration
    /// the conversation is filed under, the conversation, each one literal word whatever
    /// it holds.
    #[test]
    fn the_command_runs_literally_in_its_folder_and_configuration() {
        const ID: &str = "11111111-1111-4111-8111-111111111111";
        let bench = crate::registry::tests::Scratch::new("terminal");
        let cwd = bench.path().join("it's $HOME; work");
        let root = bench.path().join("claude's $(touch nope)");
        let bin = bench.path().join("bin");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        crate::tests::stand_in(
            &bin.join("claude"),
            "#!/bin/sh\npwd\necho \"$CLAUDE_CONFIG_DIR\"\nfor arg in \"$@\"; do echo \"$arg\"; done\n",
        );
        let transcript = root.join(format!("projects/w/{ID}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                json!({"type": "user", "cwd": cwd, "entrypoint": "cli", "sessionId": ID,
                    "message": {"role": "user", "content": "go"}})
            ),
        )
        .unwrap();
        let store = Store::new(root.clone());
        let opened = terminal(
            bench.path(),
            &store,
            Terminal {
                external_id: ID.into(),
                cwd: cwd.clone(),
                holder: None,
            },
        )
        .unwrap();
        assert_eq!(
            opened.app, None,
            "a terminal's conversation opens in a terminal"
        );
        let ran = std::process::Command::new("/bin/sh")
            .args(["-c", &opened.command])
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert_eq!(
            String::from_utf8(ran.stdout).unwrap(),
            format!("{}\n{}\n--resume\n{ID}\n", cwd.display(), root.display())
        );
        assert!(!cwd.join("nope").exists());
        for (id, folder) in [(ID, "relative"), ("--fork", "/w"), ("a b", "/w")] {
            assert!(
                terminal(
                    bench.path(),
                    &store,
                    Terminal {
                        external_id: id.into(),
                        cwd: PathBuf::from(folder),
                        holder: None,
                    },
                )
                .is_err(),
                "{id} in {folder}"
            );
        }
    }
}
