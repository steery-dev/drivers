//! The Copilot driver, run as the runtime runs it, over standard input and output:
//! `initialize`, then `sessions/list` and `sessions/read` against a Copilot home of its own
//! holding two conversations, one that a person had and one that a program drove.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::Client;
use steery_sdk::{method, wire};

const ID: &str = "11111111-2222-4333-8444-555555555555";
const HEADLESS: &str = "66666666-7777-4888-9999-000000000000";

/// The driver, in an environment holding only `HOME` and `COPILOT_HOME`.
async fn start(home: &Path) -> Client {
    Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            format!("COPILOT_HOME={}", home.join("copilot").display()),
            env!("CARGO_BIN_EXE_steery-driver-copilot").into(),
        ],
        None,
    )
    .await
    .unwrap()
}

fn offer(home: &Path) -> wire::InitializeParams {
    wire::InitializeParams {
        protocol: vec![13],
        runtime: wire::Runtime {
            release: "7.0.0".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        },
        home: home.to_owned(),
        config: home.join(".config/steery"),
        settings: Default::default(),
    }
}

/// One event of `events.jsonl`, the `n`th second of the conversation.
fn event(n: u32, kind: &str, data: Value) -> String {
    json!({
        "type": kind,
        "data": data,
        "id": format!("e{n}"),
        "timestamp": format!("2026-11-10T11:00:{n:02}.000Z"),
        "parentId": if n == 0 { Value::Null } else { json!(format!("e{}", n - 1)) },
    })
    .to_string()
}

fn conversation(home: &Path, id: &str, client: &str, events: &[String]) {
    let folder = home.join("copilot/session-state").join(id);
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(
        folder.join("workspace.yaml"),
        format!(
            "id: {id}\ncwd: /work/project\nclient_name: {client}\nlast_client_name: {client}\n\
             name: 'Fix the parser''s tests'\nuser_named: false\n\
             created_at: 2026-11-10T11:00:00.000Z\nupdated_at: 2026-11-10T11:00:30.000Z\n"
        ),
    )
    .unwrap();
    std::fs::write(folder.join("events.jsonl"), events.join("\n") + "\n").unwrap();
}

/// A person's turn that ran a shell call and a sub-agent and finished, then one they
/// aborted: the turns, words, calls with their ids, the sub-agent and the tokens.
#[tokio::test]
async fn copilot_reads_turns_words_calls_and_an_abort() {
    let home = std::env::temp_dir().join(format!("steery-copilot-read-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let events = [
        event(
            0,
            "session.start",
            json!({
                "sessionId": ID, "version": 1, "producer": "copilot-agent", "copilotVersion": "1.0.108",
                "startTime": "2026-11-10T11:00:00.000Z", "context": {"cwd": "/work/project"},
            }),
        ),
        event(
            1,
            "session.model_change",
            json!({"newModel": "claude-sonnet-5.5"}),
        ),
        event(
            2,
            "user.message",
            json!({
                "content": "Fix the tests", "transformedContent": "<x>Fix the tests</x>",
                "messageId": "m1", "interactionId": "i1", "turnId": "0",
            }),
        ),
        event(
            3,
            "assistant.turn_start",
            json!({"turnId": "0", "interactionId": "i1"}),
        ),
        event(
            4,
            "assistant.message",
            json!({
                "messageId": "a1", "content": "Running them.", "apiCallId": "r1", "turnId": "0",
                "toolRequests": [
                    {"toolCallId": "call_sh", "name": "bash", "arguments": {"command": "cargo test"}},
                    {"toolCallId": "call_task", "name": "task", "arguments": {"prompt": "Read the parser"}},
                ],
            }),
        ),
        event(
            5,
            "tool.execution_start",
            json!({
                "toolCallId": "call_sh", "toolName": "bash", "arguments": {"command": "cargo test"}, "turnId": "0",
            }),
        ),
        event(
            6,
            "tool.execution_complete",
            json!({
                "toolCallId": "call_sh", "success": true, "turnId": "0",
                "result": {"content": "test result: ok\n<shellId: 0 completed with exit code 0>"},
            }),
        ),
        event(
            7,
            "tool.execution_start",
            json!({
                "toolCallId": "call_task", "toolName": "task", "arguments": {"prompt": "Read the parser"},
            }),
        ),
        event(
            8,
            "subagent.started",
            json!({
                "toolCallId": "call_task", "agentName": "explore", "agentDisplayName": "Explore",
                "agentDescription": "Explores code",
            }),
        ),
        event(
            9,
            "assistant.message",
            json!({
                "messageId": "s1", "content": "The parser is fine.", "parentToolCallId": "call_task",
            }),
        ),
        event(
            10,
            "subagent.completed",
            json!({
                "toolCallId": "call_task", "agentName": "explore", "agentDisplayName": "Explore",
            }),
        ),
        event(
            11,
            "tool.execution_complete",
            json!({
                "toolCallId": "call_task", "success": true, "result": {"content": "The parser is fine."},
            }),
        ),
        event(
            12,
            "session.usage_record",
            json!({"usage": {
                "model": "claude-sonnet-5.5", "inputTokens": 100, "outputTokens": 20,
                "cacheReadTokens": 80, "cacheWriteTokens": 10, "reasoningTokens": 0, "apiCallId": "r1",
            }}),
        ),
        event(13, "assistant.turn_end", json!({"turnId": "0"})),
        event(
            14,
            "assistant.turn_start",
            json!({"turnId": "1", "interactionId": "i1"}),
        ),
        event(
            15,
            "assistant.message",
            json!({"messageId": "a2", "content": "All green.", "toolRequests": []}),
        ),
        event(16, "assistant.turn_end", json!({"turnId": "1"})),
        event(
            17,
            "user.message",
            json!({"content": "Now refactor", "interactionId": "i2"}),
        ),
        event(
            18,
            "assistant.turn_start",
            json!({"turnId": "0", "interactionId": "i2"}),
        ),
        event(19, "abort", json!({"reason": "user_initiated"})),
        event(20, "assistant.turn_end", json!({"turnId": "0"})),
        event(21, "session.shutdown", json!({})),
    ];
    conversation(&home, ID, "github/cli", &events);
    conversation(&home, HEADLESS, "sdk", &events[..3]);

    let client = start(&home).await;
    client.initialize(&offer(&home)).await.unwrap();

    let listed = client
        .call(method::SESSIONS_LIST, json!({}), Duration::from_secs(30))
        .await
        .unwrap();
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{listed}");
    let session = &sessions[0];
    assert_eq!(session["external_id"], ID);
    assert_eq!(session["title"], "Fix the parser's tests");
    assert_eq!(session["cwd"], "/work/project");
    assert_eq!(session["sub_agents"][0]["external_id"], "call_task");
    assert_eq!(session["sub_agents"][0]["task"], "Read the parser");
    assert_eq!(listed["excluded"], json!([HEADLESS]));

    let read = client
        .call(
            method::SESSIONS_READ,
            json!({"external_id": ID}),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    let said: Vec<String> = read["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            let kind = event["type"].as_str().unwrap();
            match kind {
                "words" => format!(
                    "words {} {}",
                    event["voice"].as_str().unwrap(),
                    event["text"].as_str().unwrap()
                ),
                "call_began" => format!(
                    "call_began {} {}",
                    event["call"]["id"].as_str().unwrap(),
                    event["call"]["group"].as_str().unwrap()
                ),
                "call_ended" => format!(
                    "call_ended {} {}",
                    event["id"].as_str().unwrap(),
                    event["ok"]
                ),
                "turn_ended" => format!("turn_ended {}", event["state"].as_str().unwrap()),
                "sub_agent" => format!(
                    "sub_agent {} {}",
                    event["external_id"].as_str().unwrap(),
                    event["state"].as_str().unwrap()
                ),
                "model" => format!("model {}", event["model"].as_str().unwrap()),
                "tokens" => format!("tokens {}", event["input"]),
                other => other.to_owned(),
            }
        })
        .collect();
    assert_eq!(
        said,
        [
            "model claude-sonnet-5.5",
            "turn_began",
            "words person Fix the tests",
            "words agent Running them.",
            "call_began call_sh shell",
            "call_ended call_sh true",
            "words tool test result: ok\n<shellId: 0 completed with exit code 0>",
            "call_began call_task agents",
            "sub_agent call_task running",
            "sub_agent call_task finished",
            "call_ended call_task true",
            "words tool The parser is fine.",
            "tokens 100",
            "words agent All green.",
            "turn_ended finished",
            "turn_began",
            "words person Now refactor",
            "turn_ended interrupted",
        ],
        "{read}"
    );
    assert_eq!(read["whole"], true);
    assert_eq!(read["after"], "e21");

    let again = client
        .call(
            method::SESSIONS_READ,
            json!({"external_id": ID, "cursor": read["cursor"]}),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    assert_eq!(again["events"], json!([]), "{again}");

    let agent = client
        .call(
            method::SESSIONS_READ,
            json!({"external_id": ID, "sub_agent": "call_task"}),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    let kinds: Vec<&str> = agent["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["turn_began", "words", "words", "turn_ended"],
        "{agent}"
    );
    assert_eq!(agent["events"][1]["text"], "Read the parser");
    assert_eq!(agent["events"][2]["text"], "The parser is fine.");
    assert_eq!(
        agent["events"].as_array().unwrap().last().unwrap()["state"],
        "finished"
    );

    let missing = client
        .call(
            method::SESSIONS_READ,
            json!({"external_id": "nope"}),
            Duration::from_secs(30),
        )
        .await;
    assert!(missing.is_err());

    // Drive. `held`: a lock counts only while the process it names runs.
    let folder = home.join("copilot/session-state").join(ID);
    let live = folder.join(format!("inuse.{}.lock", std::process::id()));
    std::fs::write(&live, "").unwrap();
    let call = |method: &'static str, params: Value| {
        let client = &client;
        async move {
            client
                .call(method, params, Duration::from_secs(30))
                .await
                .unwrap()
        }
    };
    let held = call(method::HELD, json!({"external_id": ID})).await;
    assert_eq!(
        held["person"]["stop"]["process"]["pid"],
        std::process::id(),
        "{held}"
    );
    std::fs::remove_file(&live).unwrap();
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    gone.wait().unwrap();
    std::fs::write(folder.join(format!("inuse.{}.lock", gone.id())), "").unwrap();
    let held = call(method::HELD, json!({"external_id": ID})).await;
    assert!(held.get("look").is_some(), "{held}");

    let idle = call(method::IDLE, json!({"external_id": ID})).await;
    assert_eq!(idle["sample"]["state"], "interrupted", "{idle}");
    assert_eq!(idle["sample"]["model"], "claude-sonnet-5.5", "{idle}");

    // `turn/read` of a continue: the turn mode's own line names the conversation, Copilot's
    // `user.message` carries the marker, and its `result` closes the turn.
    let marker = "steery-delivery:t.1";
    let lines = [
        json!({"steery": {"named": ID}}),
        json!({"type": "user.message", "id": "r1", "parentId": "e21",
               "timestamp": "2026-11-10T11:01:00.000Z",
               "data": {"content": format!("Again. {marker}"), "interactionId": "i3"}}),
        json!({"type": "assistant.turn_start", "id": "r2", "parentId": "r1",
               "timestamp": "2026-11-10T11:01:01.000Z",
               "data": {"turnId": "0", "interactionId": "i3"}}),
        json!({"type": "assistant.message", "id": "r3", "parentId": "r2",
               "timestamp": "2026-11-10T11:01:02.000Z",
               "data": {"messageId": "a3", "content": "again", "toolRequests": []}}),
        json!({"type": "assistant.turn_end", "id": "r4", "parentId": "r3",
               "timestamp": "2026-11-10T11:01:03.000Z", "data": {"turnId": "0"}}),
        json!({"type": "result", "sessionId": ID, "exitCode": 0}),
    ];
    let mut read = Vec::new();
    for line in &lines {
        read.push(
            call(
                method::TURN_READ,
                json!({"turn": "t", "stream": "stdout", "line": line.to_string(),
                       "delivery": marker}),
            )
            .await,
        );
    }
    assert_eq!(read[0]["named"], ID, "{read:?}");
    assert_eq!(read[1]["took"], true, "{read:?}");
    let words: Vec<Value> = read
        .iter()
        .flat_map(|line| line["events"].as_array().cloned().unwrap_or_default())
        .filter(|event| event["type"] == "words")
        .map(|event| event["text"].clone())
        .collect();
    assert_eq!(words, [json!(format!("Again. {marker}")), json!("again")]);
    let ended: Vec<&Value> = read.iter().filter_map(|line| line.get("ended")).collect();
    assert_eq!(ended.len(), 1, "{read:?}");
    assert_eq!(ended[0]["state"], "finished");

    let refused = call(
        method::TURN_READ,
        json!({"turn": "u", "stream": "stderr",
               "line": "Error: Model \"gpt-0\" from --model flag is not available."}),
    )
    .await;
    assert_eq!(refused["refused"], "model-missing", "{refused}");

    let terminal = call(
        method::TERMINAL,
        json!({"external_id": ID, "cwd": "/work/project"}),
    )
    .await;
    assert_eq!(
        terminal["command"],
        format!(
            "cd '/work/project' && COPILOT_HOME='{}' copilot --ui-server --resume={ID}",
            home.join("copilot").display()
        ),
        "{terminal}"
    );

    let _ = std::fs::remove_dir_all(&home);
}
