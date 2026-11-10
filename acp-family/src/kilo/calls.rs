//! What a Kilo tool call is: its tool as Steery spells it, its group, Steery's own tool
//! where it is one, and the questions a `question` call puts to the person.
//!
//! The groups are Steery's (`steery-types` `calls::TOOL_GROUPS`), read off a tool's name
//! alone. Kilo names an MCP server's tool `<server>_<tool>`, each part with every character
//! outside `[A-Za-z0-9_-]` made `_`, so nothing in the name marks where the server ends: the
//! servers Kilo's configuration names say it, the longest first.

use serde_json::Value;
use steery_sdk::wire::harness::{Asks, Call, Choice, Group, Question};

/// Steery's MCP server, as setup names it in Kilo's configuration.
pub const STEERY: &str = "steery";

/// One call as Kilo keeps it: a `tool` part's `callID`, `tool` and `state.input`. `servers`
/// are the MCP servers Kilo's configuration names.
pub fn call(id: &str, name: &str, input: Value, servers: &[String]) -> Call {
    let (tool, group, server) = match builtin(name) {
        Some(group) => (name.to_owned(), group, None),
        None => match served(name, servers) {
            Some((server, inner)) => {
                let group = if server == STEERY {
                    Group::Steery
                } else {
                    Group::Mcp
                };
                let named = (group == Group::Mcp).then(|| server.to_owned());
                (format!("mcp__{server}__{inner}"), group, named)
            }
            None => (name.to_owned(), Group::Custom, None),
        },
    };
    let steery = (group == Group::Steery)
        .then(|| tool.strip_prefix("mcp__steery__").map(str::to_owned))
        .flatten();
    let asks = (name == "question").then(|| asks(&input)).flatten();
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

/// The server of an MCP tool's name and the tool's own name: the longest server whose
/// sanitized name, then `_`, starts it. Steery's server is always one.
fn served<'a>(name: &'a str, servers: &'a [String]) -> Option<(String, &'a str)> {
    servers
        .iter()
        .map(String::as_str)
        .chain([STEERY])
        .filter_map(|server| {
            let inner = name.strip_prefix(&sanitized(server))?.strip_prefix('_')?;
            (!inner.is_empty()).then_some((server, inner))
        })
        .max_by_key(|(server, _)| server.len())
        .map(|(server, inner)| (server.to_owned(), inner))
}

/// A name as Kilo puts it in a tool's name.
fn sanitized(name: &str) -> String {
    name.chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                true => c,
                false => '_',
            },
        )
        .collect()
}

fn builtin(name: &str) -> Option<Group> {
    BUILTINS
        .iter()
        .find(|(_, tools)| tools.contains(&name))
        .map(|(group, _)| *group)
}

/// Kilo's `question`: `{questions: [{question, header, options: [{label, description}],
/// multiple?}]}`.
fn asks(input: &Value) -> Option<Asks> {
    let questions: Vec<Question> = input
        .get("questions")?
        .as_array()?
        .iter()
        .filter_map(|asked| {
            Some(Question {
                question: asked.get("question")?.as_str()?.to_owned(),
                header: asked
                    .get("header")
                    .and_then(Value::as_str)
                    .filter(|header| !header.is_empty())
                    .map(str::to_owned),
                multiple: asked.get("multiple").and_then(Value::as_bool) == Some(true),
                options: asked
                    .get("options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|option| {
                        Some(Choice {
                            label: option.get("label")?.as_str()?.to_owned(),
                            description: option
                                .get("description")
                                .and_then(Value::as_str)
                                .filter(|text| !text.is_empty())
                                .map(str::to_owned),
                            preview: None,
                        })
                    })
                    .collect(),
            })
        })
        .collect();
    (!questions.is_empty()).then_some(Asks::Questions(questions))
}

/// Kilo's own tools by group, as its tool registry names them (Kilo 7.8).
#[rustfmt::skip]
const BUILTINS: &[(Group, &[&str])] = &[
    (Group::Shell, &["bash", "background_process"]),
    (Group::FilesRead, &["read", "notebook_read", "repo_overview"]),
    (Group::FilesEdit, &["edit", "write", "apply_patch", "multiedit", "notebook_edit"]),
    (Group::Search, &["glob", "grep", "list", "codesearch", "semantic_search", "lsp"]),
    (Group::Web, &["webfetch", "websearch"]),
    (Group::Browser, &["browser_open"]),
    (Group::Code, &["execute"]),
    (Group::Agents, &["task", "agent_manager", "agent_manager_models", "board_read", "board_post"]),
    (Group::Plan, &["todowrite", "todoread", "plan_enter", "plan_exit", "open_plan"]),
    (Group::Ask, &["question", "suggest"]),
    (Group::Memory, &["skill", "kilo_memory_recall", "kilo_memory_save", "kilo_local_recall"]),
    (Group::Harness, &[
        "get_context_info", "compact", "schedule_wakeup", "cancel_wakeup", "cron_create",
        "cron_delete", "cron_list", "notify_user", "link_pr", "send_file", "repo_clone",
        "invalid",
    ]),
    (Group::Media, &["generate_image", "chart"]),
];
