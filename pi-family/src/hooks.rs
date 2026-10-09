//! `hook/read`: a moment Steery's plugin sent over the door, as the protocol's Moment.
//!
//! The plugin classifies nothing: it sends the harness's own hook payload, with what only
//! it knows beside it. Every document carries `session_id` (the conversation; for an omp
//! sub-agent filed in its lead's transcript, the lead's), `agent_id` (that sub-agent's own
//! id), `cwd`, `context_left`, `transcript_path` (never a sub-agent's) and `at`; a
//! moment's lead is the runtime's to read, from the door's hello. `UserPromptSubmit` adds
//! `prompt`; `PreToolUse` `tool_name`, `tool_use_id` and `tool_input`; `PostToolUse`
//! `tool_name`, `tool_use_id` and `is_error`; `Stop` the last answer's `stop_reason` and
//! `error_message`, and `continued` where a settle's answer already kept the turn going;
//! `SessionEnd` `mode`, `reason` and, in pi, `signal`.

use std::path::{Path, PathBuf};

use serde_json::Value;
use steery_sdk::Error;
use steery_sdk::wire::harness::{HookMoment, HookRead, Moment, TurnState};

use crate::Harness;
use crate::events::text;
use crate::transcripts::{Store, id_of};

/// `hook/read`.
pub fn read(store: &Store, read: HookRead) -> Result<HookMoment, Error> {
    let moment = serde_json::from_str::<Value>(&read.document)
        .ok()
        .and_then(|document| moment(store, &read.event, &document));
    Ok(HookMoment { moment })
}

fn moment(store: &Store, event: &str, document: &Value) -> Option<Moment> {
    let harness = store.harness();
    if !matches!(
        event,
        "SessionStart" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "Stop" | "SessionEnd"
    ) {
        return None;
    }
    let said = |field: &str| {
        text(document, field)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let external_id = said("session_id")?.to_owned();
    let call = match event {
        "PreToolUse" | "PostToolUse" => {
            let (id, tool) = (said("tool_use_id")?, said("tool_name")?);
            let input = document.get("tool_input").cloned().unwrap_or(Value::Null);
            Some(crate::calls::call(
                id,
                tool,
                input,
                None,
                &crate::calls::servers(store.root()),
            ))
        }
        _ => None,
    };
    Some(Moment {
        event: event.to_owned(),
        sub_agent: said("agent_id").map(str::to_owned),
        at: document.get("at").and_then(Value::as_u64),
        cwd: said("cwd")
            .map(PathBuf::from)
            .filter(|cwd| cwd.is_absolute()),
        transcript: said("transcript_path")
            .map(PathBuf::from)
            .filter(|file| names(file, &external_id)),
        call,
        ok: (event == "PostToolUse")
            .then(|| document.get("is_error").and_then(Value::as_bool))
            .flatten()
            .map(|failed| !failed),
        outcome: (event == "Stop")
            .then(|| said("stop_reason"))
            .flatten()
            .map(|stop| match stop {
                "error" => TurnState::Failed,
                "aborted" => TurnState::Interrupted,
                _ => TurnState::Finished,
            }),
        continued: event == "Stop" && document.get("continued") == Some(&Value::Bool(true)),
        person_exited: event == "SessionEnd" && person_exited(harness, document),
        prompt: (event == "UserPromptSubmit")
            .then(|| text(document, "prompt").map(str::to_owned))
            .flatten(),
        context_left: document
            .get("context_left")
            .and_then(Value::as_f64)
            .filter(|left| (0.0..=1.0).contains(left)),
        suggestions: Vec::new(),
        external_id,
    })
}

/// Whether a transcript a moment named is that conversation's own: an absolute `.jsonl`
/// filed under its id. Any other is no place to look for it.
fn names(file: &Path, external_id: &str) -> bool {
    file.is_absolute()
        && file.extension().is_some_and(|ext| ext == "jsonl")
        && id_of(file) == Some(external_id)
}

/// Whether the person left at the prompt, by the harness's own word, in a terminal. omp
/// says `dispose` for `/exit`, `/quit`, Ctrl+C and Ctrl+D and a signal's name for a signal,
/// and its `/new` leaves the conversation it was in (`new`). pi 1.0 says `quit` for
/// `/quit`, Ctrl+C and Ctrl+D and for `SIGTERM` and `SIGHUP` alike, so the plugin says
/// `signal` beside it; `new` leaves the conversation; reload, resume and fork are no exit.
fn person_exited(harness: Harness, document: &Value) -> bool {
    if text(document, "mode") != Some("tui") {
        return false;
    }
    match (harness, text(document, "reason")) {
        (Harness::Omp, Some("dispose" | "new")) | (Harness::Pi, Some("new")) => true,
        (Harness::Pi, Some("quit")) => document.get("signal") == Some(&Value::Null),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_one(harness: Harness, event: &str, document: Value) -> Option<Moment> {
        let home = Path::new("/nonexistent-home");
        let store = Store::new(harness, home, home.join(".pi/agent"), None);
        read(
            &store,
            HookRead {
                event: event.into(),
                document: document.to_string(),
                env: Default::default(),
            },
        )
        .expect("hook/read answers")
        .moment
    }

    /// A moment's document may say how much of the context window is left: kept while
    /// it is a fraction, and nothing when it is not one.
    #[test]
    fn a_pi_moment_keeps_the_room_its_document_says_is_left() {
        let left = |value: Value| {
            read_one(
                Harness::Pi,
                "Stop",
                json!({"session_id": "s1", "cwd": "/tmp", "context_left": value}),
            )
            .expect("a moment")
            .context_left
        };
        assert_eq!(left(json!(0.4)), Some(0.4));
        assert_eq!(left(json!(1.5)), None, "not a fraction");
        assert_eq!(left(json!("most")), None);
    }

    /// The transcript a moment names is a harness's word: only an absolute `.jsonl` filed
    /// under that conversation's own id is taken, never a path that names another file.
    #[test]
    fn an_identifier_from_a_harness_cannot_name_a_file_elsewhere() {
        let id = "01a0a013-c9d7-7000-8000-000000000001";
        let named = |path: &str| {
            read_one(
                Harness::Omp,
                "SessionStart",
                json!({"session_id": id, "transcript_path": path}),
            )
            .expect("a moment")
            .transcript
        };
        let own = format!("/home/a/.omp/agent/sessions/--w--/2026-10-09T10-00-00-000Z_{id}.jsonl");
        assert_eq!(named(&own), Some(PathBuf::from(&own)));
        for refused in [
            format!("relative/2026-10-09T10-00-00-000Z_{id}.jsonl"),
            format!("/home/a/2026-10-09T10-00-00-000Z_{id}.json"),
            "/home/a/2026-10-09T10-00-00-000Z_another.jsonl".to_owned(),
            "/home/a/.ssh/id_ed25519".to_owned(),
        ] {
            assert_eq!(named(&refused), None, "{refused} was taken");
        }
    }

    /// The runtime's rule for a person leaving: omp in its terminal on `dispose` or `new`,
    /// pi on `new`, or on `quit` that no signal caused; never in a one-shot run.
    #[test]
    fn a_person_exits_by_the_harnesses_own_word() {
        let ended = |harness, document: Value| person_exited(harness, &document);
        for reason in ["dispose", "new"] {
            assert!(ended(
                Harness::Omp,
                json!({"mode": "tui", "reason": reason})
            ));
        }
        for (mode, reason) in [("tui", "sigterm"), ("tui", "sighup"), ("print", "dispose")] {
            assert!(!ended(
                Harness::Omp,
                json!({"mode": mode, "reason": reason})
            ));
        }
        assert!(ended(Harness::Pi, json!({"mode": "tui", "reason": "new"})));
        assert!(ended(
            Harness::Pi,
            json!({"mode": "tui", "reason": "quit", "signal": null})
        ));
        assert!(
            !ended(
                Harness::Pi,
                json!({"mode": "tui", "reason": "quit", "signal": "SIGTERM"})
            ),
            "a signal says quit too"
        );
        assert!(!ended(
            Harness::Pi,
            json!({"mode": "tui", "reason": "quit"})
        ));
        assert!(!ended(
            Harness::Pi,
            json!({"mode": "tui", "reason": "reload"})
        ));
    }
}
