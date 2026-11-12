//! What a Crush tool call is: its tool as Crush spells it, its group, and its MCP server.
//! Crush names an MCP server's tool `mcp_<server>_<tool>` (`internal/agent/tools/mcp-tools.go`),
//! so a server is told by the names Crush loads, the longest first; Steery's own is `steery`.

use serde_json::Value;
use steery_sdk::wire::harness::{Call, Group};

/// The call `id` of `tool` with `input`; `servers` names the MCP servers Crush loads.
pub fn call(id: &str, tool: &str, input: Value, servers: &[String]) -> Call {
    let (group, server, steery) = match mcp(tool, servers) {
        Some(("steery", named)) => (Group::Steery, None, Some(named.to_owned())),
        Some((server, _)) => (Group::Mcp, Some(server.to_owned()), None),
        None if tool.starts_with("mcp_") => (Group::Mcp, None, None),
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

/// The server and tool of an MCP tool's name, by the longest server name that fits.
fn mcp<'a>(tool: &'a str, servers: &[String]) -> Option<(&'a str, &'a str)> {
    let rest = tool.strip_prefix("mcp_")?;
    servers
        .iter()
        .map(String::as_str)
        .chain(["steery"])
        .filter_map(|server| {
            let named = rest.strip_prefix(server)?.strip_prefix('_')?;
            Some((&rest[..server.len()], named))
        })
        .filter(|(_, named)| !named.is_empty())
        .max_by_key(|(server, _)| server.len())
}

/// The group of one of Crush's own tools (`internal/agent/tools`, 0.100.0).
fn builtin(tool: &str) -> Group {
    match tool {
        "bash" | "job_output" | "job_kill" => Group::Shell,
        "view" | "ls" => Group::FilesRead,
        "edit" | "multiedit" | "write" => Group::FilesEdit,
        "glob" | "grep" | "sourcegraph" => Group::Search,
        "fetch" | "web_fetch" | "web_search" | "agentic_fetch" | "download" => Group::Web,
        "agent" | "agent_dispatch" | "agent_send" | "agent_stop" | "agent_status" => Group::Agents,
        "todos" => Group::Plan,
        "question" => Group::Ask,
        "crush_info" | "crush_logs" => Group::Harness,
        "list_mcp_resources" | "read_mcp_resource" => Group::Mcp,
        tool if tool.starts_with("lsp_") => Group::Code,
        _ => Group::Custom,
    }
}
