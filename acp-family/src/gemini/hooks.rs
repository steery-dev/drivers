//! Gemini CLI's hooks: which of its moments Steery reads, what a moment's document says
//! (`hook/read`), and how Steery's answer is printed back for Gemini to parse
//! (`hook/answer`). Gemini runs each hook as a shell command with one JSON document on
//! standard input, its keys in snake case: `session_id`, `transcript_path`, `cwd`,
//! `hook_event_name`, `timestamp`, then the event's own (`prompt`, `tool_name`,
//! `tool_input`, `tool_response`, `mcp_context`, `source`, `reason`, `stop_hook_active`,
//! `notification_type`). It reads the hook's standard output as JSON
//! (`core/src/hooks/types.ts`): `{decision, reason}`, or `hookSpecificOutput` with an
//! `additionalContext`.
//!
//! Three things the documents do not say. A tool's document carries no call id. A
//! sub-agent's tool calls fire `BeforeTool` and `AfterTool` with the session id and
//! transcript of the conversation that started it, so they read as that conversation's
//! calls. A resumed conversation's `SessionStart` names a transcript that Gemini never
//! writes; Gemini keeps every transcript under its root, so no moment names one.

use std::path::PathBuf;

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Answer, Decision, Hook, HookAnswer, HookAnswered, HookKind, HookMoment, HookRead, Moment, Says,
    TurnState, Via,
};

/// How long Gemini gives a hook that only says a moment happened: its own default.
const SAYS_WAITS: u32 = 60;
/// How long Gemini gives a hook that may put the call to a person, who answers from a
/// phone; the runtime answers within `waits - 90`. A day.
const ASKS_WAITS: u32 = 86_400;

/// One of Gemini's hook events Steery is hooked on, by its name in `settings.json`.
pub(crate) struct Event {
    pub(crate) name: &'static str,
    kind: HookKind,
    says: &'static [Says],
    /// The seconds setup gives its hook; Gemini's `timeout` is in milliseconds.
    pub(crate) waits: u32,
}

/// Every event Gemini fires once per moment that Steery reads, in the order setup writes
/// them. Gemini adds a hook's `additionalContext` to the conversation at `SessionStart`,
/// `BeforeAgent` and `AfterTool`, and goes on from an `AfterAgent` that a hook blocks
/// with a reason. Its model and compression hooks fire before every model request, and
/// say nothing that the transcript does not.
pub(crate) const EVENTS: [Event; 7] = [
    Event {
        name: "SessionStart",
        kind: HookKind::SessionStart,
        says: &[Says::Context, Says::Notice],
        waits: SAYS_WAITS,
    },
    Event {
        name: "BeforeAgent",
        kind: HookKind::Prompt,
        says: &[Says::Context, Says::Notice],
        waits: SAYS_WAITS,
    },
    // Gemini reads no paragraph before a call: only a block, which the model reads.
    Event {
        name: "BeforeTool",
        kind: HookKind::BeforeCall,
        says: &[],
        waits: ASKS_WAITS,
    },
    // Words for a conversation Steery started reach it deep in a tool loop as a call comes back.
    Event {
        name: "AfterTool",
        kind: HookKind::AfterCall,
        says: &[Says::Notice, Says::Calls, Says::Working],
        waits: SAYS_WAITS,
    },
    // Gemini asks its person to allow a call here, but reads no answer from the hook.
    Event {
        name: "Notification",
        kind: HookKind::Notification,
        says: &[],
        waits: SAYS_WAITS,
    },
    Event {
        name: "AfterAgent",
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
            // Gemini hands a hook no file to export a lead into.
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
            "",
            text(document, "tool_name")?,
            document.get("tool_input").cloned().unwrap_or(Value::Null),
            document
                .pointer("/mcp_context/server_name")
                .and_then(Value::as_str),
        )),
        _ => None,
    };
    Some(Moment {
        event: name.to_owned(),
        sub_agent: None,
        at: text(document, "timestamp").and_then(super::events::millis),
        cwd: text(document, "cwd")
            .filter(|cwd| cwd.starts_with('/') && !cwd.chars().any(char::is_control))
            .map(PathBuf::from),
        transcript: None,
        call,
        // Gemini puts the error of a call that failed in its `tool_response`.
        ok: (event.kind == HookKind::AfterCall).then(|| {
            document
                .pointer("/tool_response/error")
                .is_none_or(Value::is_null)
        }),
        outcome: (event.kind == HookKind::TurnEnd).then_some(TurnState::Finished),
        continued: event.kind == HookKind::TurnEnd
            && document.get("stop_hook_active").and_then(Value::as_bool) == Some(true),
        // Gemini says `exit` as its process ends (`gemini.tsx`, `AppContainer.tsx`), and
        // `clear` as `/clear` starts the next conversation.
        person_exited: event.kind == HookKind::SessionEnd
            && text(document, "reason") == Some("exit"),
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

/// What Steery decided, printed as Gemini parses it on this event: a paragraph as
/// `additionalContext`, or at `AfterAgent` as the reason the agent goes on; a deny as
/// `BeforeTool`'s `decision`. Always exit 0: Gemini reads exit 2 and up as a block whose
/// reason is the output, and exit 1 as a warning. Gemini hands a hook no file to export a
/// lead into: a lead is dropped.
pub fn answer(answer: HookAnswer) -> Result<HookAnswered, Error> {
    let HookAnswer {
        event: name,
        answer: Answer { say, decision, .. },
        ..
    } = answer;
    let event = event(&name)
        .ok_or_else(|| Error::invalid(format!("Steery has no hook on Gemini's {name}")))?;
    let output = match (event.kind, decision) {
        (_, None) => say.map(|say| context(event, say)).transpose()?,
        (HookKind::BeforeCall, Some(decision)) => before_call(decision),
        (_, Some(_)) => {
            return Err(Error::invalid(format!(
                "Gemini does not read a decision at {name}"
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
            "Gemini does not read a paragraph at {}",
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

/// `BeforeTool`'s decision. A deny blocks the call, its reason being what the model reads.
/// An allow prints nothing: Gemini asks its own policy whatever a hook allowed
/// (`scheduler.ts`, a hook can only add an ask), so the call goes as Gemini would let it,
/// and Gemini keeps no permission from a hook.
fn before_call(decision: Decision) -> Option<Value> {
    match decision {
        Decision::Allow { .. } => None,
        Decision::Deny { reason } => Some(json!({"decision": "deny", "reason": reason})),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Gemini CLI 0.67.0's own documents, one per moment, from real runs: a conversation
    /// that ran a sub-agent, a person asked to allow a shell command, and a resume.
    const DOCUMENTS: &str = include_str!("../../tests/gemini/hooks/documents.jsonl");

    fn moments() -> Vec<(String, Option<Moment>)> {
        DOCUMENTS
            .lines()
            .map(|line| {
                let line: Value = serde_json::from_str(line).unwrap();
                let event = line["event"].as_str().unwrap().to_owned();
                let read = HookRead {
                    event: event.clone(),
                    document: line["stdin"].to_string(),
                    env: Default::default(),
                };
                (event, super::read(read).unwrap().moment)
            })
            .collect()
    }

    #[test]
    fn each_document_is_the_moment_it_says() {
        let parent = "488d4dbe-87e4-4026-aa02-5905bc8790fb";
        let said: Vec<_> = moments()
            .into_iter()
            .map(|(event, moment)| {
                let moment = moment.unwrap_or_else(|| panic!("{event} is a moment"));
                assert_eq!(moment.event, event);
                assert!(moment.at.is_some(), "{event} says when");
                assert!(
                    moment
                        .cwd
                        .as_deref()
                        .is_some_and(|cwd| cwd.ends_with("work"))
                );
                assert_eq!(
                    moment.transcript, None,
                    "no transcript, not even resume's wrong one"
                );
                assert!(moment.suggestions.is_empty() && moment.sub_agent.is_none());
                moment
            })
            .collect();
        let shape: Vec<_> = said
            .iter()
            .map(|moment| {
                (
                    moment.event.as_str(),
                    moment.call.as_ref().map(|call| call.id.as_str()),
                    moment.ok,
                    moment.outcome,
                    moment.continued,
                    moment.person_exited,
                    moment.prompt.as_deref(),
                )
            })
            .collect();
        let prompt = "use a sub-agent to list files";
        assert_eq!(
            shape,
            [
                ("SessionStart", None, None, None, false, false, None),
                ("BeforeAgent", None, None, None, false, false, Some(prompt)),
                ("BeforeTool", Some(""), None, None, false, false, None),
                ("BeforeTool", Some(""), None, None, false, false, None),
                ("AfterTool", Some(""), Some(true), None, false, false, None),
                ("BeforeTool", Some(""), None, None, false, false, None),
                ("AfterTool", Some(""), Some(true), None, false, false, None),
                ("AfterTool", Some(""), Some(true), None, false, false, None),
                (
                    "AfterAgent",
                    None,
                    None,
                    Some(TurnState::Finished),
                    false,
                    false,
                    None
                ),
                ("SessionEnd", None, None, None, false, true, None),
                ("Notification", None, None, None, false, false, None),
                ("SessionStart", None, None, None, false, false, None),
            ]
        );
        // A sub-agent's calls carry the session of the conversation that started it.
        let tools: Vec<_> = said
            .iter()
            .filter_map(|moment| Some((moment.call.as_ref()?.tool.as_str(), &moment.external_id)))
            .collect();
        assert_eq!(
            tools.iter().map(|(tool, _)| *tool).collect::<Vec<_>>(),
            [
                "invoke_agent",
                "list_directory",
                "list_directory",
                "complete_task",
                "complete_task",
                "invoke_agent"
            ]
        );
        assert!(tools.iter().all(|(_, id)| *id == parent));
        assert_eq!(said[1].call, None);
        assert_eq!(
            said[2].call.as_ref().unwrap().input,
            json!({"agent_name": "generalist", "prompt": "List the files in the current folder and report them."})
        );
    }

    #[test]
    fn other_documents_are_no_moment() {
        let read = |event: &str, document: &str| {
            super::read(HookRead {
                event: event.into(),
                document: document.into(),
                env: Default::default(),
            })
        };
        let pre_compress = r#"{"session_id":"s","hook_event_name":"PreCompress","trigger":"auto"}"#;
        assert_eq!(read("PreCompress", pre_compress).unwrap().moment, None);
        assert_eq!(
            read("BeforeAgent", r#"{"prompt":"hi"}"#).unwrap().moment,
            None
        );
        assert!(read("BeforeAgent", "not json").is_err());
        let failed = r#"{"session_id":"s","tool_name":"read_file","tool_input":{},
            "tool_response":{"llmContent":"","error":{"message":"no such file"}}}"#;
        assert_eq!(
            read("AfterTool", failed).unwrap().moment.unwrap().ok,
            Some(false)
        );
        let again =
            r#"{"session_id":"s","prompt":"p","prompt_response":"r","stop_hook_active":true}"#;
        assert!(read("AfterAgent", again).unwrap().moment.unwrap().continued);
        let cleared = r#"{"session_id":"s","reason":"clear"}"#;
        assert!(
            !read("SessionEnd", cleared)
                .unwrap()
                .moment
                .unwrap()
                .person_exited
        );
    }

    #[test]
    fn an_answer_is_printed_as_gemini_parses_it() {
        let said = |event: &str, say: Option<&str>, decision: Option<Decision>| {
            super::answer(HookAnswer {
                event: event.into(),
                document: "{}".into(),
                env: Default::default(),
                answer: Answer {
                    say: say.map(str::to_owned),
                    decision,
                    lead: Some("ignored".into()),
                },
            })
        };
        let stdout = |answered: Result<HookAnswered, Error>| {
            let answered = answered.unwrap();
            assert_eq!((answered.exit_code, answered.append), (0, None));
            answered.stdout
        };
        let allow = Decision::Allow {
            reason: "yes".into(),
            always: None,
        };
        assert_eq!(stdout(said("BeforeTool", None, Some(allow))), "");
        let deny = Decision::Deny {
            reason: "Use the tests folder.".into(),
        };
        assert_eq!(
            stdout(said("BeforeTool", None, Some(deny))),
            r#"{"decision":"deny","reason":"Use the tests folder."}"#
        );
        assert_eq!(
            stdout(said("AfterAgent", Some("Words wait."), None)),
            r#"{"decision":"block","reason":"Words wait."}"#
        );
        for event in ["SessionStart", "BeforeAgent", "AfterTool"] {
            assert_eq!(
                serde_json::from_str::<Value>(&stdout(said(event, Some("Words wait."), None)))
                    .unwrap(),
                json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": "Words wait."}})
            );
        }
        assert_eq!(stdout(said("SessionEnd", None, None)), "");
        assert!(said("BeforeTool", Some("hi"), None).is_err());
        assert!(said("Notification", Some("hi"), None).is_err());
        assert!(said("PreCompress", None, None).is_err());
        let deny = Decision::Deny {
            reason: "no".into(),
        };
        assert!(said("AfterTool", None, Some(deny)).is_err());
    }
}
