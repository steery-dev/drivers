//! Antigravity's hooks: which of its moments Steery reads, what a moment's document says
//! (`hook/read`), and how Steery's answer is printed back for Antigravity to parse
//! (`hook/answer`). Antigravity runs each hook as a shell command with one JSON document on
//! standard input, its keys in camel case: `conversationId`, `workspacePaths`, `cwd`,
//! `transcriptPath`, `artifactDirectoryPath`, `modelName`, `lastUserInput`, then the
//! event's own (`toolCall`, `stepIdx`, `error`, `invocationNum`, `terminationReason`,
//! `fullyIdle`). It reads the hook's standard output as JSON (the hooks page of its docs).
//!
//! Four things the documents do not say. A document carries no time. A call has no id of
//! its own: `stepIdx` is the step that will hold its result, the id the transcript's reader
//! gives it. A sub-agent's hooks fire with its own conversation's id, and at its first
//! moment the database does not yet name its lead; the lead's
//! `brain/<lead>/.system_generated/subagents/<its id>.json` does, so the moment is read as
//! the lead's. And a lead that started a sub-agent fires `Stop` with `fullyIdle` false
//! when its own loop pauses; the sub-agent's result wakes it, and it stops again idle.

use std::fs::File;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Answer, Decision, Hook, HookAnswer, HookAnswered, HookKind, HookMoment, HookRead, Moment, Says,
    TurnState, Via,
};

use super::store::{BRAIN, DEPTH, TRANSCRIPT};

/// How long Antigravity gives a hook that only says a moment happened: its own default.
const SAYS_WAITS: u32 = 30;
/// How long Antigravity gives a hook that may put the call to a person, who answers from a
/// phone; the runtime answers within `waits - 90`. A day.
const ASKS_WAITS: u32 = 86_400;

/// One of Antigravity's hook events Steery is hooked on, by its name in `hooks.json`.
pub(crate) struct Event {
    pub(crate) name: &'static str,
    kind: HookKind,
    says: &'static [Says],
    /// The seconds setup gives its hook, as `hooks.json`'s `timeout`.
    pub(crate) waits: u32,
    /// Whether `hooks.json` lists its hooks under a `matcher`, as it does a tool's.
    pub(crate) matched: bool,
}

/// Every event Antigravity fires once per moment that Steery reads, in the order setup
/// writes them. Antigravity puts a hook's `injectSteps` in the conversation at
/// `SessionStart` and `PreInvocation`, and goes on from a `Stop` that a hook answers
/// `continue`. `PreInvocation` fires before every model call; the first on a prompt is the
/// prompt's moment. `PostInvocation` says nothing the transcript does not.
pub(crate) const EVENTS: [Event; 5] = [
    Event {
        name: "SessionStart",
        kind: HookKind::SessionStart,
        says: &[Says::Context, Says::Notice],
        waits: SAYS_WAITS,
        matched: false,
    },
    Event {
        name: "PreInvocation",
        kind: HookKind::Prompt,
        says: &[Says::Context, Says::Notice],
        waits: SAYS_WAITS,
        matched: false,
    },
    // Antigravity reads no paragraph before a call: only a decision.
    Event {
        name: "PreToolUse",
        kind: HookKind::BeforeCall,
        says: &[],
        waits: ASKS_WAITS,
        matched: true,
    },
    // Antigravity reads nothing back after a call.
    Event {
        name: "PostToolUse",
        kind: HookKind::AfterCall,
        says: &[],
        waits: SAYS_WAITS,
        matched: true,
    },
    Event {
        name: "Stop",
        kind: HookKind::TurnEnd,
        says: &[Says::Context],
        waits: SAYS_WAITS,
        matched: false,
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
            // Antigravity hands a hook no file to export a lead into.
            env: Vec::new(),
            // It keeps no permission from a hook.
            keeps: (event.kind == HookKind::BeforeCall).then(Vec::new),
        })
        .collect()
}

/// The moment a hook's document says, or none for an event Steery is not hooked on, a
/// document that names no conversation, a model call within a turn, or a lead that paused
/// for its sub-agents.
pub fn read(root: &Path, read: HookRead) -> Result<HookMoment, Error> {
    let document = parse(&read.document)?;
    Ok(HookMoment {
        moment: moment(root, &read.event, &document),
    })
}

fn moment(root: &Path, name: &str, document: &Value) -> Option<Moment> {
    let event = event(name)?;
    let id = text(document, "conversationId")?;
    match event.kind {
        HookKind::Prompt if !prompted(root, id, document) => return None,
        HookKind::TurnEnd if document["fullyIdle"].as_bool() == Some(false) => return None,
        _ => {}
    }
    let call = match event.kind {
        HookKind::BeforeCall | HookKind::AfterCall => Some(super::events::call(
            &document["stepIdx"].as_u64()?.to_string(),
            document.pointer("/toolCall/name")?.as_str()?,
            document
                .pointer("/toolCall/args")
                .cloned()
                .unwrap_or(Value::Null),
        )),
        _ => None,
    };
    let error = text(document, "error");
    let outcome =
        (event.kind == HookKind::TurnEnd).then(|| match text(document, "terminationReason") {
            Some("ERROR") => TurnState::Failed,
            Some("USER_CANCELED") => TurnState::Interrupted,
            _ => TurnState::Finished,
        });
    let (external_id, sub_agent) = match lead(root, id) {
        Some(lead) => (lead, Some(id.to_owned())),
        None => (id.to_owned(), None),
    };
    Some(Moment {
        event: name.to_owned(),
        external_id,
        sub_agent,
        at: None,
        cwd: text(document, "cwd")
            .or_else(|| document.pointer("/workspacePaths/0")?.as_str())
            .filter(|cwd| cwd.starts_with('/') && !cwd.chars().any(char::is_control))
            .map(PathBuf::from),
        transcript: None,
        call,
        ok: (event.kind == HookKind::AfterCall).then_some(error.is_none()),
        failure: (outcome == Some(TurnState::Failed))
            .then(|| error.map(|error| super::events::failure(error.to_owned())))
            .flatten(),
        outcome,
        continued: event.kind == HookKind::TurnEnd
            && document["executionNum"].as_u64().is_some_and(|n| n > 0),
        person_exited: false,
        prompt: (event.kind == HookKind::Prompt)
            .then(|| text(document, "lastUserInput"))
            .flatten()
            .map(str::to_owned),
        context_left: None,
        model: text(document, "modelName").map(str::to_owned),
        thinking: None,
        suggestions: Vec::new(),
    })
}

/// Whether a `PreInvocation` is a turn's first model call on a prompt: its `invocationNum`
/// is 0, and the step before it is a `USER_INPUT`. A lead that its sub-agent's result
/// wakes starts again at 0 after its own answer, and a sub-agent's first call has no step
/// before it: its task is a message from its lead.
fn prompted(root: &Path, id: &str, document: &Value) -> bool {
    if document["invocationNum"].as_u64() != Some(0) {
        return false;
    }
    let Some(before) = document["initialNumSteps"]
        .as_u64()
        .and_then(|steps| usize::try_from(steps.checked_sub(1)?).ok())
    else {
        return false;
    };
    let Ok(file) = File::open(root.join(BRAIN).join(id).join(TRANSCRIPT)) else {
        return false;
    };
    BufReader::new(file)
        .lines()
        .nth(before)
        .and_then(Result::ok)
        .and_then(|line| serde_json::from_str::<Value>(&line).ok())
        .is_some_and(|step| step["type"] == "USER_INPUT")
}

/// The conversation at the top of the sub-agent `id` runs under, where it is one: the
/// lead whose `subagents/` names it, and in turn that lead's, to [`DEPTH`].
fn lead(root: &Path, id: &str) -> Option<String> {
    let brain = root.join(BRAIN);
    let leads: Vec<String> = std::fs::read_dir(&brain)
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    let mut found = None;
    let mut child = id.to_owned();
    for _ in 0..DEPTH {
        let file = format!("{child}.json");
        let Some(lead) = leads.iter().find(|lead| {
            brain
                .join(lead)
                .join(".system_generated/subagents")
                .join(&file)
                .is_file()
        }) else {
            break;
        };
        child = lead.clone();
        found = Some(lead.clone());
    }
    found
}

/// What Steery decided, printed as Antigravity parses it on this event: a paragraph as a
/// system message it injects, or at `Stop` as the reason the loop goes on; a decision as
/// `PreToolUse`'s. Always exit 0. Antigravity hands a hook no file to export a lead into:
/// a lead is dropped.
pub fn answer(answer: HookAnswer) -> Result<HookAnswered, Error> {
    let HookAnswer {
        event: name,
        answer: Answer { say, decision, .. },
        ..
    } = answer;
    let event = event(&name)
        .ok_or_else(|| Error::invalid(format!("Steery has no hook on Antigravity's {name}")))?;
    let output = match (event.kind, decision) {
        (_, None) => say.map(|say| context(event, say)).transpose()?,
        (HookKind::BeforeCall, Some(decision)) => Some(before_call(decision)?),
        (_, Some(_)) => {
            return Err(Error::invalid(format!(
                "Antigravity does not read a decision at {name}"
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
            "Antigravity does not read a paragraph at {}",
            event.name
        )));
    }
    Ok(match event.kind {
        HookKind::TurnEnd => json!({"decision": "continue", "reason": say}),
        _ => json!({"injectSteps": [{"systemMessage": {"systemMessage": say}}]}),
    })
}

/// `PreToolUse`'s decision. An allow is `auto_approve`, so the call runs without Antigravity
/// asking (its `allow` only passes the call on to its own policy, which in a headless run
/// denies a command); a deny blocks the call, its reason being what the model reads.
fn before_call(decision: Decision) -> Result<Value, Error> {
    Ok(match decision {
        Decision::Allow {
            reason,
            always: None,
        } => json!({"decision": "auto_approve", "reason": reason}),
        Decision::Allow {
            always: Some(_), ..
        } => {
            return Err(Error::invalid(
                "Antigravity does not keep a permission from PreToolUse",
            ));
        }
        Decision::Deny { reason } => json!({"decision": "deny", "reason": reason}),
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

#[cfg(test)]
mod tests {
    use super::*;
    use steery_sdk::wire::harness::FailureKind;

    /// Antigravity CLI 1.5.5's own documents, recorded offline beside the conversations
    /// they name: a person's lead that started a sub-agent, then a run whose model service
    /// refused its key.
    fn documents(scene: &str) -> Vec<Moment> {
        let scene = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/antigravity")
            .join(scene);
        let root = scene.join(".gemini/antigravity-cli");
        std::fs::read_to_string(scene.join("hooks.jsonl"))
            .unwrap()
            .lines()
            .filter_map(|line| {
                let line: Value = serde_json::from_str(line).unwrap();
                read(
                    &root,
                    line["event"].as_str().unwrap(),
                    &line["stdin"].to_string(),
                )
            })
            .collect()
    }

    fn read(root: &Path, event: &str, document: &str) -> Option<Moment> {
        super::read(
            root,
            HookRead {
                event: event.into(),
                document: document.into(),
                env: Default::default(),
            },
        )
        .unwrap()
        .moment
    }

    #[test]
    fn each_document_is_the_moment_it_says() {
        let lead = "1a328775-8ec5-4ea3-9921-6d4c0d910ada";
        let child = "f4c750f6-d77f-4716-a9c9-90f8d6af6d50";
        let mut said = documents("tuisub2");
        let typed = said.len();
        said.extend(documents("e401"));
        let shape: Vec<_> = said
            .iter()
            .map(|moment| {
                (
                    moment.event.as_str(),
                    moment.sub_agent.is_some(),
                    moment
                        .call
                        .as_ref()
                        .map(|call| (call.id.as_str(), call.tool.as_str())),
                    moment.ok,
                    moment.outcome,
                    moment.prompt.as_deref(),
                )
            })
            .collect();
        let finished = Some(TurnState::Finished);
        assert_eq!(
            shape,
            [
                ("SessionStart", false, None, None, None, None),
                (
                    "PreInvocation",
                    false,
                    None,
                    None,
                    None,
                    Some("delegate reading")
                ),
                (
                    "PreToolUse",
                    false,
                    Some(("2", "invoke_subagent")),
                    None,
                    None,
                    None
                ),
                (
                    "PostToolUse",
                    false,
                    Some(("2", "invoke_subagent")),
                    Some(true),
                    None,
                    None
                ),
                // The lead paused for its sub-agent: no moment. The sub-agent's own, whose
                // task came from its lead, not a person:
                ("SessionStart", true, None, None, None, None),
                (
                    "PreToolUse",
                    true,
                    Some(("2", "read_file")),
                    None,
                    None,
                    None
                ),
                (
                    "PostToolUse",
                    true,
                    Some(("2", "read_file")),
                    Some(true),
                    None,
                    None
                ),
                ("Stop", true, None, None, finished, None),
                // The sub-agent's result woke the lead; its call on it is no prompt.
                ("Stop", false, None, None, finished, None),
                // A key the service refused.
                ("SessionStart", false, None, None, None, None),
                ("PreInvocation", false, None, None, None, Some("hello")),
                ("Stop", false, None, None, Some(TurnState::Failed), None),
            ]
        );
        for moment in &said[..typed] {
            assert_eq!(moment.external_id, lead);
            assert!(moment.sub_agent.as_deref().is_none_or(|id| id == child));
            assert!(
                moment
                    .cwd
                    .as_deref()
                    .is_some_and(|cwd| cwd.ends_with("work"))
            );
            assert_eq!(moment.model.as_deref(), Some("gemini-3.1-pro-low"));
        }
        let failure = said.last().unwrap().failure.as_ref().unwrap();
        assert_eq!(
            (failure.kind, failure.http_status),
            (FailureKind::SignIn, Some(401))
        );
        let nowhere = Path::new("/nonexistent");
        assert!(
            read(
                nowhere,
                "PreInvocation",
                r#"{"conversationId":"c","invocationNum":1}"#
            )
            .is_none()
        );
        assert!(read(nowhere, "PostInvocation", r#"{"conversationId":"c"}"#).is_none());
        let failed = r#"{"conversationId":"c","stepIdx":4,"toolCall":{"name":"run_command","args":{}},"error":"exit status 1"}"#;
        assert_eq!(
            read(nowhere, "PostToolUse", failed).unwrap().ok,
            Some(false)
        );
        let again = r#"{"conversationId":"c","executionNum":1,"fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#;
        assert!(read(nowhere, "Stop", again).unwrap().continued);
    }

    #[test]
    fn an_answer_is_printed_as_antigravity_parses_it() {
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
            serde_json::from_str::<Value>(&answered.stdout).ok()
        };
        let allow = || Decision::Allow {
            reason: "yes".into(),
            always: None,
        };
        assert_eq!(
            stdout(said("PreToolUse", None, Some(allow()))),
            Some(json!({"decision": "auto_approve", "reason": "yes"}))
        );
        let deny = || Decision::Deny {
            reason: "Use the tests folder.".into(),
        };
        assert_eq!(
            stdout(said("PreToolUse", None, Some(deny()))),
            Some(json!({"decision": "deny", "reason": "Use the tests folder."}))
        );
        assert_eq!(
            stdout(said("Stop", Some("Words wait."), None)),
            Some(json!({"decision": "continue", "reason": "Words wait."}))
        );
        for event in ["SessionStart", "PreInvocation"] {
            assert_eq!(
                stdout(said(event, Some("Words wait."), None)),
                Some(json!({"injectSteps": [{"systemMessage": {"systemMessage": "Words wait."}}]}))
            );
        }
        assert_eq!(stdout(said("PostToolUse", None, None)), None);
        assert!(said("PreToolUse", Some("hi"), None).is_err());
        assert!(said("PostToolUse", Some("hi"), None).is_err());
        assert!(said("PostInvocation", None, None).is_err());
        assert!(said("Stop", None, Some(deny())).is_err());
        let always = Decision::Allow {
            reason: "yes".into(),
            always: Some(steery_sdk::wire::harness::Always::Tool),
        };
        assert!(said("PreToolUse", None, Some(always)).is_err());
    }
}
