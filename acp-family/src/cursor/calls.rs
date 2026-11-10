//! A Cursor call as Steery reads it. Cursor writes each call twice: in the JSON message
//! that the model reads (`tool-call {toolCallId, toolName, args}`) and in the turn's
//! protobuf step. The JSON names the tool as the model calls it and holds its input. The
//! step holds the MCP server and tool, where the JSON has only `CallDynamicTool`.

use serde_json::Value;
use steery_sdk::wire::harness::{Call, Group};

/// Steery's MCP server, as the setup names it.
pub const STEERY: &str = "steery";

/// The call `id`, by its tool's `name` and `args` as the JSON message holds them, and by
/// the MCP server and tool that its step names.
pub fn call(
    id: &str,
    name: Option<&str>,
    args: Option<&Value>,
    mcp: Option<&(String, String)>,
) -> Call {
    let dynamic = name.is_some_and(|name| name.eq_ignore_ascii_case("CallDynamicTool"));
    if dynamic || mcp.is_some() {
        let text = |key| args.and_then(|args| args.get(key)).and_then(Value::as_str);
        let server = text("namespace").or(mcp.map(|mcp| mcp.0.as_str()));
        let tool = text("toolName").or(mcp.map(|mcp| mcp.1.as_str()));
        if let (Some(server), Some(tool)) = (server, tool) {
            let steery = server == STEERY;
            return Call {
                id: id.to_owned(),
                tool: format!("mcp__{server}__{tool}"),
                input: args
                    .and_then(|args| args.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Null),
                group: if steery { Group::Steery } else { Group::Mcp },
                server: (!steery).then(|| server.to_owned()),
                steery: steery.then(|| tool.to_owned()),
                asks: None,
                parent: None,
                response: None,
            };
        }
    }
    let tool = name.unwrap_or("unknown");
    Call {
        id: id.to_owned(),
        tool: tool.to_owned(),
        input: args.cloned().unwrap_or(Value::Null),
        group: group(tool),
        server: None,
        steery: None,
        asks: None,
        parent: None,
        response: None,
    }
}

/// The group of one of Cursor's own tools, by its name in any case.
fn group(tool: &str) -> Group {
    match tool.to_ascii_lowercase().as_str() {
        "shell" | "awaitshell" | "writeshellstdin" | "await" => Group::Shell,
        "read" | "ls" | "list" | "readfile" => Group::FilesRead,
        "write" | "edit" | "strreplace" | "multiedit" | "delete" | "editnotebook"
        | "applyagentdiff" | "applypatch" => Group::FilesEdit,
        "grep" | "glob" | "semanticsearch" | "semsearch" | "codebasesearch" => Group::Search,
        "websearch" | "webfetch" | "fetch" => Group::Web,
        "computeruse" => Group::Browser,
        "readlints" => Group::Code,
        "task" => Group::Agents,
        "todowrite" | "updatetodos" | "readtodos" | "createplan" | "creategoal" | "updategoal"
        | "switchmode" => Group::Plan,
        "askquestion" => Group::Ask,
        "generateimage" | "generatevideo" | "recordscreen" => Group::Media,
        "getdynamictools" | "listmcpresources" | "fetchmcpresource" => Group::Harness,
        _ => Group::Custom,
    }
}
