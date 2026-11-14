//! Steery's hooks on Kiro: the moments of the person's own `kiro-cli chat`, which Kiro runs
//! from `~/.kiro/hooks/steery.json`, and the permissions a turn this driver runs asks for
//! ([`crate::turn::hooks`]).
//!
//! Kiro 3.3.0 runs each hook as a shell command with one JSON document on standard input, in
//! snake case like Claude Code's: `session_id` (`sess_…`), `hook_event_name`, `cwd`, then the
//! event's own: `prompt`, `tool_name`, `tool_input`, `tool_response`, and at `SessionEnd`
//! the `reason`. It names no transcript and no call id. Of a hook's answer it reads a
//! decision only from its exit (2 blocks a call or a prompt), and at `Stop` a JSON
//! `{"decision":"block","reason":…}`, with which the agent goes on. An ACP turn loads no
//! hook files unless its client asks for them, and this driver's turn does not, so these
//! moments come only from runs that a person started.
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Answer, Hook, HookAnswer, HookAnswered, HookKind, HookMoment, HookRead, Moment, Says, Via,
};

use super::events;
use super::store::Store;
use crate::turn::hooks as turn_hooks;

/// How long Kiro gives a hook that only says a moment happened, in seconds.
const SAYS_WAITS: u32 = 10;

/// What an `allow_always` of Kiro's keeps.
const ALWAYS: &str = "calls like it in this conversation: Kiro keeps the choice for the session";

/// One of Kiro's hook triggers Steery is hooked on, by its name in a hooks file.
pub(crate) struct Event {
    pub(crate) name: &'static str,
    kind: HookKind,
    says: &'static [Says],
}

/// Every trigger Steery reads, in the order setup writes them. Kiro reads a paragraph only
/// at `Stop`, as the reason the agent goes on. `PreToolUse` is not among them: Kiro reads
/// no decision from a hook's output there, only its exit, and the call is in the file.
pub(crate) const EVENTS: [Event; 5] = [
    Event {
        name: "SessionStart",
        kind: HookKind::SessionStart,
        says: &[],
    },
    Event {
        name: "UserPromptSubmit",
        kind: HookKind::Prompt,
        says: &[],
    },
    Event {
        name: "PostToolUse",
        kind: HookKind::AfterCall,
        says: &[],
    },
    Event {
        name: "Stop",
        kind: HookKind::TurnEnd,
        says: &[Says::Context],
    },
    Event {
        name: "SessionEnd",
        kind: HookKind::SessionEnd,
        says: &[],
    },
];

/// The hooks `initialize` declares: Kiro's own, then the turn's.
pub fn table(protocol: u32) -> Vec<Hook> {
    EVENTS
        .iter()
        .map(|event| Hook {
            event: event.name.to_owned(),
            kind: event.kind,
            via: Via::Command,
            says: event.says.to_vec(),
            waits: Some(SAYS_WAITS),
            // Kiro hands a hook no file to export a lead into.
            env: Vec::new(),
            keeps: None,
        })
        .chain(turn_hooks::table(protocol))
        .collect()
}

/// Whether `event` is a turn's, which [`turn`] and the shared answer handle.
pub fn of_turn(event: &str) -> bool {
    [turn_hooks::PERMISSION, turn_hooks::ENDED].contains(&event)
}

/// The moment a hook's document says, or none for an event Steery is not hooked on or a
/// document that names no conversation.
pub fn read(store: &Store, read: HookRead) -> Result<HookMoment, Error> {
    let document = turn_hooks::parse(&read.document)?;
    Ok(HookMoment {
        moment: moment(store, &read.event, &document),
    })
}

fn moment(store: &Store, name: &str, document: &Value) -> Option<Moment> {
    let event = event(name)?;
    let external_id = text(document, "session_id")?.to_owned();
    let call = match event.kind {
        HookKind::AfterCall => {
            let tool = text(document, "tool_name")?;
            let input = document.get("tool_input").cloned().unwrap_or(Value::Null);
            let id = store
                .records(&external_id)
                .ok()
                .and_then(|records| call_id(&records, tool, &input))
                .unwrap_or_default();
            Some(events::call(&id, tool, input, None))
        }
        _ => None,
    };
    let transcript = store
        .folder(&external_id)
        .ok()
        .map(|(folder, _)| folder.join("messages.jsonl"));
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
        continued: false,
        // A person who quits `kiro-cli chat` ends it `client_disconnect`, as a turn's
        // process that ends does; Kiro names no reason of the person's alone.
        person_exited: false,
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

/// The id of the newest call of `tool` with `input` that has a result, else of the newest
/// such call: Kiro runs `PostToolUse` after it writes the result.
fn call_id(records: &[Value], tool: &str, input: &Value) -> Option<String> {
    let mut calls: Vec<(String, bool)> = Vec::new();
    for record in records {
        let payload = &record["payload"];
        match payload["type"].as_str() {
            Some("tool_call") if payload["toolName"] == tool && same(&payload["args"], input) => {
                if let Some(id) = payload["toolCallId"].as_str() {
                    calls.push((id.to_owned(), false));
                }
            }
            Some("tool_result") => {
                for call in calls.iter_mut() {
                    if Some(call.0.as_str()) == payload["toolCallId"].as_str() {
                        call.1 = true;
                    }
                }
            }
            _ => {}
        }
    }
    calls
        .iter()
        .rev()
        .find(|(_, answered)| *answered)
        .or(calls.last())
        .map(|(id, _)| id.clone())
}

/// Whether a call's `args` in the file are the hook's `tool_input`: the hook's names every
/// field, null where the call left it out.
fn same(args: &Value, input: &Value) -> bool {
    let (Some(args), Some(input)) = (args.as_object(), input.as_object()) else {
        return args == input;
    };
    input
        .iter()
        .all(|(key, value)| args.get(key).unwrap_or(&Value::Null) == value)
        && args
            .iter()
            .all(|(key, value)| input.get(key) == Some(value))
}

/// A permission a turn asks for, or the turn's end. Kiro names the tool that asks in the
/// request's `_meta.kiro.toolName` and the shell command in `_meta.kiro.command`.
pub fn turn(store: &Store, read: HookRead) -> Result<HookMoment, Error> {
    let document = turn_hooks::parse(&read.document)?;
    let Some(external_id) = turn_hooks::text(&document, "/sessionId") else {
        return Ok(HookMoment { moment: None });
    };
    let cwd = store.cwd(external_id).ok().flatten();
    if read.event != turn_hooks::PERMISSION {
        return Ok(turn_hooks::ended(&document, cwd));
    }
    let (id, kind, input) = turn_hooks::requested(&document);
    let tool = turn_hooks::text(&document, "/_meta/kiro/toolName").map_or(kind, str::to_owned);
    let input = match (input, turn_hooks::text(&document, "/_meta/kiro/command")) {
        (Value::Null, Some(command)) => json!({ "command": command }),
        (input, _) => input,
    };
    let kind = turn_hooks::text(&document, "/toolCall/kind");
    let call = events::call(id, &tool, input, kind);
    Ok(turn_hooks::asked(external_id, cwd, call, &document, ALWAYS))
}

/// What Steery decided, printed as Kiro parses it: at `Stop`, a paragraph as the reason the
/// agent goes on. Always exit 0: Kiro reads exit 2 as a block.
pub fn answer(answer: HookAnswer) -> Result<HookAnswered, Error> {
    let HookAnswer {
        event: name,
        answer: Answer { say, decision, .. },
        ..
    } = answer;
    let event = event(&name)
        .ok_or_else(|| Error::invalid(format!("Steery has no hook on Kiro's {name}")))?;
    if decision.is_some() {
        return Err(Error::invalid(format!(
            "Kiro reads no decision from a hook at {name}"
        )));
    }
    let stdout = match say.filter(|say| !say.trim().is_empty()) {
        None => String::new(),
        Some(_) if event.says.is_empty() => {
            return Err(Error::invalid(format!(
                "Kiro does not read a paragraph at {name}"
            )));
        }
        Some(say) => json!({"decision": "block", "reason": say}).to_string(),
    };
    Ok(HookAnswered {
        stdout,
        exit_code: 0,
        append: None,
    })
}

fn event(name: &str) -> Option<&'static Event> {
    EVENTS.iter().find(|event| event.name == name)
}

/// A field of the document as text, trimmed, where it holds any.
fn text<'a>(document: &'a Value, field: &str) -> Option<&'a str> {
    document
        .get(field)?
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// The hooks file setup writes: `~/.kiro/hooks/steery.json`.
pub fn file(root: &Path) -> PathBuf {
    root.join("hooks").join("steery.json")
}

/// The hooks file's document: each event runs `<hook> <event>`, which never blocks.
pub fn document(hook: &str) -> Value {
    json!({
        "version": "v1",
        "hooks": EVENTS.iter().map(|event| json!({
            "name": format!("steery-{}", event.name),
            "trigger": event.name,
            "action": {"type": "command", "command": format!("{hook} {} || true", event.name)},
            "timeout": SAYS_WAITS,
        })).collect::<Vec<_>>(),
    })
}
