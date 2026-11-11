//! What a Copilot tool call is: its group, Steery's own tool where it is one, and what it
//! puts to the person where it waits on them.
//!
//! The groups are read off a tool's name alone: Copilot's own tools by name, and an MCP
//! tool by its server. Copilot's transcript names an MCP tool's server and tool apart
//! (`mcpServerName`, `mcpToolName`), and its hooks spell it `<server>-<tool>`; Steery
//! spells it `mcp__<server>__<tool>`.

use serde_json::Value;
use steery_sdk::wire::harness::{Asks, Call, Choice, Group, Question};

use super::STEERY;

/// One call as Copilot names it: a tool's `toolCallId`, `toolName` and arguments. Its
/// `parent` and `response` are the caller's to set.
pub(crate) fn call(id: &str, tool: &str, input: Value) -> Call {
    match tool
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
    {
        Some((server, inner)) => mcp(id, server, inner, input),
        None => match builtin(tool) {
            Some(group) => made(id, tool.to_owned(), input, group, None, None),
            // Copilot's own tools are one word with underscores: a dash joins a server's
            // name to its tool's.
            None => match tool.split_once('-') {
                Some((server, inner)) if !server.is_empty() && !inner.is_empty() => {
                    mcp(id, server, inner, input)
                }
                _ => made(id, tool.to_owned(), input, Group::Custom, None, None),
            },
        },
    }
}

/// One call of the tool `tool` that MCP server `server` offers.
pub(crate) fn mcp(id: &str, server: &str, tool: &str, input: Value) -> Call {
    let named = format!("mcp__{server}__{tool}");
    if server == STEERY {
        let steery = (!tool.is_empty()).then(|| tool.to_owned());
        return made(id, named, input, Group::Steery, None, steery);
    }
    made(id, named, input, Group::Mcp, Some(server.to_owned()), None)
}

fn made(
    id: &str,
    tool: String,
    input: Value,
    group: Group,
    server: Option<String>,
    steery: Option<String>,
) -> Call {
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

/// Copilot's own tools, by the names its transcript and hooks give them.
fn builtin(tool: &str) -> Option<Group> {
    Some(match tool {
        "bash" | "powershell" | "read_bash" | "write_bash" | "stop_bash" | "list_bash"
        | "read_powershell" | "write_powershell" | "stop_powershell" | "list_powershell" => {
            Group::Shell
        }
        "view" => Group::FilesRead,
        "create" | "edit" | "str_replace_editor" | "apply_patch" => Group::FilesEdit,
        "grep" | "rg" | "glob" | "lexical_code_search" | "semantic_code_search" => Group::Search,
        "web_fetch" | "web_search" => Group::Web,
        "task" | "read_agent" | "write_agent" | "list_agents" => Group::Agents,
        "update_todo" | "exit_plan_mode" | "report_intent" => Group::Plan,
        "ask_user" => Group::Ask,
        "store_memory" => Group::Memory,
        "skill" | "sql" | "session_store_sql" | "fetch_copilot_cli_documentation" => Group::Harness,
        _ => return None,
    })
}

/// What a call waits on the person for: `ask_user`, one question with its choices, and
/// `exit_plan_mode`, a plan waiting for a yes.
fn asks(tool: &str, input: &Value) -> Option<Asks> {
    let said = |name: &str| input.get(name).and_then(Value::as_str).map(str::to_owned);
    match tool {
        "ask_user" => Some(Asks::Questions(vec![Question {
            question: said("question").unwrap_or_default(),
            header: None,
            multiple: false,
            options: input
                .get("choices")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|label| Choice {
                    label: label.to_owned(),
                    description: None,
                    preview: None,
                })
                .collect(),
        }])),
        "exit_plan_mode" => Some(Asks::Plan(
            said("plan").or_else(|| said("summary")).unwrap_or_default(),
        )),
        _ => None,
    }
}
