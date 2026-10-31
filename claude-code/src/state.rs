//! `session/changed`: where each live Claude Code session stands, sent unasked whenever it
//! changes.
//!
//! Claude Code rewrites a session's registry entry ([`registry`]) as it goes: `status`
//! `busy` while a turn runs, `waiting` while it waits on its person mid-turn, with what it
//! waits on in `waitingFor`, `idle` once the turn is over, and `statusUpdatedAt`. The
//! model and effort it runs on are its transcript's newest answer's, read on as it grows
//! ([`crate::context`]), and a turn its transcript ended since the one running began is
//! over, whatever the registry was seen saying in between. Nothing of Claude Code's says
//! what its person is typing, what is queued, or which calls run, so those stay unsaid. A
//! session whose process is gone is said once more, with nothing running.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use steery_sdk::Notifier;
use steery_sdk::wire::harness::{Dialog, DialogKind, LiveState, LiveTurn, SessionChanged};

use crate::registry::{self, Registered};
use crate::transcripts::Store;

/// How often the registry is read again: Claude Code rewrites an entry as its status
/// changes, and reading every entry costs a few file reads.
const EVERY: Duration = Duration::from_millis(500);

/// How long a session whose transcript was not found is left before it is looked for
/// again: looking walks every folder of conversations.
const LOOK_AGAIN: Duration = Duration::from_secs(10);

/// What a program-started run (`claude -p`, any agent-SDK caller) calls itself: no
/// conversation Steery lists ([`crate::transcripts`]).
const HEADLESS: &str = "sdk-cli";

/// Follow every live session under `root` on a thread of its own, for as long as the
/// driver's process runs.
pub fn watch(root: &Path, store: Arc<Store>, notifier: Notifier) {
    let dir = registry::dir(root);
    std::thread::spawn(move || {
        let mut followed: HashMap<String, Followed> = HashMap::new();
        loop {
            look(&dir, &store, &notifier, &mut followed);
            std::thread::sleep(EVERY);
        }
    });
}

/// What was last said of one session, and where its transcript is.
#[derive(Default)]
struct Followed {
    said: Option<LiveState>,
    /// When the turn running began, while one runs.
    began: Option<u64>,
    transcript: Option<PathBuf>,
    /// When its transcript was last looked for and not found.
    missed: Option<Instant>,
}

/// One look at every live session: each one whose state changed is said, and each one
/// gone is said with nothing running, then forgotten.
fn look(dir: &Path, store: &Store, notifier: &Notifier, followed: &mut HashMap<String, Followed>) {
    let mut live: Vec<Registered> = Vec::new();
    for entry in registry::living(dir) {
        if entry.entrypoint.as_deref() != Some(HEADLESS)
            && !live.iter().any(|seen| seen.session_id == entry.session_id)
        {
            live.push(entry);
        }
    }
    followed.retain(|id, _| {
        let gone = !live.iter().any(|entry| &entry.session_id == id);
        if gone {
            say(notifier, id, LiveState::default());
        }
        !gone
    });
    for entry in live {
        let follow = followed.entry(entry.session_id.clone()).or_default();
        let state = follow.state(&entry, store);
        if follow.said.as_ref() != Some(&state) {
            say(notifier, &entry.session_id, state.clone());
            follow.said = Some(state);
        }
    }
}

impl Followed {
    /// Where the session stands now: its registry entry's status, and what its
    /// transcript's end says.
    fn state(&mut self, entry: &Registered, store: &Store) -> LiveState {
        if self.transcript.is_none()
            && self.missed.is_none_or(|missed| missed.elapsed() >= LOOK_AGAIN)
        {
            self.transcript = store.transcript(&entry.session_id).ok();
            self.missed = self.transcript.is_none().then(Instant::now);
        }
        let now = self
            .transcript
            .as_deref()
            .map(|transcript| store.now(transcript))
            .unwrap_or_default();
        let since = entry.status_updated_at.unwrap_or_else(millis_now);
        let running = matches!(entry.status.as_deref(), Some("busy" | "waiting"));
        let began = match self.began {
            Some(began) if now.ended.is_none_or(|ended| ended < began) => began,
            Some(_) => since.max(now.ended.unwrap_or_default()),
            None => since,
        };
        self.began = running.then_some(began);
        LiveState {
            turn: self.began.map(|began_at| LiveTurn {
                began_at,
                says: Vec::new(),
            }),
            idle: match entry.status.as_deref() {
                Some("idle") => Some(true),
                Some(_) => Some(false),
                None => None,
            },
            model: now.model,
            thinking: now.thinking,
            context_left: now.context_left,
            dialog: (entry.status.as_deref() == Some("waiting"))
                .then_some(entry.waiting_for.as_deref())
                .flatten()
                .map(|waiting| dialog(waiting, since)),
            ..LiveState::default()
        }
    }
}

/// What a waiting session shows its person, as `waitingFor` names it.
fn dialog(waiting: &str, since: u64) -> Dialog {
    let kind = match waiting {
        "permission prompt" | "sandbox request" | "worker request" => DialogKind::Confirm,
        "input needed" => DialogKind::Input,
        _ => DialogKind::Custom,
    };
    let mut title = waiting.to_owned();
    if let Some(first) = title.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    Dialog { kind, title, since }
}

fn say(notifier: &Notifier, external_id: &str, state: LiveState) {
    if let Ok(params) = serde_json::to_value(SessionChanged {
        external_id: external_id.to_owned(),
        state,
    }) {
        notifier.notify(steery_sdk::method::SESSION_CHANGED, params);
    }
}

/// Now, in milliseconds since 1970.
fn millis_now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or_default()
}
