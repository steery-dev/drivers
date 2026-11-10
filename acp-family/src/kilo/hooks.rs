//! Steery's hooks on Kilo, in a turn this driver runs. Kilo runs no hook command itself. The
//! turn ([`super::turn`]) is Kilo's ACP client there, and runs `<hook> <event>` with a
//! document on standard input at two moments:
//! - `session/request_permission`: Kilo asks its client to allow a call. The document is the
//!   request's `params`, and the hook's standard output is Kilo's answer, an ACP
//!   `RequestPermissionResponse` (`{outcome}`). Kilo asks only where its rules say `ask`, so
//!   this is the moment Kilo puts a call to its person (`permission`), never every call.
//! - `session/prompt`: Kilo answered the turn's last prompt, and runs nothing more of the
//!   session (`turn_end`). The document is `{sessionId, stopReason, continued}`, and the
//!   hook's standard output is words that the turn prompts the session with, to go on.

use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Always, Answer, Decision, Hook, HookAnswer, HookAnswered, HookKind, HookMoment, HookRead,
    Moment, Says, Suggestion, TurnState, Via,
};

use super::store::Store;
use super::{calls, config};

/// The permission event, by the ACP method Kilo asks with.
pub const PERMISSION: &str = "session/request_permission";

/// The end of a turn, by the ACP method whose answer ends it.
pub const ENDED: &str = "session/prompt";

/// How long the turn gives the hook at its end: the runtime answers it at once.
pub const ENDED_WAITS: Duration = Duration::from_secs(5);

/// How long the turn gives the hook before it rejects the call: a person answers from a
/// phone, and the runtime answers within `waits - 90`. Kilo itself waits without bound.
pub const ASKS_WAITS: u32 = 86_400;

/// The hooks `initialize` declares. Kilo keeps an `allow_always` as rules in the person's
/// global configuration, which are its own suggestions; it keeps no "this tool" or
/// "everything" that an ACP client can ask for.
pub fn table(protocol: u32) -> Vec<Hook> {
    vec![
        Hook {
            event: PERMISSION.to_owned(),
            kind: HookKind::Permission,
            via: Via::Command,
            says: Vec::new(),
            waits: Some(ASKS_WAITS),
            env: Vec::new(),
            keeps: (protocol >= steery_sdk::KEEPS).then(Vec::new),
        },
        Hook {
            event: ENDED.to_owned(),
            kind: HookKind::TurnEnd,
            via: Via::Command,
            says: vec![Says::Context],
            waits: None,
            env: Vec::new(),
            keeps: None,
        },
    ]
}

/// The moment of the hook's `event`, or none where it is no moment Steery carries.
pub fn read(store: &Store, read: HookRead) -> Result<HookMoment, Error> {
    match read.event.as_str() {
        PERMISSION => asked(store, &parse(&read.document)?),
        ENDED => Ok(ended(store, &parse(&read.document)?)),
        _ => Ok(HookMoment { moment: None }),
    }
}

/// The end of a turn: how Kilo's answer to its last prompt says it ended.
fn ended(store: &Store, document: &Value) -> HookMoment {
    let Some(external_id) = text(document, "/sessionId") else {
        return HookMoment { moment: None };
    };
    let outcome = match text(document, "/stopReason") {
        Some("end_turn") => TurnState::Finished,
        Some("cancelled") => TurnState::Interrupted,
        _ => TurnState::Failed,
    };
    let cwd = store.directory(external_id).ok().flatten();
    HookMoment {
        moment: Some(Moment {
            event: ENDED.to_owned(),
            outcome: Some(outcome),
            continued: document.get("continued").and_then(Value::as_bool) == Some(true),
            ..moment(external_id, cwd)
        }),
    }
}

/// The moment of a permission Kilo asks for. The call is the one Kilo's database keeps
/// under the request's `toolCallId`, which is the call's `callID`. A request with no tool
/// behind it (Kilo then sends the permission's own id) is named by its ACP `kind`.
fn asked(store: &Store, document: &Value) -> Result<HookMoment, Error> {
    let Some(external_id) = text(document, "/sessionId") else {
        return Ok(HookMoment { moment: None });
    };
    let id = text(document, "/toolCall/toolCallId").unwrap_or_default();
    let cwd = store.directory(external_id).ok().flatten();
    let (tool, input) = match store.tool(external_id, id)? {
        Some(kept) => kept,
        None => (
            text(document, "/toolCall/kind")
                .unwrap_or("other")
                .to_owned(),
            document
                .pointer("/toolCall/rawInput")
                .cloned()
                .unwrap_or(Value::Null),
        ),
    };
    let servers = config::servers(store.config(), cwd.as_deref());
    let call = calls::call(id, &tool, input, &servers);
    Ok(HookMoment {
        moment: Some(Moment {
            call: Some(call),
            suggestions: options(document, "allow_always")
                .map(|(_, name)| Suggestion {
                    label: name.to_owned(),
                    about: "calls like it, everywhere: Kilo saves the rule in your global \
                            configuration"
                        .to_owned(),
                })
                .collect(),
            ..moment(external_id, cwd)
        }),
    })
}

/// A permission's moment with no call yet: the fields both moments fill alike.
fn moment(external_id: &str, cwd: Option<std::path::PathBuf>) -> Moment {
    Moment {
        event: PERMISSION.to_owned(),
        external_id: external_id.to_owned(),
        sub_agent: None,
        at: None,
        cwd: cwd.filter(|cwd| cwd.is_absolute()),
        transcript: None,
        call: None,
        ok: None,
        outcome: None,
        continued: false,
        person_exited: false,
        prompt: None,
        context_left: None,
        model: None,
        thinking: None,
        failure: None,
        suggestions: Vec::new(),
    }
}

/// Steery's answer as the hook's standard output. At a permission, it is the option Kilo
/// reads; no decision prints nothing, and the turn then rejects the call, as no person is
/// at Kilo to ask. Kilo reads no reason with an option, so a refusal's reason does not
/// reach the agent: Kilo tells it that the person rejected the call. At the end of a turn,
/// it is Steery's paragraph, which the turn prompts the session with.
pub fn answer(answer: HookAnswer) -> Result<HookAnswered, Error> {
    let printed = |stdout: String| HookAnswered {
        stdout,
        exit_code: 0,
        append: None,
    };
    let Answer { say, decision, .. } = answer.answer;
    match answer.event.as_str() {
        PERMISSION => {}
        ENDED if decision.is_some() => {
            return Err(Error::invalid("a turn's end has no call to decide"));
        }
        ENDED => return Ok(printed(say.unwrap_or_default())),
        event => {
            return Err(Error::invalid(format!(
                "Steery has no hook on Kilo's {event}"
            )));
        }
    }
    if say.is_some() {
        return Err(Error::invalid("Kilo reads no paragraph at a permission"));
    }
    let Some(decision) = decision else {
        return Ok(printed(String::new()));
    };
    let document = parse(&answer.document)?;
    let (kind, nth) = match decision {
        Decision::Allow { always: None, .. } => ("allow_once", 0),
        Decision::Allow {
            always: Some(Always::Suggestion(n)),
            ..
        } => ("allow_always", n as usize),
        Decision::Allow {
            always: Some(_), ..
        } => {
            return Err(Error::invalid(
                "Kilo keeps only its own suggestions from a permission",
            ));
        }
        Decision::Deny { .. } => ("reject_once", 0),
    };
    let (option, _) = options(&document, kind).nth(nth).ok_or_else(|| {
        Error::invalid(format!("Kilo offered no {kind} option number {nth} here"))
    })?;
    Ok(printed(
        json!({"outcome": {"outcome": "selected", "optionId": option}}).to_string(),
    ))
}

/// The `(optionId, name)` of each option of one ACP `kind`, in Kilo's order.
fn options<'a>(document: &'a Value, kind: &'a str) -> impl Iterator<Item = (&'a str, &'a str)> {
    document
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(move |option| option.get("kind").and_then(Value::as_str) == Some(kind))
        .filter_map(|option| {
            let id = option.get("optionId")?.as_str()?;
            Some((id, option.get("name").and_then(Value::as_str).unwrap_or(id)))
        })
}

fn parse(document: &str) -> Result<Value, Error> {
    serde_json::from_str(document)
        .map_err(|error| Error::invalid(format!("the hook's document is not JSON: {error}")))
}

fn text<'a>(document: &'a Value, pointer: &str) -> Option<&'a str> {
    document
        .pointer(pointer)?
        .as_str()
        .filter(|text| !text.is_empty())
}
