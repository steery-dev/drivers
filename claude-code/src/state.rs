//! `session/changed`: where each live Claude Code session stands, sent unasked whenever it
//! changes.
//!
//! Claude Code rewrites a session's registry entry ([`registry`]) as it goes: `status`
//! `busy` while a turn runs, `waiting` while it waits on its person mid-turn, with what it
//! waits on in `waitingFor`, `idle` once the turn is over, and `statusUpdatedAt`. The
//! model and effort it runs on are its transcript's newest answer's, read on as it grows
//! ([`crate::context`]), and a turn its transcript ended since the one running began is
//! over, whatever the registry was seen saying in between. The says a turn took are the
//! ones whose words went in by its end ([`crate::says`]), and a turn that took any is
//! said settled once it is over. The transcript also says when a running turn waits to try
//! a failed request again (`retrying`, until the wait its `api_error` record names is
//! over), how much background work its calls left running or ended untold (`jobs`), and
//! which call a permission prompt asks about: the newest with no result. Nothing of
//! Claude Code's says what its person is typing, what is queued, or that it compacts while
//! it does (its `compact_boundary` is written once it is done), so those stay unsaid. A
//! session whose process is gone is said once more, with nothing running and its turn
//! settled.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use steery_sdk::Notifier;
use steery_sdk::wire::harness::{
    Dialog, DialogKind, LiveSettled, LiveState, LiveTurn, SessionChanged,
};

use crate::registry::{self, Registered};
use crate::says::Says;
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

/// The most settled turns said, the newest kept (PROTOCOL.md §4.3).
const SETTLED_KEPT: usize = 16;

/// Follow every live session under `root` on a thread of its own, for as long as the
/// driver's process runs.
pub fn watch(root: &Path, store: Arc<Store>, says: Arc<Says>, notifier: Notifier) {
    let dir = registry::dir(root);
    std::thread::spawn(move || {
        let mut followed: HashMap<String, Followed> = HashMap::new();
        loop {
            look(&dir, &store, &says, &notifier, &mut followed);
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
    /// Turns that took Steery's words and are over, oldest first.
    settled: Vec<LiveSettled>,
}

/// One look at every live session: each one whose state changed is said, and each one
/// gone is said with nothing running, then forgotten.
fn look(
    dir: &Path,
    store: &Store,
    says: &Says,
    notifier: &Notifier,
    followed: &mut HashMap<String, Followed>,
) {
    let mut live: Vec<Registered> = Vec::new();
    for entry in registry::living(dir) {
        if entry.entrypoint.as_deref() != Some(HEADLESS)
            && !live.iter().any(|seen| seen.session_id == entry.session_id)
        {
            live.push(entry);
        }
    }
    followed.retain(|id, follow| {
        let gone = !live.iter().any(|entry| &entry.session_id == id);
        if gone {
            // The turn it ran is over with its process: by `claude stop`, or its end.
            says.read(id, follow.transcript.as_deref());
            let began = follow.began.take();
            follow.settle(says, id, began, millis_now());
            say(
                notifier,
                id,
                LiveState {
                    settled: follow.settled.clone(),
                    ..LiveState::default()
                },
            );
            says.forget(id);
        }
        !gone
    });
    for entry in live {
        let follow = followed.entry(entry.session_id.clone()).or_default();
        let state = follow.state(&entry, store, says);
        if follow.said.as_ref() != Some(&state) {
            say(notifier, &entry.session_id, state.clone());
            follow.said = Some(state);
        }
    }
}

impl Followed {
    /// Where the session stands now: its registry entry's status, what its transcript's
    /// end says, and which of Steery's says the turn running took.
    fn state(&mut self, entry: &Registered, store: &Store, says: &Says) -> LiveState {
        let id = &entry.session_id;
        if self.transcript.is_none()
            && self.missed.is_none_or(|missed| missed.elapsed() >= LOOK_AGAIN)
        {
            self.transcript = store.transcript(id).ok();
            self.missed = self.transcript.is_none().then(Instant::now);
        }
        says.read(id, self.transcript.as_deref());
        let now = self
            .transcript
            .as_deref()
            .map(|transcript| store.now(transcript))
            .unwrap_or_default();
        let since = entry.status_updated_at.unwrap_or_else(millis_now);
        let running = matches!(entry.status.as_deref(), Some("busy" | "waiting"));
        let was = self.began;
        let began = match was {
            Some(began) if now.ended.is_none_or(|ended| ended < began) => began,
            Some(_) => since.max(now.ended.unwrap_or_default()),
            None => since,
        };
        self.began = running.then_some(began);
        // The turn seen running is over: none runs now, or the transcript ended it and
        // another began.
        if let Some(over) = was.filter(|over| self.began != Some(*over)) {
            let at = now
                .ended
                .filter(|ended| *ended >= over)
                .unwrap_or(self.began.unwrap_or(since));
            self.settle(says, id, Some(over), at);
        }
        // A turn no look saw run: words went in and the transcript ended a turn after
        // them, and no turn running began before that end.
        if let Some(ended) = now.ended
            && self.began.is_none_or(|began| began > ended)
        {
            self.settle(says, id, None, ended);
        }
        // A wait to try a request again is the turn's: one a turn left unanswered as it
        // ended, or whose time is past, is waited on no more.
        let now_at = millis_now();
        let retry = now
            .retry
            .filter(|retry| running && retry.until > now_at)
            .map(|retry| retry.retry);
        LiveState {
            turn: self.began.map(|began_at| LiveTurn {
                began_at,
                says: says.took(id).into_iter().map(|(say, _)| say).collect(),
            }),
            idle: match entry.status.as_deref() {
                Some("idle") => Some(true),
                Some(_) => Some(false),
                None => None,
            },
            retrying: retry.is_some(),
            retry,
            jobs: now.jobs,
            model: now.model,
            thinking: now.thinking,
            context_left: now.context_left,
            dialog: (entry.status.as_deref() == Some("waiting"))
                .then_some(entry.waiting_for.as_deref())
                .flatten()
                .map(|waiting| dialog(waiting, now.open.as_deref(), since)),
            settled: self.settled.clone(),
            ..LiveState::default()
        }
    }

    /// A turn over at `at`, begun at `began` where a look saw it run: the says whose words
    /// went in by then are the ones it took, and where it took any it is settled, failed
    /// where Claude Code's own error is the last answer written since it began.
    fn settle(&mut self, says: &Says, id: &str, began: Option<u64>, at: u64) {
        let took = says.settle(id, at);
        let Some(first) = took.iter().map(|(_, went)| *went).min() else {
            return;
        };
        let began_at = began.map_or(first, |began| began.min(first));
        let error = self
            .transcript
            .as_deref()
            .and_then(|transcript| crate::turn::stopped_on(transcript, began_at));
        self.settled.push(LiveSettled {
            began_at,
            at,
            says: took.into_iter().map(|(say, _)| say).collect(),
            failed: error.is_some(),
            error,
        });
        let past = self.settled.len().saturating_sub(SETTLED_KEPT);
        self.settled.drain(..past);
    }
}

/// What a waiting session shows its person, as `waitingFor` names it. A permission prompt
/// asks about a call that has no result yet: the newest such call of the main thread
/// (`open`) names it, unless it is a sub-agent's run, whose own call is the one asked
/// about.
fn dialog(waiting: &str, open: Option<&str>, since: u64) -> Dialog {
    let kind = match waiting {
        "permission prompt" | "sandbox request" | "worker request" => DialogKind::Confirm,
        "input needed" => DialogKind::Input,
        _ => DialogKind::Custom,
    };
    let asked =
        open.filter(|call| waiting == "permission prompt" && !crate::events::AGENTS.contains(call));
    if let Some(call) = asked {
        return Dialog {
            kind,
            title: format!("Allow {call}?"),
            since,
        };
    }
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
pub(crate) fn millis_now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_permission_prompt_names_the_call_it_asks_about() {
        let asked = dialog("permission prompt", Some("Bash: rm -rf build"), 7);
        assert_eq!(asked.kind, DialogKind::Confirm);
        assert_eq!(asked.title, "Allow Bash: rm -rf build?");
        // A sub-agent's call asks inside its run; its lead's open call is not it.
        assert_eq!(
            dialog("permission prompt", Some("Agent"), 7).title,
            "Permission prompt"
        );
        assert_eq!(
            dialog("input needed", Some("Bash: ls"), 7).title,
            "Input needed"
        );
        assert_eq!(
            dialog("permission prompt", None, 7).title,
            "Permission prompt"
        );
    }
}
