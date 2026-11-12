//! Droid's hooks: which of its moments Steery reads, what a moment's document says
//! (`hook/read`), and how Steery's answer is printed back for Droid to parse
//! (`hook/answer`). Droid runs each hook of `~/.factory/hooks.json` as a shell command with
//! one JSON document on standard input, in snake case like Claude Code's: `session_id`,
//! `transcript_path`, `cwd`, `permission_mode`, `hook_event_name`, then the event's own
//! (`source` and `CLAUDE_ENV_FILE`, `prompt`, `tool_name`, `tool_input`, `tool_response`,
//! `stop_hook_active`, `reason`). It reads the hook's standard output as Claude Code's JSON.
//!
//! As Droid 1.9.0 runs them: a tool's document carries no call id. Droid writes a call into
//! the conversation's file before `PreToolUse` fires and its result after `PostToolUse`, so
//! the call is the newest of that tool and input in the file that has no result yet. `Stop`
//! fires at the end of a turn that failed too, so it says no outcome: the file does.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Answer, Append, Decision, Hook, HookAnswer, HookAnswered, HookKind, HookMoment, HookRead,
    Moment, Says, Via,
};

use super::events;
use super::store::Store;

/// The file Droid names to a `SessionStart` hook, in its document and its environment. The
/// agent's shell sources what is appended there before each command it runs.
const ENV_FILE: &str = "CLAUDE_ENV_FILE";

/// The variable naming a process's lead (PROTOCOL.md §3.3).
const LEAD: &str = "STEERY_LEAD";

/// How long Droid gives a hook that only says a moment happened.
const SAYS_WAITS: u32 = 10;
/// How long Droid gives a hook that may put the call to a person, who answers from a phone;
/// the runtime answers within `waits - 90`. A day.
const ASKS_WAITS: u32 = 86_400;

/// One of Droid's hook events Steery is hooked on, by its name in `hooks.json`.
pub(crate) struct Event {
    pub(crate) name: &'static str,
    kind: HookKind,
    says: &'static [Says],
    /// The seconds setup gives its hook: Droid's `timeout`.
    pub(crate) waits: u32,
}

/// Every event Steery reads, in the order setup writes them. Droid adds a hook's
/// `additionalContext` at `SessionStart`, `UserPromptSubmit` and `PostToolUse`, and goes on
/// from a `Stop` that a hook blocks with a reason. `SubagentStop` and `PreCompact` say
/// nothing that Steery's moments hold.
pub(crate) const EVENTS: [Event; 7] = [
    Event {
        name: "SessionStart",
        kind: HookKind::SessionStart,
        says: &[Says::Context, Says::Notice],
        waits: SAYS_WAITS,
    },
    Event {
        name: "UserPromptSubmit",
        kind: HookKind::Prompt,
        says: &[Says::Context, Says::Notice],
        waits: SAYS_WAITS,
    },
    // Droid reads a decision before a call, and no paragraph.
    Event {
        name: "PreToolUse",
        kind: HookKind::BeforeCall,
        says: &[],
        waits: ASKS_WAITS,
    },
    Event {
        name: "PostToolUse",
        kind: HookKind::AfterCall,
        says: &[Says::Notice, Says::Calls, Says::Working],
        waits: SAYS_WAITS,
    },
    // Droid's documentation names it; no run here fired it.
    Event {
        name: "Notification",
        kind: HookKind::Notification,
        says: &[],
        waits: SAYS_WAITS,
    },
    Event {
        name: "Stop",
        kind: HookKind::TurnEnd,
        says: &[Says::Context],
        waits: SAYS_WAITS,
    },
    Event {
        name: "SessionEnd",
        kind: HookKind::SessionEnd,
        says: &[],
        waits: SAYS_WAITS,
    },
];

/// The hooks `initialize` declares.
pub fn table() -> Vec<Hook> {
    EVENTS
        .iter()
        .map(|event| Hook {
            event: event.name.to_owned(),
            kind: event.kind,
            via: Via::Command,
            says: event.says.to_vec(),
            waits: Some(event.waits),
            env: (event.kind == HookKind::SessionStart)
                .then(|| ENV_FILE.to_owned())
                .into_iter()
                .collect(),
            keeps: None,
        })
        .collect()
}

/// The moment a hook's document says, or none for an event Steery is not hooked on or a
/// document that names no conversation. `store` finds a call's id.
pub fn read(store: &Store, read: HookRead) -> Result<HookMoment, Error> {
    let document = parse(&read.document)?;
    Ok(HookMoment {
        moment: moment(store, &read.event, &document),
    })
}

fn moment(store: &Store, name: &str, document: &Value) -> Option<Moment> {
    let event = event(name)?;
    let external_id = text(document, "session_id")?.to_owned();
    let transcript = text(document, "transcript_path")
        .filter(|path| path.starts_with('/') && !path.chars().any(char::is_control))
        .map(PathBuf::from);
    let call = match event.kind {
        HookKind::BeforeCall | HookKind::AfterCall => {
            let tool = text(document, "tool_name")?;
            let input = document.get("tool_input").cloned().unwrap_or(Value::Null);
            let id = store
                .file(&external_id)
                .ok()
                .and_then(|(file, _)| call_id(&file, tool, &input))
                .unwrap_or_default();
            Some(events::call(&id, tool, input, None))
        }
        _ => None,
    };
    Some(Moment {
        event: name.to_owned(),
        sub_agent: None,
        at: None,
        cwd: text(document, "cwd")
            .filter(|cwd| cwd.starts_with('/') && !cwd.chars().any(char::is_control))
            .map(PathBuf::from),
        transcript,
        call,
        ok: None,
        outcome: None,
        continued: event.kind == HookKind::TurnEnd
            && document.get("stop_hook_active").and_then(Value::as_bool) == Some(true),
        // Droid's reference names the reasons `clear`, `logout`, `prompt_input_exit` and
        // `other`; a `droid exec` run ends `other`.
        person_exited: event.kind == HookKind::SessionEnd
            && matches!(
                text(document, "reason"),
                Some("prompt_input_exit" | "clear" | "logout")
            ),
        prompt: (event.kind == HookKind::Prompt)
            .then(|| document.get("prompt").and_then(Value::as_str))
            .flatten()
            .map(str::to_owned),
        context_left: None,
        model: None,
        thinking: None,
        failure: None,
        suggestions: Vec::new(),
        external_id,
    })
}

/// The id of the newest call of `tool` with `input` in `file` that has no result yet, else
/// of the newest such call.
fn call_id(file: &Path, tool: &str, input: &Value) -> Option<String> {
    let bytes = std::fs::read(file).ok()?;
    let mut calls: Vec<(String, bool)> = Vec::new();
    for line in bytes.split(|&b| b == b'\n') {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        for block in record["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
        {
            match block["type"].as_str() {
                Some("tool_use") if block["name"] == tool && block["input"] == *input => {
                    if let Some(id) = block["id"].as_str() {
                        calls.push((id.to_owned(), false));
                    }
                }
                Some("tool_result") => {
                    for call in calls.iter_mut() {
                        if Some(call.0.as_str()) == block["tool_use_id"].as_str() {
                            call.1 = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    calls
        .iter()
        .rev()
        .find(|(_, answered)| !answered)
        .or(calls.last())
        .map(|(id, _)| id.clone())
}

/// What Steery decided, printed as Droid parses it on this event: a paragraph as
/// `additionalContext`, or at a `Stop` as the reason the turn goes on; a decision as
/// `PreToolUse`'s `permissionDecision`; and the lead, appended to `CLAUDE_ENV_FILE` where
/// Droid named one. Always exit 0: Droid reads exit 2 as a block.
pub fn answer(answer: HookAnswer) -> Result<HookAnswered, Error> {
    let HookAnswer {
        event: name,
        document,
        env,
        answer: Answer {
            say,
            decision,
            lead,
        },
    } = answer;
    let event = event(&name)
        .ok_or_else(|| Error::invalid(format!("Steery has no hook on Droid's {name}")))?;
    let document = parse(&document)?;
    let output = match (event.kind, decision) {
        (_, None) => say.map(|say| context(event, say)).transpose()?,
        (HookKind::BeforeCall, Some(decision)) => {
            if say.is_some() {
                return Err(Error::invalid(
                    "Droid reads no paragraph beside PreToolUse's decision",
                ));
            }
            Some(before_call(decision)?)
        }
        (_, Some(_)) => {
            return Err(Error::invalid(format!(
                "Droid does not read a decision at {name}"
            )));
        }
    };
    let file = text(&document, ENV_FILE).or(env.get(ENV_FILE).map(String::as_str));
    let append = match (lead, file) {
        (Some(lead), Some(file)) if event.kind == HookKind::SessionStart => Some(Append {
            path: PathBuf::from(file),
            text: exported(&lead)?,
        }),
        _ => None,
    };
    Ok(HookAnswered {
        stdout: output.map(|output| output.to_string()).unwrap_or_default(),
        exit_code: 0,
        append,
    })
}

/// A paragraph for the agent, where this event's hook can give one.
fn context(event: &Event, say: String) -> Result<Value, Error> {
    if event.says.is_empty() {
        return Err(Error::invalid(format!(
            "Droid does not read a paragraph at {}",
            event.name
        )));
    }
    Ok(match event.kind {
        HookKind::TurnEnd => json!({"decision": "block", "reason": say}),
        _ => json!({"hookSpecificOutput": {
            "hookEventName": event.name,
            "additionalContext": say,
        }}),
    })
}

/// `PreToolUse`'s decision: the call runs without Droid asking, or is blocked, the reason
/// being what the model reads. Droid keeps no permission from a hook.
fn before_call(decision: Decision) -> Result<Value, Error> {
    let (verdict, reason) = match decision {
        Decision::Allow {
            reason,
            always: None,
        } => ("allow", reason),
        Decision::Allow {
            always: Some(_), ..
        } => {
            return Err(Error::invalid(
                "Droid does not keep a permission from PreToolUse",
            ));
        }
        Decision::Deny { reason } => ("deny", reason),
    };
    Ok(json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": verdict,
        "permissionDecisionReason": reason,
    }}))
}

/// The line that names a lead for every later process, quoted for the shell that sources
/// it: a harness id and a conversation id, neither able to break out of the quotes.
fn exported(lead: &str) -> Result<String, Error> {
    let named = lead.split_once(':').is_some_and(|(harness, external_id)| {
        !harness.is_empty()
            && harness
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !external_id.is_empty()
            && external_id.len() <= 128
            && !external_id.starts_with('.')
            && external_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    });
    if !named {
        return Err(Error::invalid(format!(
            "this lead cannot be exported: {lead:?}"
        )));
    }
    Ok(format!("export {LEAD}='{lead}'\n"))
}

fn event(name: &str) -> Option<&'static Event> {
    EVENTS.iter().find(|event| event.name == name)
}

fn parse(document: &str) -> Result<Value, Error> {
    serde_json::from_str(document)
        .map_err(|error| Error::invalid(format!("the hook's document is not JSON: {error}")))
}

/// A field of the document as text, trimmed, where it holds any.
fn text<'a>(document: &'a Value, field: &str) -> Option<&'a str> {
    document
        .get(field)?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}
