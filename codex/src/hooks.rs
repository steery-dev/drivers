//! Codex's hooks: which of its moments Steery reads, the document Codex hands a hook, and
//! what a hook prints back.
//!
//! Codex reads `hooks.json` beside its `config.toml` in the shape Claude Code's settings
//! use, and hands each hook a JSON document on standard input: `session_id`, `cwd`,
//! `hook_event_name`, `model` (the moment's, kept on the Moment as state), and per moment
//! `prompt`, `tool_name`, `tool_input`, `tool_response`, `tool_use_id`,
//! `stop_hook_active`, `permission_suggestions`. A sub-agent's moments come under its
//! lead's `session_id` with its own `agent_id`.
//!
//! It reads a paragraph back for its agent on `SessionStart` and `UserPromptSubmit`
//! (`hookSpecificOutput.additionalContext`) and keeps a turn going at `Stop` only on
//! `decision: "block"` with the paragraph as `reason` (codex-rs `hooks/src/schema.rs`
//! `StopCommandOutputWire`). `PreToolUse` takes a `permissionDecision`, and
//! `PermissionRequest` a `decision` with a `behavior`, as Claude Code does.

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Always, Answer, Decision, Hook, HookAnswer, HookAnswered, HookKind, HookMoment, HookRead,
    Moment, Says, Suggestion, TurnState, Via,
};

use crate::calls;

/// How long Codex waits for a hook that writes one small file.
pub const QUICK: u32 = 5;
/// The most Codex waits on a `SessionEnd` or `Interrupt` hook.
pub const ENDING: u32 = 3;
/// How long Codex waits on a moment a person may answer from Steery: the runtime answers
/// within it, by enough that what Codex reads is always the runtime's answer.
pub const ASKING: u32 = 2100;

/// Every moment of Codex's Steery reads, its kind, and what it can say there.
const EVENTS: [(&str, HookKind, &[Says]); 8] = [
    (
        "SessionStart",
        HookKind::SessionStart,
        &[Says::Context, Says::Notice],
    ),
    (
        "UserPromptSubmit",
        HookKind::Prompt,
        &[Says::Context, Says::Notice],
    ),
    ("PreToolUse", HookKind::BeforeCall, &[]),
    ("PostToolUse", HookKind::AfterCall, &[]),
    ("PermissionRequest", HookKind::Permission, &[]),
    // Codex keeps a turn going at its end with a paragraph.
    ("Stop", HookKind::TurnEnd, &[Says::Context]),
    // Fires in place of `Stop` when the person interrupts the turn.
    ("Interrupt", HookKind::TurnEnd, &[]),
    ("SessionEnd", HookKind::SessionEnd, &[]),
];

/// The seconds Codex gives a hook on `event`, as `setup/plan` writes it.
pub fn timeout(event: &str) -> u32 {
    match event {
        "SessionEnd" | "Interrupt" => ENDING,
        "PreToolUse" | "PermissionRequest" => ASKING,
        _ => QUICK,
    }
}

/// The hooks as `initialize` declares them.
pub fn declared() -> Vec<Hook> {
    EVENTS
        .iter()
        .map(|(event, kind, says)| Hook {
            event: (*event).into(),
            kind: *kind,
            via: Via::Command,
            says: says.to_vec(),
            waits: (timeout(event) == ASKING).then_some(ASKING),
            env: Vec::new(),
        })
        .collect()
}

/// Every event setup writes a hook on.
pub fn events() -> impl Iterator<Item = &'static str> {
    EVENTS.iter().map(|(event, _, _)| *event)
}

fn kind_of(event: &str) -> Option<HookKind> {
    EVENTS
        .iter()
        .find(|(name, _, _)| *name == event)
        .map(|(_, kind, _)| *kind)
}

fn text<'a>(document: &'a Value, key: &str) -> Option<&'a str> {
    document
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// `hook/read`: the moment a document is, or none where it is no moment Steery carries.
pub fn read(read: HookRead) -> Result<HookMoment, Error> {
    let Some(kind) = kind_of(&read.event) else {
        return Ok(HookMoment { moment: None });
    };
    let Ok(document) = serde_json::from_str::<Value>(&read.document) else {
        return Ok(HookMoment { moment: None });
    };
    let Some(external_id) = text(&document, "session_id") else {
        return Ok(HookMoment { moment: None });
    };
    let absolute = |key: &str| {
        text(&document, key)
            .filter(|path| path.starts_with('/') && !path.chars().any(char::is_control))
            .map(std::path::PathBuf::from)
    };
    let call = matches!(
        kind,
        HookKind::BeforeCall | HookKind::AfterCall | HookKind::Permission
    )
    .then(|| {
        let tool = text(&document, "tool_name")?;
        Some(calls::call(
            text(&document, "tool_use_id").unwrap_or_default(),
            tool,
            document.get("tool_input").cloned().unwrap_or(Value::Null),
        ))
    })
    .flatten();
    let moment = Moment {
        event: read.event.clone(),
        external_id: external_id.to_owned(),
        sub_agent: text(&document, "agent_id")
            .filter(|agent| *agent != external_id)
            .map(str::to_owned),
        at: None,
        cwd: absolute("cwd"),
        // Codex keeps every rollout under its own home, which `sessions/list` reads.
        transcript: None,
        call,
        ok: (kind == HookKind::AfterCall).then(|| succeeded(document.get("tool_response"))),
        outcome: match read.event.as_str() {
            "Stop" => Some(TurnState::Finished),
            "Interrupt" => Some(TurnState::Interrupted),
            _ => None,
        },
        continued: kind == HookKind::TurnEnd
            && document.get("stop_hook_active").and_then(Value::as_bool) == Some(true),
        // Codex says `other` for every way a session ends: nothing is its person's word.
        person_exited: false,
        prompt: (kind == HookKind::Prompt)
            .then(|| document.get("prompt").and_then(Value::as_str))
            .flatten()
            .map(str::to_owned),
        context_left: None,
        // Codex names the model each moment runs on, and no thinking level.
        model: text(&document, "model").map(str::to_owned),
        thinking: None,
        failure: None,
        suggestions: (kind == HookKind::Permission)
            .then(|| {
                document
                    .get("permission_suggestions")
                    .and_then(Value::as_array)
                    .map(|offered| offered.iter().map(suggestion).collect())
            })
            .flatten()
            .unwrap_or_default(),
    };
    Ok(HookMoment {
        moment: Some(moment),
    })
}

/// One of Codex's `permission_suggestions` as a person reads it, kept at its index so
/// `always: {suggestion: n}` passes back the entry Codex wrote. Every shape it declares is
/// named; one it grows later is offered by its type, never dropped.
fn suggestion(update: &Value) -> Suggestion {
    let about = match text(update, "destination") {
        Some("session") => "this conversation",
        Some("projectSettings") => "this project, for everyone",
        Some("localSettings") => "this project, for you",
        Some("userSettings") => "everywhere, from now on",
        _ => "as Codex keeps it",
    };
    let kind = text(update, "type").unwrap_or("unnamed");
    let label = match kind {
        "addRules" | "replaceRules" => {
            let rules: Vec<String> = update
                .get("rules")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .map(
                    |rule| match (text(rule, "toolName"), text(rule, "ruleContent")) {
                        (Some(tool), Some(content)) => format!("`{tool}({content})`"),
                        (Some(tool), None) => format!("`{tool}`"),
                        _ => "an unnamed rule".to_owned(),
                    },
                )
                .collect();
            match rules.is_empty() {
                true => "Always allow what Codex offered".to_owned(),
                false => format!("Always {}", rules.join(", ")),
            }
        }
        "setMode" => format!("Switch to `{}`", text(update, "mode").unwrap_or("—")),
        "addDirectories" => "Add the directories it named".to_owned(),
        other => format!("`{other}`, as Codex offered it"),
    };
    Suggestion {
        label,
        about: about.to_owned(),
    }
}

/// Whether a call's response says it worked: an object's own `success`, `is_error` or
/// `exit_code`; anything else is taken as done.
fn succeeded(response: Option<&Value>) -> bool {
    let Some(response) = response else {
        return true;
    };
    if let Some(success) = response.get("success").and_then(Value::as_bool) {
        return success;
    }
    if response.get("is_error").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    response
        .get("exit_code")
        .and_then(Value::as_i64)
        .is_none_or(|code| code == 0)
}

/// `hook/answer`: what the hook prints for what the runtime decided. Codex reads a
/// non-zero exit as a hook that failed, so the exit is always 0.
pub fn answer(answer: HookAnswer) -> Result<HookAnswered, Error> {
    let document: Value = serde_json::from_str(&answer.document).unwrap_or(Value::Null);
    let printed = printed(&answer.event, &document, &answer.answer);
    Ok(HookAnswered {
        stdout: printed.map(|value| value.to_string()).unwrap_or_default(),
        exit_code: 0,
        append: None,
    })
}

fn printed(event: &str, document: &Value, answer: &Answer) -> Option<Value> {
    let say = answer.say.as_deref().filter(|say| !say.trim().is_empty());
    match (event, &answer.decision) {
        ("Stop", _) => say.map(|say| json!({"decision": "block", "reason": say})),
        // Codex runs a call no hook denied, and reads `permissionDecision: allow` as
        // unsupported (it is reserved for a hook that rewrites the call's input), so an
        // allow prints nothing.
        ("PreToolUse", Some(Decision::Allow { .. })) => None,
        ("PreToolUse", Some(Decision::Deny { reason })) => {
            let mut output = json!({
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            });
            if let Some(say) = say {
                output["additionalContext"] = json!(say);
            }
            Some(json!({"hookSpecificOutput": output}))
        }
        ("PermissionRequest", Some(decision)) => {
            let decision = match decision {
                Decision::Allow { always, .. } => {
                    let mut allowed = json!({"behavior": "allow"});
                    if let Some(update) = always.and_then(|always| standing(always, document)) {
                        allowed["updatedPermissions"] = json!([update]);
                    }
                    allowed
                }
                Decision::Deny { reason } => json!({"behavior": "deny", "message": reason}),
            };
            Some(json!({"hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": decision,
            }}))
        }
        ("SessionStart" | "UserPromptSubmit", _) => say.map(
            |say| json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": say}}),
        ),
        _ => None,
    }
}

/// The permission a yes stands for from now on: one of the harness's own suggestions,
/// passed back whole; every call of this tool; or every call at all, both for this
/// session only.
fn standing(always: Always, document: &Value) -> Option<Value> {
    match always {
        Always::Suggestion(n) => document
            .get("permission_suggestions")
            .and_then(Value::as_array)
            .and_then(|suggestions| suggestions.get(n as usize))
            .cloned(),
        Always::Tool => text(document, "tool_name").map(|tool| {
            json!({"type": "addRules", "rules": [{"toolName": tool}], "behavior": "allow",
                   "destination": "session"})
        }),
        Always::Everything => {
            Some(json!({"type": "setMode", "mode": "bypassPermissions", "destination": "session"}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn moment(event: &str, document: Value) -> Option<Moment> {
        read(HookRead {
            event: event.into(),
            document: document.to_string(),
            env: BTreeMap::new(),
        })
        .unwrap()
        .moment
    }

    fn said(event: &str, document: Value, answer: Answer) -> String {
        super::answer(HookAnswer {
            event: event.into(),
            document: document.to_string(),
            env: BTreeMap::new(),
            answer,
        })
        .unwrap()
        .stdout
    }

    /// Codex's document as Codex 0.155.1 wrote it on 2026-09-28: the call it is at, its
    /// tool named as the transcript names it, and its id.
    #[test]
    fn a_codex_document_reads_as_its_moment() {
        let codex = json!({"session_id": "019d", "transcript_path": "/tmp/r.jsonl", "cwd": "/tmp/ra/codexwork",
            "hook_event_name": "PostToolUse", "model": "gpt-5.5", "tool_name": "multi_agent_v1wait_agent",
            "tool_input": {"targets": ["a"]}, "tool_response": {"status": {}}, "tool_use_id": "call_1"});
        let read = moment("PostToolUse", codex.clone()).unwrap();
        assert_eq!(read.external_id, "019d");
        assert_eq!(read.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(
            read.cwd.as_deref(),
            Some(std::path::Path::new("/tmp/ra/codexwork"))
        );
        let call = read.call.unwrap();
        assert_eq!(
            (call.id.as_str(), call.tool.as_str()),
            ("call_1", "wait_agent")
        );
        assert_eq!(read.ok, Some(true));
        assert!(moment("Unhooked", codex.clone()).is_none());
        assert!(
            moment("PostToolUse", json!({"cwd": "/w"})).is_none(),
            "names no conversation"
        );
        // A sub-agent's moment comes under its lead with its own id.
        let mut led = codex;
        led["agent_id"] = json!("child");
        assert_eq!(
            moment("PostToolUse", led).unwrap().sub_agent.as_deref(),
            Some("child")
        );
    }

    /// A turn's end says how it ended and whether a hook already kept it going.
    #[test]
    fn a_turn_s_end_says_how_and_whether_it_was_kept_going() {
        let stop = moment("Stop", json!({"session_id": "s", "stop_hook_active": true})).unwrap();
        assert_eq!(stop.outcome, Some(TurnState::Finished));
        assert!(stop.continued);
        let interrupt = moment("Interrupt", json!({"session_id": "s"})).unwrap();
        assert_eq!(interrupt.outcome, Some(TurnState::Interrupted));
        let ended = moment("SessionEnd", json!({"session_id": "s", "reason": "other"})).unwrap();
        assert!(!ended.person_exited, "Codex always says `other`");
        let prompt = moment(
            "UserPromptSubmit",
            json!({"session_id": "s", "prompt": "go"}),
        )
        .unwrap();
        assert_eq!(prompt.prompt.as_deref(), Some("go"));
    }

    /// Codex's `Stop` keeps a turn going only on `decision: "block"`; a prompt and a
    /// start take the paragraph as context; a moment Codex reads nothing back from
    /// prints nothing.
    #[test]
    fn a_paragraph_is_printed_in_the_document_codex_parses() {
        let say = Answer {
            say: Some("Steery: describe this".into()),
            ..Answer::default()
        };
        let doc = json!({"session_id": "s"});
        let stop: Value = serde_json::from_str(&said("Stop", doc.clone(), say.clone())).unwrap();
        assert_eq!(
            stop,
            json!({"decision": "block", "reason": "Steery: describe this"})
        );
        let start: Value =
            serde_json::from_str(&said("SessionStart", doc.clone(), say.clone())).unwrap();
        assert_eq!(
            start["hookSpecificOutput"]["additionalContext"],
            "Steery: describe this"
        );
        assert_eq!(said("PostToolUse", doc.clone(), say), "");
        assert_eq!(
            said("Stop", doc, Answer::default()),
            "",
            "an empty answer says nothing"
        );
    }

    /// A person's yes can stand for more: the harness's own suggestion passed back whole,
    /// or the two wide ones for this session; a no carries its reason; a question
    /// answered in Steery denies the tool with the answer.
    #[test]
    fn decisions_are_printed_as_codex_reads_them() {
        let suggestion = json!({"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "cargo check:*"}],
            "behavior": "allow", "destination": "localSettings"});
        let doc =
            json!({"session_id": "s", "tool_name": "Bash", "permission_suggestions": [suggestion]});
        let allow = |always| Answer {
            decision: Some(Decision::Allow {
                reason: "yes".into(),
                always,
            }),
            ..Answer::default()
        };
        let printed: Value = serde_json::from_str(&said(
            "PermissionRequest",
            doc.clone(),
            allow(Some(Always::Suggestion(0))),
        ))
        .unwrap();
        assert_eq!(
            printed["hookSpecificOutput"]["decision"]["updatedPermissions"][0],
            suggestion
        );
        let printed: Value = serde_json::from_str(&said(
            "PermissionRequest",
            doc.clone(),
            allow(Some(Always::Tool)),
        ))
        .unwrap();
        assert_eq!(
            printed["hookSpecificOutput"]["decision"]["updatedPermissions"][0]["rules"][0]["toolName"],
            "Bash"
        );
        let printed: Value = serde_json::from_str(&said(
            "PermissionRequest",
            doc.clone(),
            Answer {
                decision: Some(Decision::Deny {
                    reason: "that would wipe the cache".into(),
                }),
                ..Answer::default()
            },
        ))
        .unwrap();
        assert_eq!(
            printed["hookSpecificOutput"]["decision"],
            json!({"behavior": "deny", "message": "that would wipe the cache"})
        );
        let printed: Value = serde_json::from_str(&said(
            "PreToolUse",
            doc.clone(),
            Answer {
                decision: Some(Decision::Deny {
                    reason: "They chose One".into(),
                }),
                ..Answer::default()
            },
        ))
        .unwrap();
        assert_eq!(printed["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            printed["hookSpecificOutput"]["permissionDecisionReason"],
            "They chose One"
        );
        assert_eq!(
            said("PreToolUse", doc, allow(None)),
            "",
            "Codex reads an allow before a call as unsupported: nothing is printed"
        );
    }

    /// Each of Codex's `permission_suggestions` is offered as a person reads it, at its
    /// own index, a shape not yet known by its type rather than dropped.
    #[test]
    fn a_permissions_suggestions_read_as_a_person_reads_them() {
        let doc = json!({"session_id": "s", "tool_name": "Bash", "tool_use_id": "c1",
        "permission_suggestions": [
            {"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "cargo check:*"}],
             "behavior": "allow", "destination": "localSettings"},
            {"type": "somethingNew", "destination": "session"},
        ]});
        let read = moment("PermissionRequest", doc).expect("a moment");
        assert_eq!(
            read.suggestions,
            vec![
                Suggestion {
                    label: "Always `Bash(cargo check:*)`".into(),
                    about: "this project, for you".into(),
                },
                Suggestion {
                    label: "`somethingNew`, as Codex offered it".into(),
                    about: "this conversation".into(),
                },
            ]
        );
    }

    /// Codex stops listening to a `SessionEnd` or `Interrupt` hook after three seconds,
    /// and a moment a person may answer waits as long as the runtime needs.
    #[test]
    fn every_hook_is_given_the_deadline_codex_honours() {
        assert_eq!(timeout("SessionEnd"), 3);
        assert_eq!(timeout("Interrupt"), 3);
        assert_eq!(timeout("Stop"), 5);
        let waits: Vec<(String, Option<u32>)> = declared()
            .into_iter()
            .map(|hook| (hook.event, hook.waits))
            .filter(|(_, w)| w.is_some())
            .collect();
        assert_eq!(
            waits,
            [
                ("PreToolUse".into(), Some(ASKING)),
                ("PermissionRequest".into(), Some(ASKING))
            ]
        );
    }
}
