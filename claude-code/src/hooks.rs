//! Claude Code's hooks: which of its moments Steery reads, what a moment's document says
//! (`hook/read`), and how Steery's answer is printed back for Claude Code to parse
//! (`hook/answer`). Claude Code runs each hook as a shell command with one JSON document
//! on standard input, the conversation's own id in its `session_id`, and reads the
//! hook's standard output as JSON.

use std::path::PathBuf;

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{
    Always, Answer, Append, Decision, Hook, HookAnswer, HookAnswered, HookKind, HookMoment,
    HookRead, Moment, Says, TurnState, Via,
};

/// The file Claude Code hands a `SessionStart` hook; its shell sources what is appended
/// there before every command the agent runs.
const ENV_FILE: &str = "CLAUDE_ENV_FILE";
/// The variable naming a process's lead (PROTOCOL.md §3.3), which the runtime reads off
/// the hook's own process.
const LEAD: &str = "STEERY_LEAD";

/// How long Claude Code gives a hook that only says a moment happened: it writes one
/// note, and a harness held up behind Steery is the one thing it must never cost a person.
const SAYS_WAITS: u32 = 5;
/// How long Claude Code gives a hook that may put the call to a person, who answers from
/// a phone; the runtime answers within `waits - 90`.
const ASKS_WAITS: u32 = 2100;

/// One of Claude Code's hook events Steery is hooked on.
pub(crate) struct Event {
    pub(crate) name: &'static str,
    kind: HookKind,
    says: &'static [Says],
    /// The `timeout` setup writes on its hook, in seconds, as Claude Code reads it.
    pub(crate) waits: u32,
}

/// Every event Claude Code fires once per moment that Steery reads, in the order setup
/// writes them. Claude Code reads a hook's `additionalContext` on `SessionStart`,
/// `UserPromptSubmit`, `PostToolUse`, `PostToolUseFailure` and `SubagentStart`, and goes
/// on from a `Stop` that a hook blocks with a reason.
pub(crate) const EVENTS: [Event; 11] = [
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
    // Both the moment a call is about to run and the moment Steery may put
    // `AskUserQuestion` or `ExitPlanMode` to a person instead.
    Event {
        name: "PreToolUse",
        kind: HookKind::BeforeCall,
        says: &[],
        waits: ASKS_WAITS,
    },
    // Words a person threw reach a conversation deep in a tool loop at a call come back.
    Event {
        name: "PostToolUse",
        kind: HookKind::AfterCall,
        says: &[Says::Notice, Says::Calls, Says::Working],
        waits: SAYS_WAITS,
    },
    // Fires in place of `PostToolUse` when the call fails.
    Event {
        name: "PostToolUseFailure",
        kind: HookKind::AfterCall,
        says: &[Says::Notice, Says::Working],
        waits: SAYS_WAITS,
    },
    Event {
        name: "PermissionRequest",
        kind: HookKind::Permission,
        says: &[],
        waits: ASKS_WAITS,
    },
    Event {
        name: "Notification",
        kind: HookKind::Notification,
        says: &[],
        waits: SAYS_WAITS,
    },
    Event {
        name: "Stop",
        kind: HookKind::TurnEnd,
        says: &[Says::Context],
        waits: SAYS_WAITS,
    },
    // The turn ended in a failure; Claude Code reads nothing back from it.
    Event {
        name: "StopFailure",
        kind: HookKind::TurnEnd,
        says: &[],
        waits: SAYS_WAITS,
    },
    Event {
        name: "SessionEnd",
        kind: HookKind::SessionEnd,
        says: &[],
        waits: SAYS_WAITS,
    },
    // A Task sub-agent starting, fired in its lead's hooks with the lead's `session_id`
    // beside the sub-agent's own `agent_id`: where the sub-agent is told its lines go.
    Event {
        name: "SubagentStart",
        kind: HookKind::SubAgentStart,
        says: &[Says::Context],
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
            // Only a start is handed the file a lead is appended to.
            env: (event.kind == HookKind::SessionStart)
                .then(|| ENV_FILE.to_owned())
                .into_iter()
                .collect(),
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
        HookKind::BeforeCall | HookKind::AfterCall | HookKind::Permission => {
            match (text(document, "tool_use_id"), text(document, "tool_name")) {
                (Some(id), Some(tool)) => Some(crate::calls::call(
                    id,
                    tool,
                    document.get("tool_input").cloned().unwrap_or(Value::Null),
                )),
                _ => None,
            }
        }
        _ => None,
    };
    let at_end = event.kind == HookKind::TurnEnd;
    Some(Moment {
        event: name.to_owned(),
        sub_agent: text(document, "agent_id").map(str::to_owned),
        at: None,
        cwd: document
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| cwd.starts_with('/') && !cwd.chars().any(char::is_control))
            .map(PathBuf::from),
        // Claude Code files every transcript under its own root.
        transcript: None,
        call,
        ok: match name {
            "PostToolUse" => Some(true),
            "PostToolUseFailure" => Some(false),
            _ => None,
        },
        outcome: match name {
            "Stop" => Some(TurnState::Finished),
            "StopFailure" => Some(TurnState::Failed),
            _ => None,
        },
        continued: at_end
            && document.get("stop_hook_active").and_then(Value::as_bool) == Some(true),
        // `prompt_input_exit` is `/exit`, `/quit`, Ctrl+C and Ctrl+D at the prompt, `clear`
        // is `/clear`; `other` (signals, a lost terminal, `-p` runs) and `resume` are not
        // the person leaving.
        person_exited: event.kind == HookKind::SessionEnd
            && matches!(
                document.get("reason").and_then(Value::as_str),
                Some("prompt_input_exit" | "clear")
            ),
        prompt: (event.kind == HookKind::Prompt)
            .then(|| document.get("prompt").and_then(Value::as_str))
            .flatten()
            .map(str::to_owned),
        context_left: None,
        suggestions: if event.kind == HookKind::Permission {
            document
                .get("permission_suggestions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        },
        external_id,
    })
}

/// What Steery decided, printed as Claude Code parses it on this event: a paragraph as
/// `additionalContext`, or at a `Stop` as the reason the turn goes on; a decision as
/// `PreToolUse`'s `permissionDecision` or `PermissionRequest`'s `decision`; and the lead,
/// appended to `CLAUDE_ENV_FILE` where Claude Code named one. Always exit 0: a hook
/// that fails is one Claude Code reports to its person.
pub fn answer(answer: HookAnswer) -> Result<HookAnswered, Error> {
    let HookAnswer {
        event: name,
        document,
        env,
        answer: Answer {
            say,
            decision,
            lead,
        },
    } = answer;
    let event = event(&name)
        .ok_or_else(|| Error::invalid(format!("Steery is not hooked on Claude Code's {name}")))?;
    let output = match (event.kind, decision) {
        (_, None) => say.map(|say| context(event, say)).transpose()?,
        (HookKind::BeforeCall, Some(decision)) => Some(before_call(decision, say)?),
        (HookKind::Permission, Some(decision)) => {
            if say.is_some() {
                return Err(Error::invalid(
                    "Claude Code reads no paragraph at PermissionRequest",
                ));
            }
            Some(permission(decision, &parse(&document)?)?)
        }
        (_, Some(_)) => {
            return Err(Error::invalid(format!(
                "Claude Code reads no decision at {name}"
            )));
        }
    };
    // Claude Code names the file only to a `SessionStart` hook; a lead anywhere else has
    // nowhere to go and leaves the processes unled, never the hook failed.
    let append = match (lead, env.get(ENV_FILE)) {
        (Some(lead), Some(file)) => Some(Append {
            path: PathBuf::from(file),
            text: exported(&lead)?,
        }),
        _ => None,
    };
    Ok(HookAnswered {
        stdout: output.map(|output| output.to_string()).unwrap_or_default(),
        exit_code: 0,
        append,
    })
}

/// A paragraph for the agent, where this event's hook can give one.
fn context(event: &Event, say: String) -> Result<Value, Error> {
    if event.says.is_empty() {
        return Err(Error::invalid(format!(
            "Claude Code reads no paragraph at {}",
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

/// `PreToolUse`'s decision: the call runs without Claude Code asking, or is blocked, the
/// reason being what the model reads. A paragraph beside it (a person's words on an
/// approved plan) is context, not a reason. Claude Code applies no standing permission
/// from this moment.
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
                "Claude Code keeps no permission from PreToolUse",
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

/// `PermissionRequest`'s decision. An allow may stand from now on: one of the
/// `permission_suggestions` Claude Code offered, passed back exactly as it wrote it; every
/// call of this tool; or every call at all — the last two kept to this conversation.
/// Claude Code's allow carries no message, so the reason is not printed; a deny's is.
fn permission(decision: Decision, document: &Value) -> Result<Value, Error> {
    let decision = match decision {
        Decision::Allow { always, .. } => {
            let mut decision = json!({"behavior": "allow"});
            if let Some(always) = always {
                decision["updatedPermissions"] = json!([standing(always, document)?]);
            }
            decision
        }
        Decision::Deny { reason } => json!({"behavior": "deny", "message": reason}),
    };
    Ok(json!({"hookSpecificOutput": {
        "hookEventName": "PermissionRequest",
        "decision": decision,
    }}))
}

/// The permission update an allow stands for.
fn standing(always: Always, document: &Value) -> Result<Value, Error> {
    Ok(match always {
        Always::Suggestion(index) => document
            .get("permission_suggestions")
            .and_then(Value::as_array)
            .and_then(|offered| offered.get(index as usize))
            .cloned()
            .ok_or_else(|| Error::invalid(format!("Claude Code offered no suggestion {index}")))?,
        Always::Tool => {
            let tool = text(document, "tool_name")
                .ok_or_else(|| Error::invalid("the document names no tool"))?;
            json!({
                "type": "addRules",
                "rules": [{"toolName": tool}],
                "behavior": "allow",
                "destination": "session",
            })
        }
        Always::Everything => json!({
            "type": "setMode",
            "mode": "bypassPermissions",
            "destination": "session",
        }),
    })
}

/// The line that names a lead for every later process, quoted for the shell that sources
/// it: a harness id and a conversation id, neither able to break out of the quotes.
fn exported(lead: &str) -> Result<String, Error> {
    let named = lead.split_once(':').is_some_and(|(harness, external_id)| {
        !harness.is_empty()
            && harness
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !external_id.is_empty()
            && external_id.len() <= 128
            && !external_id.starts_with('.')
            && external_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    });
    if !named {
        return Err(Error::invalid(format!(
            "this lead cannot be exported: {lead:?}"
        )));
    }
    Ok(format!("export {LEAD}='{lead}'\n"))
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
    use std::collections::BTreeMap;

    use super::*;

    fn read_one(event: &str, document: Value, env: &[(&str, &str)]) -> Option<Moment> {
        read(HookRead {
            event: event.to_owned(),
            document: document.to_string(),
            env: env
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        })
        .expect("JSON")
        .moment
    }

    fn printed(event: &str, document: Value, answer: Answer) -> Value {
        let answered = super::answer(HookAnswer {
            event: event.to_owned(),
            document: document.to_string(),
            env: BTreeMap::new(),
            answer,
        })
        .expect("an answer Claude Code reads");
        assert_eq!(answered.exit_code, 0);
        serde_json::from_str(&answered.stdout).expect("JSON")
    }

    fn say(line: &str) -> Answer {
        Answer {
            say: Some(line.to_owned()),
            ..Answer::default()
        }
    }

    fn decide(decision: Decision) -> Answer {
        Answer {
            decision: Some(decision),
            ..Answer::default()
        }
    }

    /// A person's own exit at the prompt parks a conversation; a signal, a lost terminal, a
    /// run nobody sat at and a switch to another conversation do not.
    #[test]
    fn only_a_persons_own_exit_parks_a_conversation() {
        let ended = |event: &str, reason: &str| {
            read_one(event, json!({"session_id": "s1", "reason": reason}), &[])
                .expect("a moment")
                .person_exited
        };
        for reason in ["prompt_input_exit", "clear"] {
            assert!(ended("SessionEnd", reason), "{reason}");
        }
        for reason in ["other", "resume", "logout"] {
            assert!(!ended("SessionEnd", reason), "{reason}");
        }
        assert!(!ended("Stop", "prompt_input_exit"));
    }

    #[test]
    fn a_sub_agents_moment_names_it() {
        let native = json!({
            "session_id": "conversation-1",
            "agent_id": "agent-7",
            "cwd": "/tmp",
            "hook_event_name": "UserPromptSubmit",
        });
        let moment = read_one("UserPromptSubmit", native, &[]).expect("a moment");
        assert_eq!(moment.external_id, "conversation-1");
        assert_eq!(moment.sub_agent.as_deref(), Some("agent-7"));

        let plain = json!({"session_id": "child-1", "agent_id": ""});
        let child = read_one("PreToolUse", plain, &[]).expect("a moment");
        assert_eq!(child.sub_agent, None);
    }

    #[test]
    fn a_hook_names_the_call_that_came_back() {
        let claude = json!({
            "session_id": "s1",
            "hook_event_name": "PostToolUseFailure",
            "tool_name": "Bash",
            "tool_input": {"command": "false"},
            "tool_use_id": "toolu_01ALePGEyBFcijva9u9iQ2a7",
            "error": "exit 1",
            "is_interrupt": false,
        });
        let moment = read_one("PostToolUseFailure", claude, &[]).expect("a moment");
        assert_eq!(moment.ok, Some(false));
        let call = moment.call.expect("the call that came back");
        assert_eq!(call.id, "toolu_01ALePGEyBFcijva9u9iQ2a7");
        assert_eq!(call.tool, "Bash");
        assert_eq!(call.input, json!({"command": "false"}));
    }

    #[test]
    fn a_hook_names_the_call_the_harness_is_about_to_make() {
        let document = json!({
            "session_id": "s1",
            "tool_name": "Bash",
            "tool_use_id": "toolu_1",
            "tool_input": {"command": "cargo test\ncargo build", "description": "Test"},
        });
        let moment = read_one("PreToolUse", document.clone(), &[]).expect("a moment");
        assert_eq!(moment.call.map(|call| call.tool).as_deref(), Some("Bash"));
        assert!(
            read_one("Stop", document, &[])
                .expect("a moment")
                .call
                .is_none(),
            "a turn's end names no call"
        );
        assert!(
            read_one("PreToolUse", json!({"session_id": "s1"}), &[])
                .expect("a moment")
                .call
                .is_none(),
            "a document naming no tool names no call"
        );
    }

    #[test]
    fn a_turns_end_says_how_it_ended_and_whether_a_hook_kept_it_going() {
        let ended = read_one(
            "Stop",
            json!({"session_id": "s1", "stop_hook_active": true}),
            &[],
        )
        .expect("a moment");
        assert_eq!(ended.outcome, Some(TurnState::Finished));
        assert!(ended.continued);
        let failed = read_one("StopFailure", json!({"session_id": "s1"}), &[]).expect("a moment");
        assert_eq!(failed.outcome, Some(TurnState::Failed));
        assert!(!failed.continued);
    }

    #[test]
    fn a_document_that_is_no_moment_steery_carries_reads_as_none() {
        assert!(read_one("PreCompact", json!({"session_id": "s1"}), &[]).is_none());
        assert!(read_one("Stop", json!({"cwd": "/tmp"}), &[]).is_none());
        assert!(
            read(HookRead {
                event: "Stop".into(),
                document: "not json".into(),
                env: BTreeMap::new(),
            })
            .is_err()
        );
    }

    #[test]
    fn a_harness_is_handed_the_paragraph_in_the_document_it_parses() {
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "PostToolUse",
            "PostToolUseFailure",
            "SubagentStart",
        ] {
            let document = printed(event, json!({"session_id": "s1"}), say("Steery: hi"));
            let out = &document["hookSpecificOutput"];
            assert_eq!(out["hookEventName"], event);
            assert_eq!(out["additionalContext"], "Steery: hi");
        }
        let kept = printed("Stop", json!({"session_id": "s1"}), say("Report first."));
        assert_eq!(
            kept,
            json!({"decision": "block", "reason": "Report first."})
        );
    }

    /// A moment that reads nothing back is told nothing, and an empty answer prints nothing.
    #[test]
    fn a_moment_that_reads_nothing_back_is_told_nothing() {
        for event in [
            "SessionEnd",
            "Notification",
            "StopFailure",
            "PermissionRequest",
        ] {
            assert!(
                super::answer(HookAnswer {
                    event: event.into(),
                    document: "{}".into(),
                    env: BTreeMap::new(),
                    answer: say("x"),
                })
                .is_err(),
                "{event}"
            );
        }
        let quiet = super::answer(HookAnswer {
            event: "Stop".into(),
            document: "{}".into(),
            env: BTreeMap::new(),
            answer: Answer::default(),
        })
        .unwrap();
        assert_eq!(quiet.stdout, "");
        assert_eq!(quiet.append, None);
    }

    #[test]
    fn a_tool_call_that_needs_a_decision_is_answered_in_claude_codes_shape() {
        let event = json!({
            "hook_event_name": "PermissionRequest",
            "session_id": "abc",
            "tool_name": "Bash",
            "tool_input": {"command": "cargo check"},
        });
        let run = printed(
            "PermissionRequest",
            event.clone(),
            decide(Decision::Allow {
                reason: "go".into(),
                always: None,
            }),
        );
        assert_eq!(
            run["hookSpecificOutput"]["decision"],
            json!({"behavior": "allow"})
        );
        let stop = printed(
            "PermissionRequest",
            event,
            decide(Decision::Deny {
                reason: "that would wipe the cache".into(),
            }),
        );
        assert_eq!(
            stop["hookSpecificOutput"]["decision"],
            json!({"behavior": "deny", "message": "that would wipe the cache"})
        );
    }

    /// The narrow scopes are Claude Code's own suggestions, passed back untouched; the two
    /// wide ones reach no further than the conversation.
    #[test]
    fn a_yes_can_be_made_to_stand_for_more_than_one_call() {
        let suggestion = json!({
            "type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "cargo check:*"}],
            "behavior": "allow",
            "destination": "localSettings",
        });
        let event = json!({
            "hook_event_name": "PermissionRequest",
            "session_id": "abc",
            "tool_name": "Bash",
            "tool_input": {"command": "cargo check"},
            "permission_suggestions": [suggestion],
        });
        let moment = read_one("PermissionRequest", event.clone(), &[]).expect("a moment");
        assert_eq!(moment.suggestions, vec![suggestion.clone()]);
        let allow = |always| {
            printed(
                "PermissionRequest",
                event.clone(),
                decide(Decision::Allow {
                    reason: "go".into(),
                    always: Some(always),
                }),
            )["hookSpecificOutput"]["decision"]["updatedPermissions"]
                .clone()
        };
        assert_eq!(allow(Always::Suggestion(0)), json!([suggestion]));
        let tool = allow(Always::Tool);
        assert_eq!(tool[0]["rules"], json!([{"toolName": "Bash"}]));
        assert_eq!(tool[0]["destination"], "session");
        let everything = allow(Always::Everything);
        assert_eq!(everything[0]["type"], "setMode");
        assert_eq!(everything[0]["destination"], "session");
        assert!(
            super::answer(HookAnswer {
                event: "PermissionRequest".into(),
                document: event.to_string(),
                env: BTreeMap::new(),
                answer: decide(Decision::Allow {
                    reason: "go".into(),
                    always: Some(Always::Suggestion(1)),
                }),
            })
            .is_err(),
            "a suggestion Claude Code never offered"
        );
    }

    #[test]
    fn a_question_or_plan_is_answered_before_the_tool_runs() {
        let answered = printed(
            "PreToolUse",
            json!({"session_id": "s1", "tool_name": "AskUserQuestion"}),
            decide(Decision::Deny {
                reason: "Their answer: blue".into(),
            }),
        );
        let out = &answered["hookSpecificOutput"];
        assert_eq!(out["permissionDecision"], "deny");
        assert_eq!(out["permissionDecisionReason"], "Their answer: blue");
        assert!(out["additionalContext"].is_null());

        let approved = printed(
            "PreToolUse",
            json!({"session_id": "s1", "tool_name": "ExitPlanMode"}),
            Answer {
                say: Some("They said: ship it".into()),
                decision: Some(Decision::Allow {
                    reason: "A person approved this plan in Steery.".into(),
                    always: None,
                }),
                lead: None,
            },
        );
        let out = &approved["hookSpecificOutput"];
        assert_eq!(out["hookEventName"], "PreToolUse");
        assert_eq!(out["permissionDecision"], "allow");
        assert_eq!(out["additionalContext"], "They said: ship it");
    }

    /// Claude Code's shell sources what a start hook appends to `CLAUDE_ENV_FILE` before
    /// every command the agent runs, so those processes inherit their lead.
    #[test]
    fn a_start_names_the_conversation_its_processes_lead() {
        let lead = |value: &str, env: &[(&str, &str)]| {
            super::answer(HookAnswer {
                event: "SessionStart".into(),
                document: json!({"session_id": "s1"}).to_string(),
                env: env
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
                answer: Answer {
                    lead: Some(value.to_owned()),
                    ..Answer::default()
                },
            })
        };
        let answered = lead("claude-code:s1", &[(ENV_FILE, "/tmp/env")]).unwrap();
        assert_eq!(answered.stdout, "");
        assert_eq!(
            answered.append,
            Some(Append {
                path: "/tmp/env".into(),
                text: "export STEERY_LEAD='claude-code:s1'\n".into(),
            })
        );
        assert_eq!(lead("claude-code:s1", &[]).unwrap().append, None);
        assert!(lead("claude-code:s1'; rm -rf ~", &[(ENV_FILE, "/tmp/env")]).is_err());
    }

    #[test]
    fn every_hook_is_a_command_with_a_bound() {
        let table = table();
        assert_eq!(table.len(), EVENTS.len());
        for hook in &table {
            assert_eq!(hook.via, Via::Command);
            assert!(hook.waits.is_some_and(|waits| waits > 0), "{}", hook.event);
        }
        for asks in ["PreToolUse", "PermissionRequest"] {
            let hook = table.iter().find(|hook| hook.event == asks).unwrap();
            assert_eq!(hook.waits, Some(ASKS_WAITS));
        }
    }
}
