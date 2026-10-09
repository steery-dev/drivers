//! What a Claude Code tool call is: its group, Steery's own tool where it is one, and what
//! it puts to the person where it waits on them.
//!
//! The groups are Steery's (`steery-types` `calls::tool_group`), read off a tool's name
//! alone, never its arguments: Claude Code's own tools by name, and an MCP tool, which
//! Claude Code always spells `mcp__<server>__<tool>`, by its server.

use serde_json::Value;
use steery_sdk::wire::harness::{Asks, Call, Choice, Group, Question};

use crate::STEERY;

/// One call as Claude Code names it: a `tool_use` block's `id`, `name` and `input` in a
/// transcript, or a hook document's `tool_use_id`, `tool_name` and `tool_input`. Its
/// `parent` and `response` are the caller's to set.
pub fn call(id: &str, tool: &str, input: Value) -> Call {
    let (group, server) = group(tool);
    let steery = tool
        .strip_prefix("mcp__")
        .and_then(|rest| rest.strip_prefix(STEERY))
        .and_then(|rest| rest.strip_prefix("__"))
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    let asks = asks(tool, &input);
    Call {
        id: id.to_owned(),
        tool: tool.to_owned(),
        input,
        group,
        server,
        steery,
        asks,
        parent: None,
        response: None,
    }
}

/// What a call waits on the person for: `AskUserQuestion`, a form of up to four questions
/// with their options, and `ExitPlanMode`, a plan waiting for a yes. A plan the call
/// carries none of is still a plan waiting, as the runtime's card says it.
fn asks(tool: &str, input: &Value) -> Option<Asks> {
    match tool {
        "AskUserQuestion" => Some(Asks::Questions(
            input
                .get("questions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(question)
                .collect(),
        )),
        "ExitPlanMode" => Some(Asks::Plan(
            input
                .get("plan")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        )),
        _ => None,
    }
}

/// One of `AskUserQuestion`'s questions: `{question, header, multiSelect, options}`, each
/// option `{label, description, preview}`. A question without `multiSelect` asks for one.
fn question(asked: &Value) -> Question {
    let said =
        |value: &Value, name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    Question {
        question: said(asked, "question").unwrap_or_default(),
        header: said(asked, "header")
            .map(|header| header.trim().to_owned())
            .filter(|header| !header.is_empty()),
        multiple: asked.get("multiSelect").and_then(Value::as_bool) == Some(true),
        options: asked
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|option| Choice {
                label: said(option, "label").unwrap_or_default(),
                description: said(option, "description"),
                preview: said(option, "preview"),
            })
            .collect(),
    }
}

/// The group of a tool, and its MCP server as Claude Code spells it where the group is
/// `mcp`.
fn group(tool: &str) -> (Group, Option<String>) {
    let lowered = tool.to_lowercase();
    let Some(rest) = lowered.strip_prefix("mcp__") else {
        return (plain(&lowered), None);
    };
    let (server, inner) = rest.split_once("__").unwrap_or((rest, ""));
    let from = "mcp__".len();
    let named = tool
        .get(from..from + server.len())
        .filter(|named| named.eq_ignore_ascii_case(server))
        .unwrap_or(server);
    served(server, inner, named)
}

/// A tool an MCP server offers, by the server's lowercased name and the tool's; `named` is
/// the server as Claude Code spells it.
fn served(server: &str, tool: &str, named: &str) -> (Group, Option<String>) {
    let server = server.replace(['-', '.'], "_");
    // A connector's name with the id appended to it (`notes_e03a32f3eb8c`).
    let bare = match server.rsplit_once('_') {
        Some((name, id)) if opaque(id) && !opaque(name) => name,
        _ => server.as_str(),
    };
    if bare.is_empty() {
        return (Group::Custom, None);
    }
    if opaque(bare) {
        return (Group::Mcp, Some(named.to_owned()));
    }
    match kind(bare) {
        Some(Group::Harness) => (builtin(tool).unwrap_or(Group::Harness), None),
        Some(group) => (group, None),
        None => (Group::Mcp, Some(named.to_owned())),
    }
}

/// Whether a server's name is an id rather than a word: its first part a run of at least
/// eight hex digits.
fn opaque(name: &str) -> bool {
    name.split('_')
        .next()
        .is_some_and(|head| head.len() >= 8 && head.chars().all(|c| c.is_ascii_hexdigit()))
}

/// A tool no MCP server offers, by its lowercased name: one of the harnesses' own, else
/// one whose first word names a browser, else one its person defined.
fn plain(name: &str) -> Group {
    builtin(name)
        .or_else(|| {
            let first = name.split(['_', '-', '.']).next().unwrap_or(name);
            BROWSER_WORDS.contains(&first).then_some(Group::Browser)
        })
        .unwrap_or(Group::Custom)
}

fn builtin(name: &str) -> Option<Group> {
    BUILTINS
        .iter()
        .find(|(_, tools)| tools.contains(&name))
        .map(|(group, _)| *group)
}

/// The group a lowercased MCP server's name says: the whole of it, else one of its words.
fn kind(server: &str) -> Option<Group> {
    let find = |word: &str| {
        KINDS
            .iter()
            .find(|(_, names)| names.contains(&word))
            .map(|(group, _)| *group)
    };
    find(server).or_else(|| {
        server
            .split('_')
            .filter(|word| !word.is_empty())
            .find_map(find)
    })
}

/// The harnesses' own tools by name, lowercased, each with its group (`steery-types`
/// `calls::BUILTINS`): Claude Code's `Bash`, `Read`, `Edit`, `Task`, `AskUserQuestion` and
/// the rest among them.
#[rustfmt::skip]
const BUILTINS: &[(Group, &[&str])] = &[
    (Group::Shell, &[
        "bash", "shell", "exec", "exec_command", "write_stdin", "connect_write_stdin",
        "local_shell", "powershell", "bashoutput", "killshell", "killbash", "hub", "monitor",
        "run_in_terminal", "read_terminal", "list_terminal_tabs",
    ]),
    (Group::FilesRead, &[
        "read", "read_file", "readfile", "readmediafile", "view", "notebookread", "view_image",
    ]),
    (Group::FilesEdit, &[
        "edit", "write", "multiedit", "notebookedit", "apply_patch", "write_file", "writefile",
        "edit_file", "create_file", "strreplacefile", "str_replace_editor", "ast_edit",
        "artifact",
    ]),
    (Group::Search, &[
        "grep", "glob", "find", "ast_grep", "search", "ls", "list_dir", "list_directory",
        "codebase_search", "file_search", "lsp", "security_scan",
    ]),
    (Group::Web, &[
        "web_search", "websearch", "web_fetch", "webfetch", "fetch", "fetchurl", "searchweb",
        "web",
    ]),
    (Group::Browser, &["browser", "computer"]),
    (Group::Code, &["eval", "codemode", "js", "repl", "python", "debug"]),
    (Group::Agents, &[
        "task", "agent", "spawn_agent", "wait_agent", "send_input", "send_message",
        "sendmessage", "close_agent", "resume_agent", "interrupt_agent", "followup_task",
        "list_agents", "listagents", "wait", "taskoutput", "taskstop", "workflow", "advise",
        "irc", "vibe_spawn", "vibe_send", "vibe_wait", "vibe_kill", "vibe_list",
        "list_threads", "list_archived_threads", "read_thread", "send_message_to_thread",
        "wait_threads", "spawn_task", "dismiss_task", "list_sessions", "list_events",
    ]),
    (Group::Plan, &[
        "todo", "todowrite", "todoread", "update_plan", "goal", "get_goal", "create_goal",
        "update_goal", "enterplanmode", "exitplanmode", "exit_plan_mode", "think",
        "mark_chapter",
    ]),
    (Group::Ask, &[
        "ask", "askuserquestion", "request_user_input", "request_user_input_async",
        "pushnotification", "senduserfile",
    ]),
    (Group::Memory, &[
        "learn", "memory", "memory_edit", "retain", "recall", "reflect", "manage_skill", "skill",
    ]),
    (Group::Harness, &[
        "yield", "toolsearch", "structuredoutput", "schedulewakeup", "enterworktree",
        "exitworktree", "list_mcp_resources", "list_mcp_resource_templates",
        "read_mcp_resource", "resolve", "reject", "report_issue", "checkpoint", "rewind",
        "sleep", "readnotifications",
    ]),
    (Group::Media, &["generate_image", "image_gen", "tts"]),
];

/// Kinds of MCP server, matched against a server's whole name or one word of it
/// (`steery-types` `calls::KINDS`). A server of `harness` is one shipped for the harness
/// itself: the Claude app's `ccd_*` servers in a session it started.
#[rustfmt::skip]
const KINDS: &[(Group, &[&str])] = &[
    (Group::Steery, &["steery"]),
    (Group::Browser, &[
        "chrome", "chrome_devtools", "claude_browser", "browser", "cua_repl", "cua", "playwright",
        "puppeteer",
    ]),
    (Group::Code, &["node_repl", "js_repl", "python", "visualize", "jupyter"]),
    (Group::Shell, &["terminal"]),
    (Group::Harness, &["ccd_session", "ccd_session_mgmt", "ccd_view", "ccd_host"]),
];

/// The first words that say a tool no MCP server offers drives a browser.
const BROWSER_WORDS: &[&str] = &["browser", "chrome", "playwright", "puppeteer"];

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Claude Code's own tools by their names, an MCP tool by its server, and Steery's own
    /// by the tool it is.
    #[test]
    fn a_call_is_grouped_by_its_tool_alone() {
        let grouped = |tool: &str| {
            let call = call("t", tool, Value::Null);
            (call.group, call.server, call.steery)
        };
        for (tool, group) in [
            ("Bash", Group::Shell),
            ("Read", Group::FilesRead),
            ("Edit", Group::FilesEdit),
            ("Write", Group::FilesEdit),
            ("MultiEdit", Group::FilesEdit),
            ("NotebookEdit", Group::FilesEdit),
            ("Grep", Group::Search),
            ("Glob", Group::Search),
            ("WebFetch", Group::Web),
            ("WebSearch", Group::Web),
            ("Task", Group::Agents),
            ("Agent", Group::Agents),
            ("TodoWrite", Group::Plan),
            ("ExitPlanMode", Group::Plan),
            ("AskUserQuestion", Group::Ask),
            ("Skill", Group::Memory),
            ("browser_new_tab", Group::Browser),
            ("my_own_tool", Group::Custom),
        ] {
            assert_eq!(grouped(tool), (group, None, None), "{tool}");
        }
        assert_eq!(
            grouped("mcp__steery__requests_create"),
            (Group::Steery, None, Some("requests_create".to_owned()))
        );
        assert_eq!(
            grouped("mcp__claude-in-chrome__navigate"),
            (Group::Browser, None, None)
        );
        assert_eq!(
            grouped("mcp__ccd_session__mark_chapter"),
            (Group::Plan, None, None),
            "a tool the app ships for itself, by the tool where it is known"
        );
        assert_eq!(
            grouped("mcp__Linear__list_issues"),
            (Group::Mcp, Some("Linear".to_owned()), None)
        );
        assert_eq!(
            grouped("mcp__notes_e03a32f3eb8c__find"),
            (Group::Mcp, Some("notes_e03a32f3eb8c".to_owned()), None),
            "a connector's appended id is no part of its kind"
        );
    }

    /// `AskUserQuestion` puts its questions to the person, and `ExitPlanMode` its plan.
    #[test]
    fn a_question_or_a_plan_waits_on_the_person() {
        let asked = call(
            "t1",
            "AskUserQuestion",
            json!({"questions": [
                {"question": "Which store?", "header": " Storage ", "multiSelect": false,
                 "options": [{"label": "Postgres", "description": "one node"},
                             {"label": "Split", "preview": "| a | b |"}]},
                {"question": "Which regions?", "multiSelect": true, "options": []}]}),
        );
        assert_eq!(
            asked.asks,
            Some(Asks::Questions(vec![
                Question {
                    question: "Which store?".into(),
                    header: Some("Storage".into()),
                    multiple: false,
                    options: vec![
                        Choice {
                            label: "Postgres".into(),
                            description: Some("one node".into()),
                            preview: None,
                        },
                        Choice {
                            label: "Split".into(),
                            description: None,
                            preview: Some("| a | b |".into()),
                        },
                    ],
                },
                Question {
                    question: "Which regions?".into(),
                    header: None,
                    multiple: true,
                    options: Vec::new(),
                },
            ]))
        );
        assert_eq!(
            call("t2", "ExitPlanMode", json!({"plan": "## Step one"})).asks,
            Some(Asks::Plan("## Step one".into()))
        );
        assert_eq!(
            call("t3", "ExitPlanMode", json!({})).asks,
            Some(Asks::Plan(String::new()))
        );
        assert_eq!(call("t4", "Bash", json!({"command": "ls"})).asks, None);
    }
}
