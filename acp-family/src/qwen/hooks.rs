//! Qwen Code's hooks: which of its moments Steery reads, what a moment's document says
//! (`hook/read`), and how Steery's answer is printed back for Qwen to parse
//! (`hook/answer`). Qwen runs each hook as a shell command with one JSON document on
//! standard input, its keys in snake case: `session_id`, `transcript_path` (the chat
//! file), `cwd`, `hook_event_name`, `timestamp`, `permission_mode`, `prompt_id`, then the
//! event's own (`source`, `model`, `prompt`, `tool_name`, `tool_input`, `tool_call_id`,
//! `tool_response`, `error`, `stop_hook_active`, `context_usage`, `reason`,
//! `notification_type`), and `agent_id` where a sub-agent fired it. It reads the hook's
//! standard output as JSON: `{decision, reason}`, or `hookSpecificOutput` with an
//! `additionalContext` or a `permissionDecision`.
//!
//! A tool's document names its call twice: `tool_call_id` is the id the chat file gives
//! the call, `tool_use_id` one of Qwen's own. Qwen's memory keeping writes its files with
//! tools whose hooks fire under the conversation's session id, though the chat file never
//! records those calls.

use std::path::PathBuf;

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Answer, Decision, Hook, HookAnswer, HookAnswered, HookKind, HookMoment, HookRead, Moment, Says,
    TurnState, Via,
};

/// How long Qwen gives a hook that only says a moment happened: its own default.
const SAYS_WAITS: u32 = 60;
/// How long Qwen gives a hook that may put the call to a person, who answers from a phone;
/// the runtime answers within `waits - 90`. Setup writes it in milliseconds, as Qwen reads a
/// `timeout` of 1000 and more (`hook-timeout.ts`), and Node's timer holds at most
/// 2^31 - 1 of them: about 24 days, the longest a hook can wait. Past it Qwen kills the hook
/// and, with no decision, runs the call.
const ASKS_WAITS: u32 = 2_147_483;

/// Qwen's event before a tool call: the one hook that can block it.
pub(crate) const BEFORE_CALL: &str = "PreToolUse";

/// What follows `<hook> PreToolUse` in Qwen's settings. `--approval-mode yolo` runs every
/// call that no hook denies, and Qwen runs a call whose hook exits other than 2. So where
/// Steery's hook fails (the runtime gone, the binary moved) this exits 2, Qwen's block, with
/// standard error as its reason. It does so only in a turn Steery started: the runtime's
/// adapter starts `qwen` itself, with `STEERY_STARTED` naming the adapter's own pid
/// (PROTOCOL.md §3.3), and Qwen runs the hook in a shell of its own, so `$PPID` is Qwen and
/// Qwen's parent is that pid. A person's own Qwen, even one that inherited the variable,
/// says nothing, as every other hook does.
const GUARD: &str = r#" || { s=$?; p=$(ps -o ppid= -p "${PPID:-0}" | tr -d " "); case "${STEERY_STARTED:-}" in "$p":turn*) echo "Steery's hook failed (exit $s) in a turn Steery started, so the call is blocked" >&2; exit 2;; esac; exit 0; }"#;

/// The command setup writes for `event`, after the runtime's `hook` command. A hook whose
/// command cannot run (a machine mid-upgrade, a filled disk) says nothing, as Qwen reads
/// an exit of 2 and more as a block.
pub(crate) fn command(hook: &str, event: &str) -> String {
    if event == BEFORE_CALL {
        format!("{hook} {event}{GUARD}")
    } else {
        format!("{hook} {event} || true")
    }
}

/// Whether `command` is the one setup writes for [`BEFORE_CALL`], for any `hook`.
pub(crate) fn guards(command: &str) -> bool {
    command.ends_with(&format!(" {BEFORE_CALL}{GUARD}"))
}

/// The `timeout` setup writes for a hook that waits `waits` seconds, as Qwen reads it.
pub(crate) fn timeout(waits: u32) -> u64 {
    if waits < 1000 {
        u64::from(waits)
    } else {
        u64::from(waits) * 1000
    }
}

/// One of Qwen's hook events Steery is hooked on, by its name in `settings.json`.
pub(crate) struct Event {
    pub(crate) name: &'static str,
    kind: HookKind,
    says: &'static [Says],
    /// The seconds setup gives its hook, as Qwen's `timeout` counts them.
    pub(crate) waits: u32,
}

/// Every event Qwen fires once per moment that Steery reads, in the order setup writes
/// them. Qwen adds a hook's `additionalContext` to the conversation at `SessionStart`,
/// `UserPromptSubmit`, `PostToolUse` and `PostToolUseFailure`, and goes on from a `Stop`
/// that a hook blocks with a reason.
pub(crate) const EVENTS: [Event; 8] = [
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
    // Qwen reads no paragraph before a call that Steery answers: only a deny's reason,
    // which the model reads.
    Event {
        name: BEFORE_CALL,
        kind: HookKind::BeforeCall,
        says: &[],
        waits: ASKS_WAITS,
    },
    // Words for a conversation Steery started reach it deep in a tool loop as a call comes back.
    Event {
        name: "PostToolUse",
        kind: HookKind::AfterCall,
        says: &[Says::Notice, Says::Calls, Says::Working],
        waits: SAYS_WAITS,
    },
    Event {
        name: "PostToolUseFailure",
        kind: HookKind::AfterCall,
        says: &[Says::Notice, Says::Calls, Says::Working],
        waits: SAYS_WAITS,
    },
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
            // Qwen hands a hook no file to export a lead into.
            env: Vec::new(),
            keeps: None,
        })
        .collect()
}

/// The moment a hook's document says, or none for an event Steery is not hooked on or a
/// document that names no conversation.
pub fn read(read: HookRead) -> Result<HookMoment, Error> {
    let document = parse(&read.document)?;
    Ok(HookMoment {
        moment: moment(&read.event, &document),
    })
}

fn moment(name: &str, document: &Value) -> Option<Moment> {
    let event = event(name)?;
    let external_id = text(document, "session_id")?.to_owned();
    let call = match event.kind {
        HookKind::BeforeCall | HookKind::AfterCall => Some(super::events::call(
            text(document, "tool_call_id").unwrap_or_default(),
            text(document, "tool_name")?,
            document.get("tool_input").cloned().unwrap_or(Value::Null),
        )),
        _ => None,
    };
    let sub_agent = text(document, "agent_id").map(str::to_owned);
    Some(Moment {
        event: name.to_owned(),
        at: text(document, "timestamp").and_then(super::events::millis),
        cwd: absolute(document, "cwd"),
        // A sub-agent's moment names its lead's chat file.
        transcript: absolute(document, "transcript_path").filter(|_| sub_agent.is_none()),
        sub_agent,
        call,
        ok: (event.kind == HookKind::AfterCall).then_some(name == "PostToolUse"),
        outcome: (event.kind == HookKind::TurnEnd).then_some(TurnState::Finished),
        continued: event.kind == HookKind::TurnEnd
            && document.get("stop_hook_active").and_then(Value::as_bool) == Some(true),
        // Qwen says `prompt_input_exit` as its process ends, and `clear` as `/clear`
        // starts the next conversation.
        person_exited: event.kind == HookKind::SessionEnd
            && text(document, "reason") == Some("prompt_input_exit"),
        prompt: (event.kind == HookKind::Prompt)
            .then(|| text(document, "prompt"))
            .flatten()
            .map(str::to_owned),
        context_left: (event.kind == HookKind::TurnEnd)
            .then(|| document.get("context_usage").and_then(Value::as_f64))
            .flatten()
            .map(|used| (1.0 - used).clamp(0.0, 1.0)),
        model: (event.kind == HookKind::SessionStart)
            .then(|| text(document, "model"))
            .flatten()
            .map(str::to_owned),
        thinking: None,
        failure: None,
        suggestions: Vec::new(),
        external_id,
    })
}

/// What Steery decided, printed as Qwen parses it on this event: a paragraph as
/// `additionalContext`, or at `Stop` as the reason the agent goes on; a deny as
/// `PreToolUse`'s `permissionDecision`. Always exit 0: Qwen reads exit 2 as a block whose
/// reason is standard error. Qwen hands a hook no file to export a lead into: a lead is
/// dropped.
pub fn answer(answer: HookAnswer) -> Result<HookAnswered, Error> {
    let HookAnswer {
        event: name,
        answer: Answer { say, decision, .. },
        ..
    } = answer;
    let event = event(&name)
        .ok_or_else(|| Error::invalid(format!("Steery has no hook on Qwen's {name}")))?;
    let output = match (event.kind, decision) {
        (_, None) => say.map(|say| context(event, say)).transpose()?,
        (HookKind::BeforeCall, Some(decision)) => before_call(decision),
        (_, Some(_)) => {
            return Err(Error::invalid(format!(
                "Qwen Code does not read a decision at {name}"
            )));
        }
    };
    Ok(HookAnswered {
        stdout: output.map(|output| output.to_string()).unwrap_or_default(),
        exit_code: 0,
        append: None,
    })
}

/// A paragraph for the agent, where this event's hook can give one.
fn context(event: &Event, say: String) -> Result<Value, Error> {
    if event.says.is_empty() {
        return Err(Error::invalid(format!(
            "Qwen Code does not read a paragraph at {}",
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

/// `PreToolUse`'s decision. A deny blocks the call, its reason being what the model reads.
/// An allow prints nothing, so the call goes as Qwen's own permissions let it: at observe
/// Steery widens no permission.
fn before_call(decision: Decision) -> Option<Value> {
    match decision {
        Decision::Allow { .. } => None,
        Decision::Deny { reason } => Some(json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }})),
    }
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

/// A field of the document as an absolute path.
fn absolute(document: &Value, field: &str) -> Option<PathBuf> {
    text(document, field)
        .filter(|path| path.starts_with('/') && !path.chars().any(char::is_control))
        .map(PathBuf::from)
}
