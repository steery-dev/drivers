//! Warp's task records: each row of `agent_tasks` is one `warp.multi_agent.v1.Task`,
//! encoded with Protocol Buffers. A task has an id (1), a description (2), its parent's id
//! (3.1, empty for the conversation's root task) and its messages (5), in the order Warp
//! appended them. A message has an id (1), its request's id (13) and a timestamp (14), and
//! one body: the person's query (2), the agent's words (3), a tool call (4) and its result
//! (5), a system query (9), the agent's reasoning (15), a summary (16), a skill the person
//! invoked (23), the model (25), and the server's metadata of a request (30). The field
//! numbers are Warp's wire contract (`apis/multi_agent/v1/task.proto`).

use base64::Engine;
use serde_json::{Value, json};
use steery_sdk::wire::harness::{Asks, Choice, Clip, Group, Question};

use crate::pb::{Msg, Wire, structure};

pub(crate) struct Task {
    pub id: String,
    /// The task that started it; none for the conversation's root.
    pub parent: Option<String>,
    pub description: String,
    pub messages: Vec<Message>,
}

pub(crate) struct Message {
    pub id: String,
    /// The request that wrote it. One request is one call of Warp's server; a turn is
    /// one or more.
    pub request: String,
    /// Milliseconds since 1970.
    pub at: Option<u64>,
    /// Its encoded length.
    pub bytes: usize,
    pub body: Body,
}

pub(crate) enum Body {
    /// The person's words, the folder they were said in, and the images they carried.
    Query {
        text: String,
        cwd: Option<String>,
        clips: Vec<Clip>,
    },
    /// Warp asked on its own: a resumed conversation, a code diff, a summary.
    System {
        cwd: Option<String>,
    },
    Output(String),
    Reasoning(String),
    Call(Call),
    Result(Outcome),
    /// A summary of the history before it; none where it summed up a tool's result.
    Summary(Option<String>),
    Model(String),
    Request(Request),
    /// Anything the driver does not read.
    Other,
}

/// A tool call as the driver reads it.
pub(crate) struct Call {
    pub id: String,
    /// The tool, as `task.proto` names its field: `run_shell_command`.
    pub tool: String,
    pub input: Value,
    pub group: Group,
    /// The MCP server, for an MCP tool.
    pub server: Option<String>,
    pub asks: Option<Asks>,
    /// The task a `subagent` call started.
    pub task: Option<String>,
}

/// A tool call's result.
pub(crate) struct Outcome {
    pub call: String,
    pub ok: bool,
    pub text: String,
}

/// The server's record of how a request ended.
pub(crate) struct Request {
    pub outcome: Ended,
    /// Tokens by model: what it read, read from a cache, wrote to a cache, and wrote.
    pub tokens: Vec<(String, [u64; 4])>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ended {
    Unspecified,
    Completed,
    Canceled,
    Errored,
}

/// A task record; none where it is not one.
pub(crate) fn task(bytes: &[u8]) -> Option<Task> {
    let task = Msg::new(bytes);
    let id = task.str(1).to_owned();
    if id.is_empty() {
        return None;
    }
    Some(Task {
        id,
        parent: task
            .msg(3)
            .map(|dependencies| dependencies.str(1).to_owned())
            .filter(|parent| !parent.is_empty()),
        description: task.str(2).to_owned(),
        messages: task
            .all(5)
            .filter_map(|value| match value {
                Wire::Bytes(bytes) => Some(message(bytes)),
                _ => None,
            })
            .collect(),
    })
}

/// The fields of `Message`'s `oneof message` (`task.proto`). Its other fields (`task_id` 11,
/// `request_id` 13, `timestamp` 14, `fetched_memories` 29) sit between them.
const BODIES: [u32; 22] = [
    2, 3, 4, 5, 6, 9, 10, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 30,
];

fn message(bytes: &[u8]) -> Message {
    let message = Msg::new(bytes);
    let body = match message.oneof(|n| BODIES.contains(&n)) {
        Some((2, Wire::Bytes(query))) => query_body(Msg::new(query)),
        Some((3, Wire::Bytes(output))) => Body::Output(Msg::new(output).str(1).to_owned()),
        Some((4, Wire::Bytes(call))) => Body::Call(tool_call(Msg::new(call))),
        Some((5, Wire::Bytes(result))) => Body::Result(outcome(Msg::new(result))),
        Some((9, Wire::Bytes(system))) => Body::System {
            cwd: cwd(Msg::new(system).msg(2)),
        },
        Some((15, Wire::Bytes(reasoning))) => {
            Body::Reasoning(Msg::new(reasoning).str(1).to_owned())
        }
        Some((16, Wire::Bytes(summary))) => Body::Summary(
            Msg::new(summary)
                .msg(1)
                .map(|conversation| conversation.str(1).to_owned()),
        ),
        Some((23, Wire::Bytes(skill))) => Msg::new(skill).msg(2).map_or(Body::Other, query_body),
        Some((25, Wire::Bytes(model))) => Body::Model(Msg::new(model).str(1).to_owned()),
        Some((30, Wire::Bytes(metadata))) => Body::Request(request(Msg::new(metadata))),
        _ => Body::Other,
    };
    Message {
        id: message.str(1).to_owned(),
        request: message.str(13).to_owned(),
        at: message.millis(14),
        bytes: bytes.len(),
        body,
    }
}

/// A `UserQuery`: its text (1) and its context (2).
fn query_body(query: Msg) -> Body {
    let context = query.msg(2);
    let clips = context
        .map(|context| {
            context
                .msgs(7)
                .filter_map(|image| {
                    let data = image.bytes(1).filter(|data| !data.is_empty())?;
                    Some(Clip::Bytes {
                        data: base64::engine::general_purpose::STANDARD.encode(data),
                        media_type: Some(image.str(2).to_owned()).filter(|t| !t.is_empty()),
                        name: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Body::Query {
        text: query.str(1).to_owned(),
        cwd: cwd(context),
        clips,
    }
}

/// The working folder of an `InputContext`: `directory.pwd`.
fn cwd(context: Option<Msg>) -> Option<String> {
    context?
        .msg(1)
        .map(|directory| directory.str(1).to_owned())
        .filter(|pwd| !pwd.is_empty())
}

/// The tools of `ToolCall`'s `oneof tool`, by field number: Warp's name for each, and its
/// group.
fn tool(number: u32) -> Option<(&'static str, Group)> {
    Some(match number {
        2 => ("run_shell_command", Group::Shell),
        3 => ("search_codebase", Group::Search),
        4 => ("server", Group::Custom),
        5 => ("read_files", Group::FilesRead),
        6 => ("apply_file_diffs", Group::FilesEdit),
        7 => ("suggest_plan", Group::Plan),
        8 => ("suggest_create_plan", Group::Plan),
        9 => ("grep", Group::Search),
        10 => ("file_glob", Group::Search),
        11 => ("read_mcp_resource", Group::Mcp),
        12 => ("call_mcp_tool", Group::Mcp),
        13 => ("write_to_long_running_shell_command", Group::Shell),
        14 => ("suggest_new_conversation", Group::Harness),
        15 => ("file_glob_v2", Group::Search),
        16 => ("suggest_prompt", Group::Harness),
        17 => ("open_code_review", Group::Code),
        18 => ("init_project", Group::Harness),
        19 => ("subagent", Group::Agents),
        20 => ("read_documents", Group::Custom),
        21 => ("edit_documents", Group::Custom),
        22 => ("create_documents", Group::Custom),
        23 => ("read_shell_command_output", Group::Shell),
        24 => ("use_computer", Group::Custom),
        25 => ("insert_review_comments", Group::Code),
        26 => ("read_skill", Group::Harness),
        27 => ("request_computer_use", Group::Custom),
        28 => ("fetch_conversation", Group::Agents),
        30 => ("send_message_to_agent", Group::Agents),
        31 => ("transfer_shell_command_control_to_user", Group::Shell),
        32 => ("ask_user_question", Group::Ask),
        34 => ("upload_file_artifact", Group::Custom),
        35 => ("run_agents", Group::Agents),
        36 => ("wait_for_events", Group::Agents),
        37 => ("start_recording", Group::Media),
        38 => ("stop_recording", Group::Media),
        _ => return None,
    })
}

fn tool_call(call: Msg) -> Call {
    let id = call.str(1).to_owned();
    let Some((number, Wire::Bytes(bytes))) = call.oneof(|n| tool(n).is_some()) else {
        return Call {
            id,
            tool: "unknown".into(),
            input: Value::Null,
            group: Group::Custom,
            server: None,
            asks: None,
            task: None,
        };
    };
    let (name, group) = tool(number).unwrap_or(("unknown", Group::Custom));
    let args = Msg::new(bytes);
    let strs = |n| args.strs(n).collect::<Vec<_>>();
    let mut server = None;
    let mut asks = None;
    let mut task = None;
    let mut tool = name.to_owned();
    // The input of each tool the driver reads; any other's is `{}`.
    let input = match number {
        2 => json!({"command": args.str(1)}),
        3 => json!({"query": args.str(1), "path_filters": strs(2), "codebase_path": args.str(3)}),
        5 => json!({"files": args.msgs(1).map(|file| file.str(1)).collect::<Vec<_>>()}),
        6 => {
            let mut files: Vec<&str> = Vec::new();
            for field in [2, 3, 4, 5] {
                files.extend(args.msgs(field).map(|file| file.str(1)));
            }
            json!({"summary": args.str(1), "files": files})
        }
        9 => json!({"queries": strs(1), "path": args.str(2)}),
        10 => json!({"patterns": strs(1), "path": args.str(2)}),
        15 => json!({"patterns": strs(1), "search_dir": args.str(2)}),
        11 | 12 => {
            // A server by the name the person gave it, else Warp's id for it.
            let named = args
                .msg(if number == 11 { 3 } else { 4 })
                .map(|identity| identity.str(1))
                .filter(|name| !name.is_empty())
                .unwrap_or(args.str(if number == 11 { 2 } else { 3 }));
            server = Some(named.to_owned()).filter(|name| !name.is_empty());
            if number == 12 {
                tool = format!(
                    "mcp__{}__{}",
                    server.as_deref().unwrap_or_default(),
                    args.str(1)
                );
                args.msg(2).map(structure).unwrap_or_else(|| json!({}))
            } else {
                json!({"uri": args.str(1)})
            }
        }
        13 => json!({"input": String::from_utf8_lossy(args.bytes(1).unwrap_or_default())}),
        19 => {
            task = Some(args.str(1).to_owned()).filter(|id| !id.is_empty());
            json!({"task_id": args.str(1), "payload": args.str(2)})
        }
        26 => json!({"name": args.str(3), "skill_path": args.str(1)}),
        32 => {
            let questions: Vec<Question> = args
                .msgs(1)
                .map(|question| {
                    let choice = question.msg(3);
                    Question {
                        question: question.str(2).to_owned(),
                        header: None,
                        multiple: choice.is_some_and(|choice| choice.bool(3)),
                        options: choice
                            .into_iter()
                            .flat_map(|choice| choice.msgs(1))
                            .map(|option| Choice {
                                label: option.str(1).to_owned(),
                                description: None,
                                preview: None,
                            })
                            .collect(),
                    }
                })
                .collect();
            let input =
                json!({"questions": questions.iter().map(|q| &q.question).collect::<Vec<_>>()});
            asks = Some(Asks::Questions(questions));
            input
        }
        _ => json!({}),
    };
    Call {
        id,
        tool,
        input,
        group,
        server,
        asks,
        task,
    }
}

/// The results whose message is `oneof { Success success = 1; Error error = 2; }`, the
/// error's `message` 1, by their field number in `ToolCallResult`.
const SUCCESS_OR_ERROR: [u32; 9] = [3, 5, 6, 9, 15, 16, 19, 30, 36];

fn outcome(result: Msg) -> Outcome {
    let call = result.str(1).to_owned();
    let (ok, text) = match result.oneof(|n| (2..=43).contains(&n) && n != 11) {
        Some((2, Wire::Bytes(shell))) => shell_result(Msg::new(shell)),
        // `cancel`: the person stopped it.
        Some((14, _)) => (false, String::new()),
        Some((16, Wire::Bytes(mcp))) => {
            let mcp = Msg::new(mcp);
            match mcp.msg(2) {
                Some(error) => (false, error.str(1).to_owned()),
                None => {
                    let texts: Vec<&str> = mcp
                        .msg(1)
                        .into_iter()
                        .flat_map(|success| success.msgs(1))
                        .filter_map(|part| part.msg(1))
                        .map(|text| text.str(1))
                        .collect();
                    (true, texts.join("\n"))
                }
            }
        }
        Some((23, Wire::Bytes(subagent))) => (true, Msg::new(subagent).str(1).to_owned()),
        // `denied_by_hook`.
        Some((43, _)) => (false, String::new()),
        Some((number, Wire::Bytes(bytes))) if SUCCESS_OR_ERROR.contains(&number) => {
            match Msg::new(bytes).msg(2) {
                Some(error) => (false, error.str(1).to_owned()),
                None => (true, String::new()),
            }
        }
        _ => (true, String::new()),
    };
    Outcome { call, ok, text }
}

/// A `RunShellCommandResult`: finished (5) with its output and exit code, refused (6) or
/// the terminal busy (7), or still running (4). Older records kept the output (1) and the
/// exit code (2) on the result itself.
fn shell_result(shell: Msg) -> (bool, String) {
    match shell.oneof(|n| (4..=7).contains(&n)) {
        Some((5, Wire::Bytes(finished))) => {
            let finished = Msg::new(finished);
            (finished.int32(2) == 0, finished.str(1).to_owned())
        }
        Some((6 | 7, _)) => (false, String::new()),
        Some(_) => (true, String::new()),
        None => (shell.int32(2) == 0, shell.str(1).to_owned()),
    }
}

/// A `RequestMetadata`: its charges (2) and its outcome (4).
fn request(metadata: Msg) -> Request {
    let outcome = match metadata.varint(4) {
        1 => Ended::Completed,
        2 => Ended::Canceled,
        3 => Ended::Errored,
        _ => Ended::Unspecified,
    };
    let mut tokens: Vec<(String, [u64; 4])> = Vec::new();
    // `RequestCharges.usage_by_category`: a map of `ChargedUsage`, whose maps 1, 2, 3 and 7
    // give each model's `InferenceUsage`, its `token_count` 1.
    let usages = metadata
        .msg(2)
        .into_iter()
        .flat_map(|charges| charges.msgs(1))
        .filter_map(|category| category.msg(2))
        .flat_map(|charged| [1, 2, 3, 7].into_iter().flat_map(move |n| charged.msgs(n)));
    for usage in usages {
        let model = usage.str(1);
        let Some(count) = usage.msg(2).and_then(|inference| inference.msg(1)) else {
            continue;
        };
        let counted = [
            count.varint(1),
            count.varint(3),
            count.varint(4),
            count.varint(2),
        ];
        match tokens.iter_mut().find(|(m, _)| m == model) {
            Some((_, sum)) => {
                for (total, n) in sum.iter_mut().zip(counted) {
                    *total += n;
                }
            }
            None => tokens.push((model.to_owned(), counted)),
        }
    }
    Request { outcome, tokens }
}
