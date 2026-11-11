//! Copilot's hooks: which of its moments Steery reads, what a moment's document says
//! (`hook/read`), and how Steery's answer is printed back for Copilot to parse
//! (`hook/answer`). Copilot runs each hook as a shell command with one JSON document on
//! standard input, in the camelCase form its hooks file names the events in, the
//! conversation's own id in its `sessionId`, and reads the hook's standard output as JSON.

use std::path::PathBuf;

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Answer, Decision, Hook, HookAnswer, HookAnswered, HookKind, HookMoment, HookRead, Kept, Moment,
    Says, TurnState, Via,
};

use super::calls;
use super::transcripts::Store;

/// How long Copilot gives a hook that only says a moment happened: it writes one note, and
/// a harness held up behind Steery is the one thing it must never cost a person.
const SAYS_WAITS: u32 = 5;
/// How long Copilot gives a hook that may put the call to a person, who answers from a
/// phone; the runtime answers within `waits - 90`. As long as a Node timer holds (a 32-bit
/// count of milliseconds, about 24 days), so the person's card stays open exactly while
/// Copilot waits on the hook.
const ASKS_WAITS: u32 = (i32::MAX / 1000) as u32;

/// One of Copilot's hook events Steery is hooked on.
pub(crate) struct Event {
    pub(crate) name: &'static str,
    kind: HookKind,
    says: &'static [Says],
    /// The `timeoutSec` setup writes on its hook.
    pub(crate) waits: u32,
}

impl Event {
    /// Whether its answer decides if a call runs.
    pub(crate) fn gates(&self) -> bool {
        matches!(self.kind, HookKind::BeforeCall | HookKind::Permission)
    }
}

/// Every event Copilot fires once per moment that Steery reads, in the order setup writes
/// them. Copilot reads a hook's `additionalContext` on `sessionStart`, `postToolUse` and
/// `subagentStart`, and goes on from an `agentStop` that a hook blocks with a reason. It
/// drops what a `userPromptSubmitted` hook prints. `preCompact`, `errorOccurred`,
/// `subagentStop` and `userPromptTransformed` have no moment Steery reads.
pub(crate) const EVENTS: [Event; 10] = [
    Event {
        name: "sessionStart",
        kind: HookKind::SessionStart,
        says: &[Says::Context, Says::Notice],
        waits: SAYS_WAITS,
    },
    Event {
        name: "userPromptSubmitted",
        kind: HookKind::Prompt,
        says: &[],
        waits: SAYS_WAITS,
    },
    Event {
        name: "preToolUse",
        kind: HookKind::BeforeCall,
        says: &[],
        waits: ASKS_WAITS,
    },
    // Words for a conversation reach it deep in a tool loop as a call comes back.
    Event {
        name: "postToolUse",
        kind: HookKind::AfterCall,
        says: &[Says::Notice, Says::Calls, Says::Working],
        waits: SAYS_WAITS,
    },
    // Fires in place of `postToolUse` when the call fails. Copilot reads its
    // `additionalContext` only from a hook that exits 2, which setup's `|| true` never does.
    Event {
        name: "postToolUseFailure",
        kind: HookKind::AfterCall,
        says: &[],
        waits: SAYS_WAITS,
    },
    Event {
        name: "permissionRequest",
        kind: HookKind::Permission,
        says: &[],
        waits: ASKS_WAITS,
    },
    // Its `additionalContext` is a user message that can start a turn: Steery gives none.
    Event {
        name: "notification",
        kind: HookKind::Notification,
        says: &[],
        waits: SAYS_WAITS,
    },
    Event {
        name: "agentStop",
        kind: HookKind::TurnEnd,
        says: &[Says::Context],
        waits: SAYS_WAITS,
    },
    // `/clear` fires it detached, with `reason: "user_exit"`.
    Event {
        name: "sessionEnd",
        kind: HookKind::SessionEnd,
        says: &[],
        waits: SAYS_WAITS,
    },
    // Fired in the lead's hooks; its `additionalContext` is prepended to the sub-agent's
    // first message. The built-in `general-purpose` agent fires none.
    Event {
        name: "subagentStart",
        kind: HookKind::SubAgentStart,
        says: &[Says::Context],
        waits: SAYS_WAITS,
    },
];

/// The hooks `initialize` declares on `protocol`.
pub fn table(protocol: u32) -> Vec<Hook> {
    EVENTS
        .iter()
        .map(|event| Hook {
            event: event.name.to_owned(),
            kind: event.kind,
            via: Via::Command,
            says: event.says.to_vec(),
            waits: Some(event.waits),
            // Copilot hands a hook no file to append a lead to.
            env: Vec::new(),
            // `permissionRequest`'s output is `behavior` and `message` alone: a yes covers
            // its one call, so Copilot keeps neither scope.
            keeps: (event.kind == HookKind::Permission && protocol >= steery_sdk::KEEPS)
                .then(Vec::<Kept>::new),
        })
        .collect()
}

/// The moment a hook's document says, or none for an event Steery is not hooked on or a
/// document that names no conversation. Copilot keeps every transcript under the store's
/// root, so no moment names one.
pub fn read(_store: &Store, read: HookRead) -> Result<HookMoment, Error> {
    let document = parse(&read.document)?;
    Ok(HookMoment {
        moment: moment(&read.event, &document),
    })
}

fn moment(name: &str, document: &Value) -> Option<Moment> {
    let event = event(name)?;
    let external_id = text(document, "sessionId")?.to_owned();
    let tool = text(document, "toolName");
    let call = match (event.kind, tool) {
        (HookKind::BeforeCall | HookKind::AfterCall, Some(tool)) => text(document, "toolCallId")
            .map(|id| calls::call(id, tool, input(document, "toolArgs"))),
        // `permissionRequest` names the tool and its input but no `toolCallId`: the call is
        // still the one a person decides on, under no id.
        (HookKind::Permission, Some(tool)) => Some(calls::call(
            text(document, "toolCallId").unwrap_or_default(),
            tool,
            input(document, "toolInput"),
        )),
        _ => None,
    };
    Some(Moment {
        event: name.to_owned(),
        sub_agent: text(document, "agentId").map(str::to_owned),
        at: document.get("timestamp").and_then(Value::as_u64),
        cwd: document
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| cwd.starts_with('/') && !cwd.chars().any(char::is_control))
            .map(PathBuf::from),
        transcript: None,
        call,
        ok: match name {
            "postToolUse" => Some(true),
            "postToolUseFailure" => Some(false),
            _ => None,
        },
        outcome: (event.kind == HookKind::TurnEnd).then_some(TurnState::Finished),
        continued: event.kind == HookKind::TurnEnd
            && document.get("stop_hook_active").and_then(Value::as_bool) == Some(true),
        // `user_exit` is `/clear` or the person quitting; `complete`, `error`, `abort` and
        // `timeout` end a run without the person leaving it.
        person_exited: event.kind == HookKind::SessionEnd
            && document.get("reason").and_then(Value::as_str) == Some("user_exit"),
        prompt: (event.kind == HookKind::Prompt)
            .then(|| document.get("prompt").and_then(Value::as_str))
            .flatten()
            .map(str::to_owned),
        context_left: None,
        model: None,
        thinking: None,
        failure: None,
        // Copilot's answer cannot pass a suggestion back, so none is offered.
        suggestions: Vec::new(),
        external_id,
    })
}

/// A call's input as the document spells it; `null` where it gives none.
fn input(document: &Value, field: &str) -> Value {
    document.get(field).cloned().unwrap_or(Value::Null)
}

/// What Steery decided, printed as Copilot parses it on this event: a paragraph as
/// `additionalContext`, or at an `agentStop` as the reason the turn goes on; a decision as
/// `preToolUse`'s `permissionDecision` or `permissionRequest`'s `behavior`. Copilot names
/// no file for a lead, so a lead leaves the processes unled. Always exit 0: a hook that
/// exits 2 is a deny at `permissionRequest`.
pub fn answer(answer: HookAnswer) -> Result<HookAnswered, Error> {
    let HookAnswer {
        event: name,
        answer: Answer { say, decision, .. },
        ..
    } = answer;
    let event = event(&name)
        .ok_or_else(|| Error::invalid(format!("Steery has no hook on Copilot's {name}")))?;
    let output = match (event.kind, decision) {
        (_, None) => say.map(|say| context(event, say)).transpose()?,
        (HookKind::BeforeCall | HookKind::Permission, Some(_)) if say.is_some() => {
            return Err(Error::invalid(format!(
                "Copilot does not read a paragraph at {name}"
            )));
        }
        (HookKind::BeforeCall, Some(decision)) => Some(before_call(decision)?),
        (HookKind::Permission, Some(decision)) => Some(permission(decision)?),
        (_, Some(_)) => {
            return Err(Error::invalid(format!(
                "Copilot does not read a decision at {name}"
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
            "Copilot does not read a paragraph at {}",
            event.name
        )));
    }
    Ok(match event.kind {
        HookKind::TurnEnd => json!({"decision": "block", "reason": say}),
        _ => json!({"additionalContext": say}),
    })
}

/// `preToolUse`'s decision: the call runs without Copilot asking, or is blocked, the reason
/// being what the model reads (Copilot requires one on a deny). Copilot keeps no
/// permission from this moment.
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
                "Copilot does not keep a permission from preToolUse",
            ));
        }
        Decision::Deny { reason } => ("deny", reason),
    };
    Ok(json!({
        "permissionDecision": verdict,
        "permissionDecisionReason": reason,
    }))
}

/// `permissionRequest`'s decision. Copilot's allow carries no message and keeps no scope;
/// a deny's message is what the model reads.
fn permission(decision: Decision) -> Result<Value, Error> {
    Ok(match decision {
        Decision::Allow { always: None, .. } => json!({"behavior": "allow"}),
        Decision::Allow {
            always: Some(_), ..
        } => {
            return Err(Error::invalid(
                "Copilot does not keep a permission from permissionRequest",
            ));
        }
        Decision::Deny { reason } => json!({"behavior": "deny", "message": reason}),
    })
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
