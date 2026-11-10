//! What a Hermes tool call is: its group, read off the tool's name in Hermes's registry
//! (`tools/*.py`, `registry.register(name=…)`), its MCP server, Steery's own tool where it
//! is one, and the questions a `clarify` puts to the person.
//!
//! Hermes names an MCP tool `mcp__<server>__<tool>`, each part with every character
//! outside `[A-Za-z0-9_]` made `_` (`tools/mcp_tool_schema.py:172-176`), and cuts a name
//! over 64 characters to a hashed tail, which keeps the server part.

use serde_json::Value;
use steery_sdk::wire::harness::{Asks, Call, Choice, Group, Question};

/// Steery's MCP server, as `hermes mcp add steery` names it.
const STEERY: &str = "steery";

/// A server's name as it stands in its tools' names.
fn sanitized(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The configured server whose tool `rest` (the name after `mcp__`) is, the longest such;
/// else what stands before the first `__`.
fn server(rest: &str, servers: &[String]) -> Option<String> {
    servers
        .iter()
        .filter(|server| {
            rest.strip_prefix(sanitized(server).as_str())
                .is_some_and(|tail| tail.len() > 2 && tail.starts_with("__"))
        })
        .max_by_key(|server| server.len())
        .cloned()
        .or_else(|| {
            rest.split_once("__")
                .filter(|(server, tool)| !server.is_empty() && !tool.is_empty())
                .map(|(server, _)| server.to_owned())
        })
}

fn group(tool: &str) -> Group {
    match tool {
        "terminal" | "process_manage" | "close_terminal" => Group::Shell,
        "read_file" => Group::FilesRead,
        "write_file" | "patch" => Group::FilesEdit,
        "search_files" => Group::Search,
        "web_search" | "web_extract" | "x_search" => Group::Web,
        "computer_use" => Group::Browser,
        "execute_code" => Group::Code,
        "delegate_task" => Group::Agents,
        "todo_list" => Group::Plan,
        "clarify" => Group::Ask,
        "memory" | "session_search" => Group::Memory,
        "skill_view" | "skills_list" | "skill_manage" => Group::Harness,
        "image_generate" | "vision_analyze" | "video_analyze" | "video_generate"
        | "text_to_speech" => Group::Media,
        _ if tool.starts_with("browser_") => Group::Browser,
        _ => Group::Custom,
    }
}

/// One call of an assistant row's `tool_calls`: `{id, function: {name, arguments}}`, the
/// arguments a JSON string.
pub fn call(entry: &Value, response: &str, servers: &[String]) -> Option<Call> {
    let id = entry.get("id").and_then(Value::as_str)?;
    let function = entry.get("function")?;
    let tool = function.get("name").and_then(Value::as_str)?;
    let input = match function.get("arguments") {
        Some(Value::String(text)) => {
            serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.clone()))
        }
        Some(other) => other.clone(),
        None => Value::Object(Default::default()),
    };
    let server = tool
        .strip_prefix("mcp__")
        .and_then(|rest| server(rest, servers));
    let group = if server.is_some() {
        Group::Mcp
    } else {
        group(tool)
    };
    let steery = match &server {
        Some(name) if sanitized(name) == STEERY => tool
            .strip_prefix("mcp__steery__")
            .filter(|tool| !tool.is_empty())
            .map(str::to_owned),
        _ => None,
    };
    let group = if steery.is_some() {
        Group::Steery
    } else {
        group
    };
    let asks = (group == Group::Ask).then(|| asks(&input)).flatten();
    Some(Call {
        id: id.to_owned(),
        tool: tool.to_owned(),
        input,
        group,
        server,
        steery,
        asks,
        parent: None,
        response: Some(response.to_owned()),
    })
}

/// `clarify`'s `questions`: each `{question, choices?, multi_select?}`.
fn asks(input: &Value) -> Option<Asks> {
    let questions: Vec<Question> = input
        .get("questions")?
        .as_array()?
        .iter()
        .filter_map(|question| {
            Some(Question {
                question: question.get("question")?.as_str()?.to_owned(),
                header: None,
                multiple: question
                    .get("multi_select")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                options: question
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
            })
        })
        .collect();
    (!questions.is_empty()).then_some(Asks::Questions(questions))
}
