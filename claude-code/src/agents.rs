//! Claude Code's background sessions, as its own commands say them.
//!
//! `claude --bg -- <words>` starts a session in Claude Code's own background service and
//! prints its short id; `--resume <id>` wakes a conversation nobody is in under the same
//! id, `--fork-session` starts a second one from it, and `-w` puts it in a new worktree.
//! The session is the one a person gets typing `claude` — an interactive session,
//! `entrypoint: cli` — so `claude agents`, `claude --resume` and the Claude app list it
//! and `claude attach` opens it. It stays in the service once its turn is done, and takes
//! the next words through its inbox. `claude agents --json` says where each background
//! session stands: `state` `working`, then where its turn stopped ([`end`]). `claude stop
//! <job>` ends one and keeps its conversation; `claude rm <job>` removes its row.

use std::path::Path;
use std::process::Output;
use std::time::Duration;

use serde_json::Value;
use steery_sdk::refusal;
use steery_sdk::wire::harness::{Ended, TurnState};

/// How long `claude agents`, `claude stop` or `claude rm` has to answer: listing its
/// sessions starts the whole harness, about a second.
pub const ASKED_WITHIN: Duration = Duration::from_secs(15);

/// The most of a command's output read.
const MOST: usize = 1024 * 1024;

/// What Claude Code says when a conversation already runs in a session of its own: a
/// `--resume` beside it is refused, and one with `--bg` starts a copy as another session,
/// which is a fork nobody asked for. This driver says the same words when it refuses
/// first, so both read as `harness-busy` ([`refusal_of`]).
pub const RUNNING: [&str; 2] = [
    "is running as a background session",
    "is already running in the background",
];

/// `claude <args>`, which writes no conversation, within `within`: its output, at most
/// [`MOST`] of each stream.
pub async fn said(binary: &Path, args: &[&str], within: Duration) -> Result<Output, String> {
    let mut command = steery_driver_common::program::helper(binary);
    command
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = command
        .spawn()
        .map_err(|e| format!("starting {}: {e}", binary.display()))?;
    let mut out = tokio::time::timeout(within, child.wait_with_output())
        .await
        .map_err(|_| {
            format!(
                "`claude {}` gave no answer within {} s",
                args.join(" "),
                within.as_secs()
            )
        })?
        .map_err(|e| format!("`claude {}`: {e}", args.join(" ")))?;
    out.stdout.truncate(MOST);
    out.stderr.truncate(MOST);
    Ok(out)
}

/// Every background session `claude agents --json --all` lists now. Without `--all`,
/// Claude Code 2.1.295 drops a session whose process has exited, so a turn that failed
/// at once (`state: failed`, no `pid`) would never be seen to end.
pub async fn agents(binary: &Path) -> Result<Vec<Value>, String> {
    let out = said(binary, &["agents", "--json", "--all"], ASKED_WITHIN).await?;
    if !out.status.success() {
        return Err(format!(
            "`claude agents` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("`claude agents` said no JSON: {e}"))
}

/// A background session as `claude agents --json` lists it names its conversation.
pub fn named(line: &Value) -> Option<String> {
    (line.get("kind").and_then(Value::as_str) == Some("background"))
        .then(|| line.get("sessionId").and_then(Value::as_str))
        .flatten()
        .map(str::to_owned)
}

/// Where a background session's turn ended, as `claude agents --json` lists it (Claude
/// Code 2.1.281, `printAgentsJson`): `done`; `failed`; or `blocked` with its process
/// `idle` or gone, where the turn stopped for its person — on a question, or on an error
/// of Claude Code's own, which the turn writes beside the listing as `error`. `blocked`
/// with its process `waiting` is held on a dialog mid-turn, and the turn goes on once its
/// person answers. `stopped` says no end: the turn was interrupted.
pub fn end(line: &Value) -> Option<Ended> {
    let said = || line.get("error").and_then(Value::as_str).map(str::to_owned);
    let ended = |state, error| Some(Ended { state, error });
    match line.get("state").and_then(Value::as_str)? {
        "done" => ended(TurnState::Finished, None),
        "failed" => ended(TurnState::Failed, said()),
        "blocked" if line.get("status").and_then(Value::as_str) != Some("waiting") => {
            match said() {
                Some(error) => ended(TurnState::Failed, Some(error)),
                None => ended(TurnState::Finished, None),
            }
        }
        _ => None,
    }
}

/// What a turn's follower has seen of its background session so far.
#[derive(Default)]
pub struct Follow {
    /// It was listed `working`: this turn ran.
    working: bool,
}

impl Follow {
    /// Whether this listing settles the turn: the session stopped — its turn ended
    /// ([`end`]), or it was `stopped` — and the stop is this turn's. A conversation
    /// continued again lists what its last run left until the new turn goes, so a stop
    /// counts once the session was seen working, or once `ended` finds this turn's end
    /// recorded.
    pub fn settled_by(&mut self, line: &Value, ended: impl FnOnce() -> bool) -> bool {
        let state = line.get("state").and_then(Value::as_str);
        let over = end(line).is_some() || state == Some("stopped");
        let settled = over && (self.working || ended());
        self.working |= state == Some("working");
        settled
    }
}

/// The short id `claude --bg` prints on its first line, `backgrounded · <id>`, a name
/// after it where the session has one.
pub fn backgrounded(said: &str) -> Option<String> {
    said.lines()
        .find_map(|line| line.trim().strip_prefix("backgrounded"))
        .and_then(|rest| rest.split('·').map(str::trim).find(|part| !part.is_empty()))
        .map(str::to_owned)
}

/// The refusal word Claude Code's own words for what it would not start mean.
pub fn refusal_of(said: &str) -> &'static str {
    if said.contains("No conversation found") {
        refusal::NOT_FOUND
    } else if said.contains("not trusted") {
        refusal::UNTRUSTED
    } else if RUNNING.iter().any(|running| said.contains(running))
        || said.contains("already")
        || said.contains("in use")
    {
        refusal::HARNESS_BUSY
    } else {
        refusal::START_FAILED
    }
}

/// Whether a listing names a session in `session` that runs a turn: `working`, or
/// `blocked` on a dialog mid-turn.
pub fn active(listed: &[Value], session: &str) -> bool {
    listed.iter().any(|entry| {
        entry.get("sessionId").and_then(Value::as_str) == Some(session)
            && (entry.get("state").and_then(Value::as_str) == Some("working")
                || entry.get("state").and_then(Value::as_str) == Some("blocked")
                    && entry.get("status").and_then(Value::as_str) == Some("waiting"))
    })
}

/// What a listing and the registry say `session` is doing: `Some(true)` a turn runs,
/// `Some(false)` none does, `None` where they cannot say. `owner` is the live session the
/// registry names in it; `unheld`, that no process holds its transcript.
///
/// A conversation the listing names is what that listing says. One it does not, whose
/// transcript nobody holds, is a conversation nobody is in. One named twice says nothing.
pub fn execution(
    listed: &[Value],
    session: &str,
    owner: Option<&crate::registry::Strict>,
    unheld: bool,
) -> Option<bool> {
    let mut matches = listed
        .iter()
        .filter(|entry| entry.get("sessionId").and_then(Value::as_str) == Some(session));
    let first = matches.next();
    if matches.next().is_some() {
        return None;
    }
    let Some(first) = first else {
        // Listed nowhere: no agent is in it. Idle only where no process holds its
        // transcript either; else something outside the listing may be writing it.
        return (unheld && owner.is_none()).then_some(false);
    };
    let exact_owner = owner.is_some_and(|owner| {
        first.get("pid").and_then(Value::as_u64) == Some(u64::from(owner.pid))
            && owner
                .job
                .as_ref()
                .is_none_or(|job| first.get("id").and_then(Value::as_str) == Some(job))
    });
    // Listed, but with no process behind it: a background job whose process is gone
    // keeps its row. With no registered owner and nobody holding its transcript, nobody
    // is in it, whatever state it last said — save `working`, which a row with no process
    // cannot honestly say, and which stays uncertain.
    let processless =
        first.get("pid").and_then(Value::as_u64).is_none() && owner.is_none() && unheld;
    match first.get("state").and_then(Value::as_str) {
        Some("working") => Some(true),
        Some("blocked") if first.get("status").and_then(Value::as_str) == Some("waiting") => {
            Some(true)
        }
        Some("blocked" | "done" | "failed") if exact_owner || processless => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Strict;
    use serde_json::json;

    fn finished() -> Option<Ended> {
        Some(Ended {
            state: TurnState::Finished,
            error: None,
        })
    }

    fn failed(error: &str) -> Option<Ended> {
        Some(Ended {
            state: TurnState::Failed,
            error: Some(error.to_owned()),
        })
    }

    #[test]
    fn a_background_session_blocked_on_its_own_error_failed() {
        assert_eq!(
            end(&json!({"state": "blocked", "error": "Autocompact is thrashing"})),
            failed("Autocompact is thrashing")
        );
        assert_eq!(
            end(&json!({"state": "blocked", "status": "waiting"})),
            None,
            "held mid-turn on a dialog for its person"
        );
        assert_eq!(end(&json!({"state": "done"})), finished());
    }

    /// Claude Code 2.1.281 lists a job `blocked` when its turn stopped for its person —
    /// its recap said it needs a reply, or it hit an error of its own — and when its
    /// process is `waiting` on a dialog mid-turn (`printAgentsJson`). Only the second is a
    /// turn that goes on. The lines are what a throw's turn into `cb75f40d` printed,
    /// 2026-09-27: read as going, `blocked` kept its follower asking every second for
    /// 10.5 hours after the turn ended.
    #[test]
    fn a_turn_ends_where_its_session_stopped_for_its_person() {
        let at = |started: i64, rest: Value| {
            let mut line = json!({"id": "cb75f40d", "kind": "background",
                "sessionId": "cb75f40d-d197-4cb2-940f-d32cf3f52c01", "startedAt": started});
            line.as_object_mut()
                .unwrap()
                .extend(rest.as_object().unwrap().clone());
            line
        };
        let recorded = [
            at(
                1790541537638,
                json!({"pid": 40471, "state": "working", "status": "idle"}),
            ),
            at(
                1790541537638,
                json!({"pid": 40471, "state": "working", "status": "busy"}),
            ),
            at(
                1790541537638,
                json!({"pid": 40471, "state": "blocked", "status": "idle"}),
            ),
            at(
                1790541537638,
                json!({"pid": 40471, "state": "working", "status": "busy"}),
            ),
            at(
                1790541537638,
                json!({"pid": 40471, "state": "blocked", "status": "idle"}),
            ),
            at(1790541535822, json!({"state": "blocked"})),
        ];
        let mut follow = Follow::default();
        let settled = recorded
            .iter()
            .position(|line| follow.settled_by(line, || false));
        assert_eq!(settled, Some(2), "the turn ended where it first stopped");
        assert_eq!(end(&recorded[2]), finished());
        assert_eq!(
            end(&recorded[5]),
            finished(),
            "its process gone, the turn is long over"
        );
        assert_eq!(
            end(&json!({"state": "failed", "error": "Credit balance is too low"})),
            failed("Credit balance is too low")
        );
        assert_eq!(
            end(&json!({"state": "stopped"})),
            None,
            "stopped says no end"
        );
        assert_eq!(end(&json!({"state": "working", "status": "idle"})), None);

        // A conversation continued again lists what its last run left until the new
        // turn goes: that is not this turn's end, and the end its transcript records
        // after the launch is.
        let mut resumed = Follow::default();
        let stale = at(
            1790541600000,
            json!({"pid": 50000, "state": "blocked", "status": "idle"}),
        );
        assert!(
            !resumed.settled_by(&stale, || false),
            "an end from before the launch"
        );
        assert!(
            resumed.settled_by(&stale, || true),
            "the transcript ended this turn"
        );
        let mut stopped = Follow::default();
        let stop = json!({"state": "stopped"});
        assert!(
            !stopped.settled_by(&stop, || false),
            "a stop left from before"
        );
        assert!(!stopped.settled_by(&json!({"state": "working", "status": "busy"}), || false));
        assert!(
            stopped.settled_by(&stop, || false),
            "stopped after it worked: over, and read as interrupted"
        );
    }

    /// A background session names its conversation as `claude agents --json` lists it,
    /// and its turn ran to its end at `done`.
    #[test]
    fn a_background_session_names_the_conversation_and_ends_at_done() {
        let listed = json!({"pid": 74329, "id": "6920e989", "cwd": "/w", "kind": "background",
            "startedAt": 1790237098062_i64, "sessionId": "6920e989-aa9e-467c-9b64-569ba2ca6059",
            "name": "alpha", "status": "busy", "state": "working"});
        assert_eq!(
            named(&listed).as_deref(),
            Some("6920e989-aa9e-467c-9b64-569ba2ca6059")
        );
        assert_eq!(
            named(&json!({"kind": "interactive", "sessionId": "x"})),
            None,
            "a terminal somebody is in is not a turn of ours"
        );
        assert_eq!(end(&listed), None);
        assert_eq!(end(&json!({"state": "done", "status": "idle"})), finished());
        assert_eq!(
            backgrounded("backgrounded · 81827bab · prompt injection attempt\n  claude agents")
                .as_deref(),
            Some("81827bab")
        );
        assert_eq!(
            backgrounded("Workspace not trusted. Run `claude` in /w once"),
            None
        );
        assert_eq!(
            refusal_of(
                "Workspace not trusted. Run `claude` in /w once and accept the trust prompt"
            ),
            refusal::UNTRUSTED
        );
        assert_eq!(
            refusal_of("No conversation found with session ID"),
            refusal::NOT_FOUND
        );
    }

    /// A conversation a session is already in is never resumed beside it, whichever way
    /// Claude Code says so — refusing an interactive resume, or starting a copy under
    /// `--bg` — and whichever way this driver says it first: each is `harness-busy`.
    #[test]
    fn a_resume_beside_a_live_session_waits_on_the_same_conversation() {
        for said in [
            "Error: Session 6b44d2be-eebf-460a-93b0-a53b72ef4b83 is running as a background \
             session (6b44d2be). Run `claude attach 6b44d2be` to open it, or `claude stop \
             6b44d2be` first to resume it here. Add --fork-session to branch off a copy instead.",
            "note: session 153824d7 is already running in the background, so this started a \
             copy as 186b32ef. `claude attach 153824d7` opens the original.\nbackgrounded · 186b32ef",
            &format!("session 153824d7 {}", RUNNING[0]),
            &format!(
                "session 153824d7 {}: the copy was stopped and removed",
                RUNNING[1]
            ),
        ] {
            assert_eq!(refusal_of(said), refusal::HARNESS_BUSY, "{said}");
        }
    }

    #[test]
    fn execution_requires_explicit_claude_state() {
        let owner = Strict {
            pid: 123,
            start: 1,
            job: Some("job".into()),
            kind: Some("bg".into()),
        };
        let listed = |state: &str| json!({"sessionId":"s","pid":123,"id":"job","state":state});
        assert_eq!(execution(&[], "s", None, false), None);
        assert_eq!(execution(&[], "s", Some(&owner), false), None);
        assert_eq!(
            execution(&[], "s", None, true),
            Some(false),
            "listed nowhere and held by nobody: nobody is in it"
        );
        assert_eq!(
            execution(&[], "s", Some(&owner), true),
            None,
            "a registered owner outside the listing may still be writing it"
        );
        assert_eq!(
            execution(&[listed("working")], "s", Some(&owner), false),
            Some(true)
        );
        assert_eq!(
            execution(
                &[
                    json!({"sessionId":"s","pid":123,"id":"job","state":"blocked","status":"waiting"})
                ],
                "s",
                Some(&owner),
                false
            ),
            Some(true)
        );
        assert_eq!(
            execution(&[listed("done")], "s", Some(&owner), false),
            Some(false)
        );
        assert_eq!(execution(&[listed("done")], "s", None, false), None);
        // A background job whose process is gone keeps its row, blocked, with no pid:
        // on 2026-09-30 a Retry waited ten hours on one.
        let processless =
            |state: &str| json!({"sessionId":"s","id":"s","kind":"background","state":state});
        assert_eq!(
            execution(&[processless("blocked")], "s", None, true),
            Some(false),
            "no process, no owner, nobody holding it: nobody is in it"
        );
        assert_eq!(
            execution(&[processless("blocked")], "s", None, false),
            None,
            "something holds its transcript"
        );
        assert_eq!(
            execution(&[processless("working")], "s", None, true),
            Some(true),
            "a row that says working is taken at its word"
        );
        assert_eq!(
            execution(
                &[json!({"sessionId":"s","pid":124,"id":"old","state":"done"})],
                "s",
                Some(&owner),
                false
            ),
            None
        );
        assert_eq!(
            execution(&[listed("stopped")], "s", Some(&owner), false),
            None
        );
        assert_eq!(
            execution(
                &[listed("done"), listed("working")],
                "s",
                Some(&owner),
                false
            ),
            None
        );
    }

    #[test]
    fn blocked_waiting_agent_is_active() {
        assert!(active(
            &[json!({"sessionId":"s1","state":"blocked","status":"waiting"})],
            "s1"
        ));
        assert!(!active(
            &[json!({"sessionId":"s1","state":"blocked","status":"idle"})],
            "s1"
        ));
    }

    #[tokio::test]
    async fn agents_requires_successful_valid_json() {
        let dir = crate::registry::tests::Scratch::new("agents");
        let binary = dir.path().join("claude");
        let stand_in = |script: &str| crate::tests::stand_in(&binary, script);
        stand_in("#!/bin/sh\necho '[]'\n");
        assert!(agents(&binary).await.unwrap().is_empty());
        stand_in("#!/bin/sh\necho '{'\n");
        assert!(agents(&binary).await.is_err());
        stand_in("#!/bin/sh\nexit 2\n");
        assert!(agents(&binary).await.is_err());
    }
}
