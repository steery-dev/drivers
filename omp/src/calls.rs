//! What an omp tool call is: its tool as Steery spells it, its group, Steery's own tool
//! where it is one, and the questions an `ask` puts to the person.
//!
//! The groups are Steery's (`steery-types` `calls::tool_group`), read off a tool's name
//! alone, never its arguments: omp's own tools by name, a per-conversation rename
//! `<word>_<tool>` as the tool it renames, an MCP tool by its server, and a `write` to an
//! `xd://` device as the device's tool, which is how omp mounts its MCP tools as files.

use std::path::Path;

use serde_json::Value;
use steery_sdk::wire::harness::{Asks, Call, Choice, Group, Question};

/// Steery's MCP server, as setup names it in omp's `mcp.json`.
const STEERY: &str = "steery";

/// The MCP servers omp's own `mcp.json` names: where omp's `mcp__<server>_<tool>` ends its
/// server's name, which nothing in the tool's name marks.
pub fn servers(root: &Path) -> Vec<String> {
    let mut servers: Vec<String> = std::fs::read(root.join("mcp.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|config| match config.get("mcpServers") {
            Some(Value::Object(servers)) => Some(servers.keys().cloned().collect()),
            _ => None,
        })
        .unwrap_or_default();
    if !servers.iter().any(|server| server == STEERY) {
        servers.push(STEERY.to_owned());
    }
    servers
}

/// One call as omp wrote it: `toolCall {id, name, arguments}` in the assistant message
/// `response`.
pub fn call(id: &str, name: &str, input: Value, response: &str, servers: &[String]) -> Call {
    let tool = spelled(name, servers);
    // omp's `write` to `xd://<tool>` invokes that tool: the call is the device's.
    let device = renames(&name.to_lowercase(), "write")
        .then(|| input.get("path").and_then(Value::as_str))
        .flatten()
        .and_then(|path| path.strip_prefix("xd://"))
        .map(|device| spelled(device, servers));
    let invoked = device.as_deref().unwrap_or(&tool);
    let (group, server) = group(invoked);
    let steery = invoked
        .strip_prefix("mcp__steery__")
        .filter(|tool| !tool.is_empty() && !tool.starts_with('_'))
        .map(str::to_owned);
    let asks = (group == Group::Ask && base(&invoked.to_lowercase()) == "ask")
        .then(|| asks(&input))
        .flatten();
    Call {
        id: id.to_owned(),
        tool,
        input,
        group,
        server,
        steery,
        asks,
        parent: None,
        response: Some(response.to_owned()),
    }
}

/// A tool as Steery spells it: omp's `mcp__<server>_<tool>` as `mcp__<server>__<tool>`
/// where the server is one omp's `mcp.json` names, the longest such; any other name as
/// omp wrote it.
fn spelled(name: &str, servers: &[String]) -> String {
    let Some(rest) = name.strip_prefix("mcp__") else {
        return name.to_owned();
    };
    if rest.contains("__") {
        return name.to_owned();
    }
    servers
        .iter()
        .flat_map(|server| [server.clone(), server.replace(['-', '.'], "_")])
        .filter(|server| {
            !server.is_empty()
                && rest
                    .strip_prefix(server.as_str())
                    .and_then(|tail| tail.strip_prefix('_'))
                    .is_some_and(|tool| !tool.is_empty())
        })
        .max_by_key(String::len)
        .map_or_else(
            || name.to_owned(),
            |server| format!("mcp__{server}__{}", &rest[server.len() + 1..]),
        )
}

/// The group of the tool a call invokes, and its MCP server where the group is `mcp`.
fn group(tool: &str) -> (Group, Option<String>) {
    let lowered = tool.to_lowercase();
    let Some(rest) = lowered.strip_prefix("mcp__") else {
        return (plain(&lowered), None);
    };
    let mut from = lowered.len() - rest.len();
    let mut rest = rest;
    while let Some(inner) = rest.strip_prefix("mcp__") {
        rest = inner;
        from += "mcp__".len();
    }
    // The server and the tool, lowercased, and where the server sits in `tool`.
    let (server, inner) = match rest.split_once("__") {
        Some((server, inner)) => (server, inner),
        // omp's `mcp__<server>_<tool>` of a server its `mcp.json` does not name: a kind
        // Steery knows by name, else up to the first `_`.
        None => KINDS
            .iter()
            .flat_map(|(_, names)| names.iter())
            .filter(|known| {
                rest.strip_prefix(**known)
                    .is_some_and(|tail| tail.starts_with('_'))
            })
            .max_by_key(|known| known.len())
            .map(|known| (&rest[..known.len()], &rest[known.len() + 1..]))
            .unwrap_or_else(|| rest.split_once('_').unwrap_or((rest, ""))),
    };
    let named = tool
        .get(from..from + server.len())
        .filter(|named| named.eq_ignore_ascii_case(server))
        .unwrap_or(server);
    served(server, inner, named)
}

/// A tool an MCP server offers, by the server's lowercased name and the tool's; `named` is
/// the server as omp spells it.
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
        Some(Group::Harness) => (own(tool).unwrap_or(Group::Harness), None),
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

/// A tool no MCP server offers, by its lowercased name.
fn plain(name: &str) -> Group {
    builtin(name)
        .or_else(|| name.starts_with("steery_").then_some(Group::Steery))
        .or_else(|| own(name))
        .or_else(|| {
            let first = name.split(['_', '-', '.']).next().unwrap_or(name);
            BROWSER_WORDS.contains(&first).then_some(Group::Browser)
        })
        .unwrap_or(Group::Custom)
}

/// A harness's own tool, renamed or not, by its lowercased name.
fn own(name: &str) -> Option<Group> {
    builtin(name).or_else(|| renamed(name).and_then(builtin))
}

/// The tool a lowercased name is, a per-conversation rename read as what it renames.
fn base(name: &str) -> &str {
    if builtin(name).is_some() {
        name
    } else {
        renamed(name).unwrap_or(name)
    }
}

fn builtin(name: &str) -> Option<Group> {
    BUILTINS
        .iter()
        .find(|(_, tools)| tools.contains(&name))
        .map(|(group, _)| *group)
}

/// Whether a lowercased tool's name is `builtin`, or a rename of it.
fn renames(tool: &str, builtin: &str) -> bool {
    tool == builtin || renamed(tool) == Some(builtin)
}

/// What a per-conversation rename `<word>_<tool>` renames: the tool after a word of at
/// least four lowercase letters. A shorter prefix is a name's own part (`db_read`).
fn renamed(tool: &str) -> Option<&str> {
    let (word, rest) = tool.split_once('_')?;
    (word.len() >= 4 && word.chars().all(|c| c.is_ascii_lowercase())).then_some(rest)
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

/// omp's `ask`: `{questions: [{id, question, options, multi}]}` (the array as JSON text
/// too), or one question as `{question, options, multi}`; an option is its label, or
/// `{label, description}`.
fn asks(input: &Value) -> Option<Asks> {
    let listed = match input.get("questions") {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).ok(),
        Some(questions) => Some(questions.clone()),
        None => None,
    };
    let mut questions: Vec<Question> = listed
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(question)
        .collect();
    if questions.is_empty() {
        questions.extend(question(input));
    }
    (!questions.is_empty()).then_some(Asks::Questions(questions))
}

fn question(asked: &Value) -> Option<Question> {
    let question = asked.get("question")?.as_str()?.to_owned();
    let options = asked
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| match option {
            Value::String(label) => Some(Choice {
                label: label.clone(),
                description: None,
                preview: None,
            }),
            Value::Object(fields) => Some(Choice {
                label: fields.get("label")?.as_str()?.to_owned(),
                description: fields
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                preview: None,
            }),
            _ => None,
        })
        .collect();
    Some(Question {
        question,
        header: None,
        multiple: asked.get("multi").and_then(Value::as_bool) == Some(true),
        options,
    })
}

/// The harnesses' own tools by name, lowercased, each with its group (`steery-types`
/// `calls::BUILTINS`).
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
/// (`steery-types` `calls::KINDS`). A server of `harness` is one a harness ships for itself.
#[rustfmt::skip]
const KINDS: &[(Group, &[&str])] = &[
    (Group::Steery, &["steery"]),
    (Group::Browser, &[
        "chrome", "chrome_devtools", "claude_browser", "browser", "cua_repl", "cua", "playwright",
        "puppeteer",
    ]),
    (Group::Code, &["node_repl", "js_repl", "python", "visualize", "jupyter"]),
    (Group::Shell, &["terminal"]),
    (Group::Harness, &["codex_app", "ccd_session", "ccd_session_mgmt", "ccd_view", "ccd_host"]),
];

/// The first words that say a tool no MCP server offers drives a browser.
const BROWSER_WORDS: &[&str] = &["browser", "chrome", "playwright", "puppeteer"];
