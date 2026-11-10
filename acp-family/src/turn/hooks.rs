//! Steery's hooks on an ACP harness (Kilo, OpenCode), in a turn its driver runs. The
//! harness runs no hook command itself. The turn ([`super`]) is the harness's ACP client
//! there, and runs `<hook> <event>` with a document on standard input at two moments:
//! - `session/request_permission`: the harness asks its client to allow a call. The document
//!   is the request's `params`, and the hook's standard output is the harness's answer, an
//!   ACP `RequestPermissionResponse` (`{outcome}`). The harness asks only where its rules say
//!   `ask`, so this is the moment it puts a call to its person (`permission`), never every
//!   call.
//! - `session/prompt`: the harness answered the turn's last prompt, and runs nothing more of
//!   the session (`turn_end`). The document is `{sessionId, stopReason, continued}`, and the
//!   hook's standard output is words that the turn prompts the session with, to go on.
//!
//! Each harness reads the moment's call from its own store; the rest is here.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Always, Answer, Call, Decision, Hook, HookAnswer, HookAnswered, HookKind, HookMoment, Moment,
    Says, Suggestion, TurnState, Via,
};

/// The permission event, by the ACP method the harness asks with.
pub const PERMISSION: &str = "session/request_permission";

/// The end of a turn, by the ACP method whose answer ends it.
pub const ENDED: &str = "session/prompt";

/// How long the turn gives the hook at its end: the runtime answers it at once.
pub const ENDED_WAITS: Duration = Duration::from_secs(5);

/// How long the turn gives the hook before it rejects the call: a person answers from a
/// phone, and the runtime answers within `waits - 90`. The harness itself waits without
/// bound.
pub const ASKS_WAITS: u32 = 86_400;

/// The hooks `initialize` declares. The harness keeps an `allow_always` as rules of its own,
/// which are its own suggestions; it keeps no "this tool" or "everything" that an ACP client
/// can ask for.
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

/// The end of a turn: how the harness's answer to its last prompt says it ended; `cwd` is
/// the conversation's folder, as the harness keeps it.
pub fn ended(document: &Value, cwd: Option<PathBuf>) -> HookMoment {
    let Some(external_id) = text(document, "/sessionId") else {
        return HookMoment { moment: None };
    };
    let outcome = match text(document, "/stopReason") {
        Some("end_turn") => TurnState::Finished,
        Some("cancelled") => TurnState::Interrupted,
        _ => TurnState::Failed,
    };
    HookMoment {
        moment: Some(Moment {
            event: ENDED.to_owned(),
            outcome: Some(outcome),
            continued: document.get("continued").and_then(Value::as_bool) == Some(true),
            ..moment(external_id, cwd)
        }),
    }
}

/// The moment of a permission the harness asks for, of the `call` its store keeps; `always`
/// says what the harness's `allow_always` options keep.
pub fn asked(
    external_id: &str,
    cwd: Option<PathBuf>,
    call: Call,
    document: &Value,
    always: &str,
) -> HookMoment {
    HookMoment {
        moment: Some(Moment {
            call: Some(call),
            suggestions: options(document, "allow_always")
                .map(|(_, name)| Suggestion {
                    label: name.to_owned(),
                    about: always.to_owned(),
                })
                .collect(),
            ..moment(external_id, cwd)
        }),
    }
}

/// The call of a permission request where the harness's store keeps none: the request's
/// `toolCallId`, its ACP `kind` as the tool, and its `rawInput`.
pub fn requested(document: &Value) -> (&str, String, Value) {
    (
        text(document, "/toolCall/toolCallId").unwrap_or_default(),
        text(document, "/toolCall/kind")
            .unwrap_or("other")
            .to_owned(),
        document
            .pointer("/toolCall/rawInput")
            .cloned()
            .unwrap_or(Value::Null),
    )
}

/// A permission's moment with no call yet: the fields both moments fill alike.
fn moment(external_id: &str, cwd: Option<PathBuf>) -> Moment {
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

/// Steery's answer as the hook's standard output, for the harness named `label`. At a
/// permission, it is the option the harness reads; no decision prints nothing, and the turn
/// then rejects the call, as no person is at the harness to ask. The harness reads no reason
/// with an option, so a refusal's reason does not reach the agent: the harness tells it that
/// the person rejected the call. At the end of a turn, it is Steery's paragraph, which the
/// turn prompts the session with.
pub fn answer(label: &str, answer: HookAnswer) -> Result<HookAnswered, Error> {
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
                "Steery has no hook on {label}'s {event}"
            )));
        }
    }
    if say.is_some() {
        return Err(Error::invalid(format!(
            "{label} reads no paragraph at a permission"
        )));
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
            return Err(Error::invalid(format!(
                "{label} keeps only its own suggestions from a permission"
            )));
        }
        Decision::Deny { .. } => ("reject_once", 0),
    };
    let (option, _) = options(&document, kind).nth(nth).ok_or_else(|| {
        Error::invalid(format!(
            "{label} offered no {kind} option number {nth} here"
        ))
    })?;
    Ok(printed(
        json!({"outcome": {"outcome": "selected", "optionId": option}}).to_string(),
    ))
}

/// The `(optionId, name)` of each option of one ACP `kind`, in the harness's order.
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

/// The hook's document.
pub fn parse(document: &str) -> Result<Value, Error> {
    serde_json::from_str(document)
        .map_err(|error| Error::invalid(format!("the hook's document is not JSON: {error}")))
}

/// A non-empty text of the document.
pub fn text<'a>(document: &'a Value, pointer: &str) -> Option<&'a str> {
    document
        .pointer(pointer)?
        .as_str()
        .filter(|text| !text.is_empty())
}
