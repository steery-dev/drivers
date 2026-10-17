//! What a Codex tool call is: its tool as Steery spells it, its group, Steery's own tool
//! where it is one, and what a call that waits on the person puts to them.
//!
//! Codex files a tool an MCP server offers under the server (`"namespace":
//! "mcp__steery"`, `"name": "sessions_describe"`), where its hooks write
//! `mcp__steery__sessions_describe`. Its own namespaces (`multi_agent_v1`, `collaboration`,
//! `clock`) file its built-in tools and are no part of their names; its hooks glue them on
//! (`multi_agent_v1wait_agent`, `clock.sleep`). The groups are Steery's, read off a tool's
//! name alone, never its arguments.

use serde_json::Value;
use steery_sdk::wire::harness::{Asks, Call, Choice, Group, Question};

/// Codex's own namespaces.
const NAMESPACES: &[&str] = &["multi_agent_v1", "collaboration", "clock"];

/// A call as the rollout files it: `name`, and `namespace` where an MCP server or Codex's
/// own family offers it.
pub fn spelled(name: &str, namespace: Option<&str>) -> String {
    match namespace {
        Some(server) if server.starts_with("mcp__") => format!("{server}__{name}"),
        _ => name.to_owned(),
    }
}

/// A tool as Codex's hooks name it, its own namespace dropped.
pub fn unglued(tool: &str) -> &str {
    for namespace in NAMESPACES {
        if let Some(rest) = tool.strip_prefix(namespace) {
            let rest = rest.strip_prefix('.').unwrap_or(rest);
            if !rest.is_empty() {
                return rest;
            }
        }
    }
    tool
}

/// One call: its id, its tool as Steery spells it, and its input as Codex wrote it. A
/// function's arguments are JSON text, read as the object they hold where they hold one.
pub fn call(id: &str, tool: &str, input: Value) -> Call {
    let input = match input {
        Value::String(text) => serde_json::from_str::<Value>(&text)
            .ok()
            .filter(Value::is_object)
            .unwrap_or(Value::String(text)),
        other => other,
    };
    let tool = unglued(tool).to_owned();
    let (group, server) = group(&tool);
    let steery = tool
        .strip_prefix("mcp__steery__")
        .filter(|tool| !tool.is_empty() && !tool.starts_with('_'))
        .map(str::to_owned);
    let asks = asks(&tool, &input);
    Call {
        id: id.to_owned(),
        tool,
        input,
        group,
        server,
        steery,
        asks,
        parent: None,
        response: None,
    }
}

/// The group of a tool, and its MCP server where the group is `mcp`.
fn group(tool: &str) -> (Group, Option<String>) {
    let lowered = tool.to_lowercase();
    let Some(mut rest) = lowered.strip_prefix("mcp__") else {
        return (plain(&lowered), None);
    };
    let mut from = "mcp__".len();
    while let Some(inner) = rest.strip_prefix("mcp__") {
        rest = inner;
        from += "mcp__".len();
    }
    // `mcp__<server>__<tool>`, a Codex server spelled `<server>.<family>` included.
    let (server, inner) = match rest.split_once("__") {
        Some((server, inner)) => (server.split('.').next().unwrap_or(server), inner),
        None => rest.split_once('.').unwrap_or((rest, "")),
    };
    let named = tool
        .get(from..from + server.len())
        .filter(|named| named.eq_ignore_ascii_case(server))
        .unwrap_or(server);
    served(server, inner, named)
}

/// A tool an MCP server offers, by the server's lowercased name and the tool's; `named`
/// is the server as Codex spells it.
fn served(server: &str, tool: &str, named: &str) -> (Group, Option<String>) {
    let server = server.replace(['-', '.'], "_");
    // A connector's name with the id a harness appends to it (`notes_e03a32f3eb8c`).
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

/// Whether a server's name is an id rather than a word.
fn opaque(name: &str) -> bool {
    name.split('_')
        .next()
        .is_some_and(|head| head.len() >= 8 && head.chars().all(|c| c.is_ascii_hexdigit()))
}

/// A tool no MCP server offers, by its lowercased name.
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

/// What a call puts to the person inside Codex, where it waits on them: `request_user_input`
/// (`{questions: [{id, header, question, options: [{label, description}]}]}`), and the two
/// question tools the hook document Codex shares with Claude Code names, `AskUserQuestion`
/// and `ExitPlanMode`.
fn asks(tool: &str, input: &Value) -> Option<Asks> {
    match tool.to_lowercase().as_str() {
        "exitplanmode" | "exit_plan_mode" => Some(Asks::Plan(
            input
                .get("plan")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        )),
        "askuserquestion" | "request_user_input" | "request_user_input_async" => {
            let questions: Vec<Question> = input
                .get("questions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(question)
                .collect();
            (!questions.is_empty()).then_some(Asks::Questions(questions))
        }
        _ => None,
    }
}

fn question(asked: &Value) -> Option<Question> {
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    Some(Question {
        question: text(asked, "question")?,
        header: text(asked, "header"),
        multiple: asked.get("multiSelect").and_then(Value::as_bool) == Some(true),
        options: asked
            .get("options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|option| {
                Some(Choice {
                    label: text(option, "label")?,
                    description: text(option, "description"),
                    preview: text(option, "preview"),
                })
            })
            .collect(),
    })
}

/// The harnesses' own tools by name, lowercased, each with its group (`steery-types`
/// `calls::BUILTINS`).
#[rustfmt::skip]
const BUILTINS: &[(Group, &[&str])] = &[
    (Group::Shell, &[
        "bash", "shell", "exec", "exec_command", "write_stdin", "connect_write_stdin",
        "local_shell", "powershell", "run_in_terminal", "read_terminal", "list_terminal_tabs",
    ]),
    (Group::FilesRead, &["read", "read_file", "view", "view_image"]),
    (Group::FilesEdit, &[
        "edit", "write", "apply_patch", "write_file", "edit_file", "create_file", "artifact",
    ]),
    (Group::Search, &[
        "grep", "glob", "find", "search", "ls", "list_dir", "list_directory", "file_search",
    ]),
    (Group::Web, &["web_search", "websearch", "web_fetch", "webfetch", "fetch", "web"]),
    (Group::Browser, &["browser", "computer"]),
    (Group::Code, &["eval", "js", "repl", "python"]),
    (Group::Agents, &[
        "spawn_agent", "wait_agent", "send_input", "send_message", "close_agent",
        "resume_agent", "interrupt_agent", "followup_task", "list_agents", "wait",
        "list_threads", "list_archived_threads", "read_thread", "send_message_to_thread",
        "wait_threads", "spawn_task", "dismiss_task",
    ]),
    (Group::Plan, &[
        "todo", "update_plan", "goal", "get_goal", "create_goal", "update_goal",
        "exitplanmode", "exit_plan_mode", "think",
    ]),
    (Group::Ask, &[
        "askuserquestion", "request_user_input", "request_user_input_async",
    ]),
    (Group::Memory, &["memory", "skill"]),
    (Group::Harness, &[
        "list_mcp_resources", "list_mcp_resource_templates", "read_mcp_resource", "sleep",
    ]),
    (Group::Media, &["generate_image", "image_gen", "tts"]),
];

/// Kinds of MCP server, matched against a server's whole name or one word of it
/// (`steery-types` `calls::KINDS`). A server of `harness` is one Codex ships for itself.
#[rustfmt::skip]
const KINDS: &[(Group, &[&str])] = &[
    (Group::Steery, &["steery"]),
    (Group::Browser, &[
        "chrome", "chrome_devtools", "browser", "cua_repl", "cua", "playwright", "puppeteer",
    ]),
    (Group::Code, &["node_repl", "js_repl", "python", "visualize", "jupyter"]),
    (Group::Shell, &["terminal"]),
    (Group::Harness, &["codex_app"]),
];

/// The first words that say a tool no MCP server offers drives a browser.
const BROWSER_WORDS: &[&str] = &["browser", "chrome", "playwright", "puppeteer"];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_tools_are_named_and_grouped_as_steery_reads_them() {
        let steery = call(
            "c1",
            &spelled("sessions_describe", Some("mcp__steery")),
            json!("{\"id\":\"r\"}"),
        );
        assert_eq!(steery.tool, "mcp__steery__sessions_describe");
        assert_eq!(steery.group, Group::Steery);
        assert_eq!(steery.steery.as_deref(), Some("sessions_describe"));
        assert_eq!(steery.input, json!({"id": "r"}));

        let shell = call(
            "c2",
            &spelled("exec_command", None),
            json!("{\"cmd\":\"ls\"}"),
        );
        assert_eq!(shell.group, Group::Shell);
        // Codex's hooks glue its own namespace onto a tool's name.
        assert_eq!(
            call("c3", "multi_agent_v1wait_agent", Value::Null).tool,
            "wait_agent"
        );
        assert_eq!(
            call("c3", "multi_agent_v1wait_agent", Value::Null).group,
            Group::Agents
        );
        assert_eq!(call("c4", "clock.sleep", Value::Null).group, Group::Harness);
        let theirs = call("c5", "mcp__notes__find", Value::Null);
        assert_eq!(
            (theirs.group, theirs.server.as_deref()),
            (Group::Mcp, Some("notes"))
        );
        assert_eq!(
            call("c6", "mcp__codex_apps__github_fetch", Value::Null).group,
            Group::Mcp
        );
        assert_eq!(
            call("c7", "apply_patch", json!("*** Begin Patch")).input,
            json!("*** Begin Patch")
        );
    }

    #[test]
    fn a_question_tool_says_what_it_asks() {
        let asked = call(
            "q",
            "request_user_input",
            json!({"questions": [
            {"id": "a", "header": "Scope", "question": "Which?", "options": [
                {"label": "One", "description": "the first"}, {"label": "Two"}]}]}),
        );
        let Some(Asks::Questions(questions)) = asked.asks else {
            panic!("asks")
        };
        assert_eq!(questions[0].header.as_deref(), Some("Scope"));
        assert_eq!(questions[0].options.len(), 2);
        assert_eq!(
            call("p", "ExitPlanMode", json!({"plan": "do it"})).asks,
            Some(Asks::Plan("do it".into()))
        );
    }
}
