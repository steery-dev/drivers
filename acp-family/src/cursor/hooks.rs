//! Cursor's own hooks: which of its moments Steery reads, what a moment's document says
//! (`hook/read`), and how Steery's answer is printed back for Cursor to parse
//! (`hook/answer`). Cursor runs each hook in `~/.cursor/hooks.json` as a shell command with
//! one JSON document on standard input, in snake case (`hook_event_name: "preToolUse"`,
//! `conversation_id`, `tool_use_id`), and reads the hook's standard output as JSON.
//!
//! As `cursor-agent` 2026.11.05 runs them: an interactive turn fires every event here; a
//! `-p` turn fires no `beforeSubmitPrompt` and no `stop`; a turn an ACP client drives fires
//! none. A sub-agent's calls fire under its own `conversation_id`, which names no parent, and
//! `subagentStart` never fires. The `tool_use_id` of a shell or MCP call is not the call id
//! that the chat's store holds.

use std::path::PathBuf;

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Answer, Call, Decision, Group, Hook, HookAnswer, HookAnswered, HookKind, HookMoment, HookRead,
    Moment, Says, TurnState, Via,
};

use super::calls;

/// How long Cursor gives a hook that only says a moment happened.
const SAYS_WAITS: u32 = 5;
/// How long Cursor gives a hook that may put the call to a person, who answers from a
/// phone; the runtime answers within `waits - 90`. A day: Cursor reads `timeout` as seconds.
const ASKS_WAITS: u32 = 86_400;

/// One of Cursor's hook events Steery is hooked on, by its name in `hooks.json`.
pub(crate) struct Event {
    pub(crate) name: &'static str,
    kind: HookKind,
    says: &'static [Says],
    /// The `timeout` setup writes on its hook, in seconds.
    pub(crate) waits: u32,
}

/// Every event Cursor fires once per moment that Steery reads, in the order setup writes
/// them. Cursor reads `additional_context` at a start and after a call, beside a
/// `preToolUse` decision, and a `stop`'s `followup_message` as the person's next words.
pub(crate) const EVENTS: [Event; 7] = [
    Event {
        name: "sessionStart",
        kind: HookKind::SessionStart,
        says: &[Says::Context, Says::Notice],
        waits: SAYS_WAITS,
    },
    // Cursor reads no paragraph here: only whether the prompt goes on.
    Event {
        name: "beforeSubmitPrompt",
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
    // Words for a conversation Steery started reach it deep in a tool loop as a call comes back.
    Event {
        name: "postToolUse",
        kind: HookKind::AfterCall,
        says: &[Says::Notice, Says::Calls, Says::Working],
        waits: SAYS_WAITS,
    },
    Event {
        name: "postToolUseFailure",
        kind: HookKind::AfterCall,
        says: &[Says::Notice, Says::Calls, Says::Working],
        waits: SAYS_WAITS,
    },
    Event {
        name: "stop",
        kind: HookKind::TurnEnd,
        says: &[Says::Context],
        waits: SAYS_WAITS,
    },
    Event {
        name: "sessionEnd",
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
            // A `sessionStart`'s `env` reaches later hooks, not the agent's shell: a lead has
            // nowhere to go. Cursor also hands each hook its sign-in token, which no entry names.
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
    let external_id = text(document, "conversation_id")?.to_owned();
    let call = match event.kind {
        HookKind::BeforeCall | HookKind::AfterCall => Some(call(
            // Cursor's own ids can hold a line break; the store keeps them so.
            document
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            text(document, "tool_name")?,
            document.get("tool_input").cloned().unwrap_or(Value::Null),
        )),
        _ => None,
    };
    let loops = document.get("loop_count").and_then(Value::as_u64);
    Some(Moment {
        event: name.to_owned(),
        sub_agent: None,
        at: None,
        // A call's `cwd` is often empty; the workspace is where the chat runs.
        cwd: text(document, "cwd")
            .into_iter()
            .chain(
                document
                    .get("workspace_roots")
                    .and_then(Value::as_array)
                    .and_then(|roots| roots.first())
                    .and_then(Value::as_str),
            )
            .find(|cwd| cwd.starts_with('/') && !cwd.chars().any(char::is_control))
            .map(PathBuf::from),
        // Its JSONL is a copy beside the store that the driver reads.
        transcript: None,
        call,
        ok: (event.kind == HookKind::AfterCall).then_some(name == "postToolUse"),
        outcome: (event.kind == HookKind::TurnEnd).then(|| match text(document, "status") {
            Some("aborted") => TurnState::Interrupted,
            Some("error") => TurnState::Failed,
            _ => TurnState::Finished,
        }),
        continued: event.kind == HookKind::TurnEnd && loops.is_some_and(|loops| loops > 0),
        // A person who quits at the prompt and a `-p` run that ends both say `completed`.
        person_exited: false,
        prompt: (event.kind == HookKind::Prompt)
            .then(|| document.get("prompt").and_then(Value::as_str))
            .flatten()
            .map(str::to_owned),
        context_left: None,
        model: text(document, "model_id")
            .or_else(|| text(document, "model"))
            .map(str::to_owned),
        thinking: None,
        failure: None,
        suggestions: Vec::new(),
        external_id,
    })
}

/// The call a tool's moment names. Cursor names an MCP tool `MCP:<tool>`, without its
/// server, so Steery's own tools cannot be told from another server's: the tool keeps
/// Cursor's spelling.
fn call(id: &str, name: &str, input: Value) -> Call {
    if !name.starts_with("MCP:") {
        return calls::call(id, Some(name), Some(&input), None);
    }
    Call {
        id: id.to_owned(),
        tool: name.to_owned(),
        input,
        group: Group::Mcp,
        server: None,
        steery: None,
        asks: None,
        parent: None,
        response: None,
    }
}

/// What Steery decided, printed as Cursor parses it on this event: a paragraph as
/// `additional_context`, or at a `stop` as the follow-up message that continues the turn; a
/// decision as `preToolUse`'s `permission`. Always exit 0: Cursor reads exit 2 as a block.
/// A lead is dropped (see [`table`]).
pub fn answer(answer: HookAnswer) -> Result<HookAnswered, Error> {
    let HookAnswer {
        event: name,
        answer: Answer { say, decision, .. },
        ..
    } = answer;
    let event = event(&name)
        .ok_or_else(|| Error::invalid(format!("Steery has no hook on Cursor's {name}")))?;
    let output = match (event.kind, decision) {
        (_, None) => say.map(|say| context(event, say)).transpose()?,
        (HookKind::BeforeCall, Some(decision)) => Some(before_call(decision, say)?),
        (_, Some(_)) => {
            return Err(Error::invalid(format!(
                "Cursor does not read a decision at {name}"
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
            "Cursor does not read a paragraph at {}",
            event.name
        )));
    }
    Ok(match event.kind {
        HookKind::TurnEnd => json!({"followup_message": say}),
        _ => json!({"additional_context": say}),
    })
}

/// `preToolUse`'s decision. At a deny the model reads `user_message`; Cursor ignores
/// `agent_message`. A paragraph beside it is context. Cursor keeps no standing permission
/// from a hook.
fn before_call(decision: Decision, say: Option<String>) -> Result<Value, Error> {
    let mut output = match decision {
        Decision::Allow { always: None, .. } => json!({"permission": "allow"}),
        Decision::Allow {
            always: Some(_), ..
        } => {
            return Err(Error::invalid(
                "Cursor does not keep a permission from preToolUse",
            ));
        }
        Decision::Deny { reason } => json!({"permission": "deny", "user_message": reason}),
    };
    if let Some(say) = say {
        output["additional_context"] = json!(say);
    }
    Ok(output)
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
