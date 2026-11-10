//! Grok Build's hooks: which of its moments Steery reads, what a moment's document says
//! (`hook/read`), and how Steery's answer is printed back for Grok to parse
//! (`hook/answer`). Grok runs each hook as a shell command with one JSON document on
//! standard input, its keys in camel case (`hookEventName: "pre_tool_use"`, `sessionId`,
//! `toolName`) with Claude Code's snake-case spellings added beside them
//! (`hook_event_name: "PreToolUse"`, `session_id`, `tool_name`); the camel key wins where
//! both are. It reads the hook's standard output as JSON: `hookSpecificOutput` with a
//! `permissionDecision` or an `additionalContext`, or `{decision: "block", reason}`.

use std::path::PathBuf;

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Answer, Call, Decision, Group, Hook, HookAnswer, HookAnswered, HookKind, HookMoment, HookRead,
    Moment, Says, TurnState, Via,
};

/// How long Grok gives a hook that only says a moment happened: its own default.
const SAYS_WAITS: u32 = 5;
/// How long Grok gives a hook that may put the call to a person, who answers from a phone;
/// the runtime answers within `waits - 90`. A day: Grok reads `timeout` as whole seconds.
const ASKS_WAITS: u32 = 86_400;

/// Grok's event before a tool call: the one hook that can block it.
pub(crate) const BEFORE_CALL: &str = "PreToolUse";

/// One of Grok's hook events Steery is hooked on, by its name in Grok's hook files.
pub(crate) struct Event {
    pub(crate) name: &'static str,
    kind: HookKind,
    says: &'static [Says],
    /// The `timeout` setup writes on its hook, in seconds.
    pub(crate) waits: u32,
}

/// Every event Grok fires once per moment that Steery reads, in the order setup writes
/// them. Grok reads a hook's `additionalContext` beside an allow, and goes on from a
/// `Stop` that a hook blocks with a reason.
pub(crate) const EVENTS: [Event; 6] = [
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
            // Grok hands a hook no file to export a lead into.
            env: Vec::new(),
            keeps: None,
        })
        .collect()
}

/// The moment a hook's document says, or none for an event Steery is not hooked on, a
/// document that names no conversation, or the `Stop` Grok fires as a conversation shuts
/// down, which ends no turn.
pub fn read(read: HookRead) -> Result<HookMoment, Error> {
    let document = parse(&read.document)?;
    Ok(HookMoment {
        moment: moment(&read.event, &document),
    })
}

fn moment(name: &str, document: &Value) -> Option<Moment> {
    let event = event(name)?;
    let external_id = field(document, "sessionId", "session_id")?.to_owned();
    if event.kind == HookKind::TurnEnd && text(document, "reason") == Some("shutdown") {
        return None;
    }
    let call = match event.kind {
        HookKind::BeforeCall | HookKind::AfterCall => {
            let tool = field(document, "toolName", "tool_name")?;
            let input = document
                .get("toolInput")
                .or_else(|| document.get("tool_input"))
                .cloned()
                .unwrap_or(Value::Null);
            Some(call(
                field(document, "toolUseId", "tool_use_id").unwrap_or_default(),
                tool,
                input,
            ))
        }
        _ => None,
    };
    Some(Moment {
        event: name.to_owned(),
        sub_agent: None,
        at: text(document, "timestamp").and_then(super::events::millis),
        cwd: document
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| cwd.starts_with('/') && !cwd.chars().any(char::is_control))
            .map(PathBuf::from),
        // Grok files every transcript under its own root.
        transcript: None,
        call,
        ok: (event.kind == HookKind::AfterCall).then_some(true),
        outcome: (event.kind == HookKind::TurnEnd).then_some(TurnState::Finished),
        continued: event.kind == HookKind::TurnEnd
            && document
                .get("stopHookActive")
                .or_else(|| document.get("stop_hook_active"))
                .and_then(Value::as_bool)
                == Some(true),
        // Grok's `SessionEnd` reasons (`shutdown`, `channel_closed`) do not tell a person
        // leaving at the prompt from a run that ended.
        person_exited: false,
        prompt: (event.kind == HookKind::Prompt)
            .then(|| document.get("prompt").and_then(Value::as_str))
            .flatten()
            .map(str::to_owned),
        context_left: None,
        model: (event.kind == HookKind::SessionStart)
            .then(|| text(document, "modelId"))
            .flatten()
            .map(str::to_owned),
        thinking: None,
        failure: None,
        suggestions: Vec::new(),
        external_id,
    })
}

/// The call a tool's moment names. An MCP tool is `<server>__<tool>`, as Grok's
/// transcript spells it `mcp__<server>__<tool>`; Steery's own server's tools are Steery's.
fn call(id: &str, name: &str, input: Value) -> Call {
    let served = name.strip_prefix("mcp__").unwrap_or(name).split_once("__");
    let (tool, group, server, steery) = match served {
        Some(("steery", inner)) => (
            format!("mcp__steery__{inner}"),
            Group::Steery,
            None,
            Some(inner.to_owned()),
        ),
        Some((server, inner)) => (
            format!("mcp__{server}__{inner}"),
            Group::Mcp,
            Some(server.to_owned()),
            None,
        ),
        None => (name.to_owned(), group(name), None, None),
    };
    Call {
        id: id.to_owned(),
        tool,
        input,
        group,
        server,
        steery,
        asks: None,
        parent: None,
        response: None,
    }
}

/// The group of a Grok tool by its name, as its `tool_definitions.json` lists them.
fn group(tool: &str) -> Group {
    match tool {
        "read_file" | "list_dir" => Group::FilesRead,
        "write" | "search_replace" => Group::FilesEdit,
        "grep" => Group::Search,
        "run_terminal_command"
        | "get_command_or_subagent_output"
        | "kill_command_or_subagent"
        | "monitor" => Group::Shell,
        "enter_plan_mode" | "exit_plan_mode" | "todo_write" => Group::Plan,
        "web_fetch" | "web_search" => Group::Web,
        "spawn_subagent" => Group::Agents,
        "ask_user_question" => Group::Ask,
        "image_gen" | "image_edit" | "image_to_video" | "reference_to_video" | "video_gen" => {
            Group::Media
        }
        "search_tool" | "use_tool" | "workflow" | "send_feedback" | "scheduler_create"
        | "scheduler_delete" | "scheduler_list" => Group::Harness,
        _ => Group::Custom,
    }
}

/// What Steery decided, printed as Grok parses it on this event: a paragraph as
/// `additionalContext`, or at a `Stop` as the reason the turn goes on; a decision as
/// `PreToolUse`'s `permissionDecision`. Always exit 0: Grok reads exit 2 as a block whose
/// reason is standard error, and any other code as a hook that failed open. Grok hands a
/// hook no file to export a lead into: a lead is dropped.
pub fn answer(answer: HookAnswer) -> Result<HookAnswered, Error> {
    let HookAnswer {
        event: name,
        answer: Answer { say, decision, .. },
        ..
    } = answer;
    let event = event(&name)
        .ok_or_else(|| Error::invalid(format!("Steery has no hook on Grok's {name}")))?;
    let output = match (event.kind, decision) {
        (_, None) => say.map(|say| context(event, say)).transpose()?,
        (HookKind::BeforeCall, Some(decision)) => Some(before_call(decision, say)?),
        (_, Some(_)) => {
            return Err(Error::invalid(format!(
                "Grok does not read a decision at {name}"
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
            "Grok does not read a paragraph at {}",
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

/// `PreToolUse`'s decision: the call runs without Grok asking, or is blocked, the reason
/// being what the model reads. A paragraph beside it is context, not a reason. Grok keeps
/// no standing permission from a hook.
fn before_call(decision: Decision, say: Option<String>) -> Result<Value, Error> {
    let (verdict, reason) = match decision {
        Decision::Allow {
            reason,
            always: None,
        } => ("allow", reason),
        Decision::Allow {
            always: Some(_), ..
        } => {
            return Err(Error::invalid(
                "Grok does not keep a permission from PreToolUse",
            ));
        }
        Decision::Deny { reason } => ("deny", reason),
    };
    let mut output = json!({
        "hookEventName": "PreToolUse",
        "permissionDecision": verdict,
        "permissionDecisionReason": reason,
    });
    if let Some(say) = say {
        output["additionalContext"] = json!(say);
    }
    Ok(json!({"hookSpecificOutput": output}))
}

fn event(name: &str) -> Option<&'static Event> {
    EVENTS.iter().find(|event| event.name == name)
}

fn parse(document: &str) -> Result<Value, Error> {
    serde_json::from_str(document)
        .map_err(|error| Error::invalid(format!("the hook's document is not JSON: {error}")))
}

/// A field of the document as text by its camel-case key, else its snake-case one.
fn field<'a>(document: &'a Value, camel: &str, snake: &str) -> Option<&'a str> {
    text(document, camel).or_else(|| text(document, snake))
}

/// A field of the document as text, trimmed, where it holds any.
fn text<'a>(document: &'a Value, field: &str) -> Option<&'a str> {
    document
        .get(field)?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}
