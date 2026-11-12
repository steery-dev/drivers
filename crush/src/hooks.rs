//! Crush's hooks: which of its moments Steery reads, what a moment's document says
//! (`hook/read`), and how Steery's answer is printed back for Crush to parse
//! (`hook/answer`). Crush runs each hook in its own embedded POSIX shell, with one JSON
//! document on standard input in Claude Code's spellings (`hook_event_name`, `session_id`,
//! `cwd`, `tool_name`, `tool_input` as an object, `tool_use_id`). It reads the hook's
//! standard output as JSON: `decision` (`allow` or `deny`), `reason`, and `context`, which
//! it adds to the call's result for the model (`internal/hooks/input.go`, 0.100.0).
//!
//! Crush fires only its tool moments in `crush run`, and only for the top-level agent: a
//! sub-agent's calls (the `agent` tool, `agentic_fetch`) run unhooked, and Steery decides on
//! the call that starts the sub-agent (`internal/agent/coordinator.go`, `buildTools`).

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Answer, Decision, Hook, HookAnswer, HookAnswered, HookKind, HookMoment, HookRead, Moment, Says,
    Via,
};

use crate::{calls, mcp};

/// How long Crush gives a hook that only says a moment happened.
const SAYS_WAITS: u32 = 5;
/// How long Crush gives a hook that may put the call to a person, who answers from a phone;
/// the runtime answers within `waits - 90`. A day: Crush reads `timeout` as whole seconds.
const ASKS_WAITS: u32 = 86_400;

/// Crush's event before a tool call: the one hook that can block it.
pub(crate) const BEFORE_CALL: &str = "PreToolUse";

/// What follows `( <hook> PreToolUse )` in Crush's configuration. Crush runs a call whose
/// hook exits other than 0, 2 or 49, or outlives its `timeout`, and `crush run` approves
/// every call itself (`AutoApproveSession`). So where Steery's hook fails (the runtime gone,
/// the binary moved) this exits 2, Crush's block, with standard error as its reason. It does
/// so only in a turn Steery started: the turn mode passes `STEERY_STARTED` on to Crush under
/// its own pid (PROTOCOL.md §3.3), and the hook runs inside Crush, so `$PPID` is Crush's
/// parent. A person's own Crush, even one that inherited the variable, keeps Crush's own
/// behaviour. Crush's shell ends the whole command where a path it runs is missing, unless
/// the path runs in a subshell: there it is status 1, which this reads.
const GUARD: &str = r#" || { s=$?; case "${STEERY_STARTED:-}" in "${PPID:-0}":turn*) echo "Steery's hook failed (exit $s) in a turn Steery started, so the call is blocked" >&2; exit 2;; esac; exit $s; }"#;

/// The command setup writes for `event`, after the runtime's `hook` command.
pub(crate) fn command(hook: &str, event: &str) -> String {
    if event == BEFORE_CALL {
        format!("( {hook} {event} ){GUARD}")
    } else {
        format!("{hook} {event}")
    }
}

/// Whether `command` is the one setup writes for [`BEFORE_CALL`], for any `hook`.
pub(crate) fn guards(command: &str) -> bool {
    command.starts_with("( ") && command.ends_with(&format!(" {BEFORE_CALL} ){GUARD}"))
}

/// One of Crush's hook events Steery is hooked on, by its name in Crush's configuration.
pub(crate) struct Event {
    pub(crate) name: &'static str,
    kind: HookKind,
    says: &'static [Says],
    /// Whether the call it follows succeeded.
    ok: Option<bool>,
    /// The `timeout` setup writes on its hook, in seconds.
    pub(crate) waits: u32,
}

/// Every event Crush fires in a turn Steery starts, in the order setup writes them. Words
/// for the session reach it as a call comes back, in the `context` Crush adds to its result.
pub(crate) const EVENTS: [Event; 3] = [
    Event {
        name: BEFORE_CALL,
        kind: HookKind::BeforeCall,
        says: &[Says::Context],
        ok: None,
        waits: ASKS_WAITS,
    },
    Event {
        name: "PostToolUse",
        kind: HookKind::AfterCall,
        says: &[Says::Notice, Says::Calls],
        ok: Some(true),
        waits: SAYS_WAITS,
    },
    Event {
        name: "PostToolUseFailure",
        kind: HookKind::AfterCall,
        says: &[Says::Notice, Says::Calls],
        ok: Some(false),
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
            // Crush hands a hook no file to export a lead into.
            env: Vec::new(),
            keeps: None,
        })
        .collect()
}

/// The moment a hook's document says, or none for an event Steery is not hooked on or a
/// document that names no session or no tool.
pub fn read(home: &std::path::Path, read: HookRead) -> Result<HookMoment, Error> {
    let document = parse(&read.document)?;
    Ok(HookMoment {
        moment: moment(home, &read.event, &document),
    })
}

fn moment(home: &std::path::Path, name: &str, document: &Value) -> Option<Moment> {
    let event = event(name)?;
    let external_id = text(document, "session_id")?.to_owned();
    let cwd = text(document, "cwd")
        .filter(|cwd| cwd.starts_with('/') && !cwd.chars().any(char::is_control))
        .map(std::path::PathBuf::from);
    let servers = cwd
        .as_deref()
        .map(|cwd| mcp::names(home, cwd))
        .unwrap_or_default();
    let call = calls::call(
        text(document, "tool_use_id").unwrap_or_default(),
        text(document, "tool_name")?,
        document.get("tool_input").cloned().unwrap_or(Value::Null),
        &servers,
    );
    Some(Moment {
        event: name.to_owned(),
        external_id,
        // Crush hooks no sub-agent's calls.
        sub_agent: None,
        at: None,
        cwd,
        // Crush keeps every session in its project's store, which the driver finds by id.
        transcript: None,
        call: Some(call),
        ok: event.ok,
        outcome: None,
        continued: false,
        person_exited: false,
        prompt: None,
        context_left: None,
        model: None,
        thinking: None,
        failure: None,
        suggestions: Vec::new(),
    })
}

/// What Steery decided, printed as Crush parses it: a decision with its reason, and a
/// paragraph as `context`. Always exit 0: Crush reads exit 2 as a block whose reason is
/// standard error, and any other code as a hook that failed open. Crush hands a hook no
/// file to export a lead into: a lead is dropped.
pub fn answer(answer: HookAnswer) -> Result<HookAnswered, Error> {
    let HookAnswer {
        event: name,
        answer: Answer { say, decision, .. },
        ..
    } = answer;
    let event = event(&name)
        .ok_or_else(|| Error::invalid(format!("Steery has no hook on Crush's {name}")))?;
    let mut output = serde_json::Map::new();
    if let Some(decision) = decision {
        if event.kind != HookKind::BeforeCall {
            return Err(Error::invalid(format!(
                "Crush does not read a decision at {name}"
            )));
        }
        let (verdict, reason) = match decision {
            Decision::Allow {
                reason,
                always: None,
            } => ("allow", reason),
            Decision::Allow {
                always: Some(_), ..
            } => {
                return Err(Error::invalid("Crush keeps no permission from a hook"));
            }
            Decision::Deny { reason } => ("deny", reason),
        };
        output.insert("decision".into(), json!(verdict));
        output.insert("reason".into(), json!(reason));
    }
    if let Some(say) = say {
        if event.says.is_empty() {
            return Err(Error::invalid(format!(
                "Crush does not read a paragraph at {name}"
            )));
        }
        output.insert("context".into(), json!(say));
    }
    Ok(HookAnswered {
        stdout: if output.is_empty() {
            String::new()
        } else {
            Value::Object(output).to_string()
        },
        exit_code: 0,
        append: None,
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
