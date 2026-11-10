//! What an Amp tool call is: its tool as Amp spells it, its group, and its MCP server.
//! Amp names an MCP server's tool `mcp__<server>__<tool>`; Steery's own server is `steery`.

use serde_json::Value;
use steery_sdk::wire::harness::{Call, Group};

/// The call `id` of `tool` with `input`.
pub fn call(id: &str, tool: &str, input: Value) -> Call {
    let (group, server, steery) = match tool.strip_prefix("mcp__").and_then(|rest| rest.split_once("__")) {
        Some(("steery", named)) => (Group::Steery, None, Some(named.to_owned())),
        Some((server, _)) => (Group::Mcp, Some(server.to_owned()), None),
        None => (builtin(tool), None, None),
    };
    Call {
        id: id.to_owned(),
        tool: tool.to_owned(),
        input,
        group,
        server,
        steery,
        asks: None,
        parent: None,
        response: None,
    }
}

/// The group of one of Amp's own tools.
fn builtin(tool: &str) -> Group {
    match tool {
        "Bash" | "run_terminal_command" | "shell_command" => Group::Shell,
        "Read" | "read_file" => Group::FilesRead,
        "edit_file" | "create_file" | "apply_patch" | "Write" | "undo_edit" => Group::FilesEdit,
        "Grep" | "glob" | "ripgrep" | "codebase_search_agent" | "finder" => Group::Search,
        "web_search" | "read_web_page" => Group::Web,
        "Task" | "oracle" | "subagent" => Group::Agents,
        "todo_write" | "todo_read" => Group::Plan,
        _ => Group::Custom,
    }
}
