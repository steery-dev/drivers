//! Steery's one hook on Kilo: a permission that Kilo asks its ACP client for, in a turn this
//! driver runs. Kilo runs no hook command itself. The turn ([`super::turn`]) is Kilo's
//! client there: at Kilo's `session/request_permission` it runs `<hook>
//! session/request_permission` with the request's `params` on standard input, and answers
//! Kilo with the hook's standard output, an ACP `RequestPermissionResponse` (`{outcome}`).
//! Kilo asks only where its rules say `ask`, so this is the moment Kilo puts a call to its
//! person (`permission`), never every call.

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Always, Answer, Decision, Hook, HookAnswer, HookAnswered, HookKind, HookMoment, HookRead,
    Moment, Suggestion, Via,
};

use super::store::Store;
use super::{calls, config};

/// The event, by the ACP method Kilo asks with.
pub const EVENT: &str = "session/request_permission";

/// How long the turn gives the hook before it rejects the call: a person answers from a
/// phone, and the runtime answers within `waits - 90`. Kilo itself waits without bound.
pub const ASKS_WAITS: u32 = 86_400;

/// The hooks `initialize` declares. Kilo keeps an `allow_always` as rules in the person's
/// global configuration, which are its own suggestions; it keeps no "this tool" or
/// "everything" that an ACP client can ask for.
pub fn table(protocol: u32) -> Vec<Hook> {
    vec![Hook {
        event: EVENT.to_owned(),
        kind: HookKind::Permission,
        via: Via::Command,
        says: Vec::new(),
        waits: Some(ASKS_WAITS),
        env: Vec::new(),
        keeps: (protocol >= steery_sdk::KEEPS).then(Vec::new),
    }]
}

/// The moment of a permission Kilo asks for. The call is the one Kilo's database keeps
/// under the request's `toolCallId`, which is the call's `callID`. A request with no tool
/// behind it (Kilo then sends the permission's own id) is named by its ACP `kind`.
pub fn read(store: &Store, read: HookRead) -> Result<HookMoment, Error> {
    if read.event != EVENT {
        return Ok(HookMoment { moment: None });
    }
    let document = parse(&read.document)?;
    let Some(external_id) = text(&document, "/sessionId") else {
        return Ok(HookMoment { moment: None });
    };
    let id = text(&document, "/toolCall/toolCallId").unwrap_or_default();
    let cwd = store.directory(external_id).ok().flatten();
    let (tool, input) = match store.tool(external_id, id)? {
        Some(kept) => kept,
        None => (
            text(&document, "/toolCall/kind")
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
            event: EVENT.to_owned(),
            external_id: external_id.to_owned(),
            sub_agent: None,
            at: None,
            cwd: cwd.filter(|cwd| cwd.is_absolute()),
            transcript: None,
            call: Some(call),
            ok: None,
            outcome: None,
            continued: false,
            person_exited: false,
            prompt: None,
            context_left: None,
            model: None,
            thinking: None,
            failure: None,
            suggestions: options(&document, "allow_always")
                .map(|(_, name)| Suggestion {
                    label: name.to_owned(),
                    about: "calls like it, everywhere: Kilo saves the rule in your global \
                            configuration"
                        .to_owned(),
                })
                .collect(),
        }),
    })
}

/// Steery's decision as the ACP answer Kilo reads: the option it picks. No decision prints
/// nothing, and the turn then rejects the call, as no person is at Kilo to ask. Kilo reads
/// no reason with an option, so a refusal's reason does not reach the agent: Kilo tells it
/// that the person rejected the call.
pub fn answer(answer: HookAnswer) -> Result<HookAnswered, Error> {
    if answer.event != EVENT {
        return Err(Error::invalid(format!(
            "Steery has no hook on Kilo's {}",
            answer.event
        )));
    }
    let Answer { say, decision, .. } = answer.answer;
    if say.is_some() {
        return Err(Error::invalid("Kilo reads no paragraph at a permission"));
    }
    let Some(decision) = decision else {
        return Ok(HookAnswered {
            stdout: String::new(),
            exit_code: 0,
            append: None,
        });
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
    Ok(HookAnswered {
        stdout: json!({"outcome": {"outcome": "selected", "optionId": option}}).to_string(),
        exit_code: 0,
        append: None,
    })
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
