//! The driver, run as the runtime runs it, over standard input and output: `initialize`,
//! then `hook/read` and `hook/answer` on documents shaped as Copilot 1.0.108 hands its
//! camelCase hooks: a call denied at `preToolUse`, one allowed at `permissionRequest`, and a
//! paragraph at `sessionStart`.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::Client;
use steery_sdk::{method, wire};

const SESSION: &str = "7611d97f-6a5a-4f07-ae12-72d18b4d52e";

/// The driver, in an environment holding only `HOME`.
async fn start(home: &Path) -> Client {
    Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            env!("CARGO_BIN_EXE_steery-driver-copilot").into(),
        ],
        None,
    )
    .await
    .unwrap()
}

fn offer(home: &Path) -> wire::InitializeParams {
    wire::InitializeParams {
        protocol: vec![3, 4, 5],
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

async fn call(client: &Client, method: &str, params: Value) -> Value {
    client
        .call(method, params, Duration::from_secs(10))
        .await
        .unwrap()
}

async fn read(client: &Client, event: &str, document: &Value) -> Value {
    call(
        client,
        method::HOOK_READ,
        json!({"event": event, "document": document.to_string()}),
    )
    .await["moment"]
        .clone()
}

async fn answer(client: &Client, event: &str, document: &Value, answer: Value) -> Value {
    call(
        client,
        method::HOOK_ANSWER,
        json!({"event": event, "document": document.to_string(), "answer": answer}),
    )
    .await
}

#[tokio::test]
async fn copilot_reads_its_hooks_and_prints_steerys_answers() {
    let home = std::env::temp_dir().join(format!("steery-copilot-hooks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".copilot/session-state")).unwrap();
    let driver = start(&home).await;
    let initialized = driver.initialize(&offer(&home)).await.unwrap();
    let hooks = serde_json::to_value(&initialized.harness.unwrap().hooks).unwrap();
    let events: Vec<&str> = hooks
        .as_array()
        .unwrap()
        .iter()
        .map(|hook| hook["event"].as_str().unwrap())
        .collect();
    assert!(events.contains(&"preToolUse") && events.contains(&"permissionRequest"));

    let before = json!({
        "sessionId": SESSION,
        "timestamp": 1794310753515u64,
        "cwd": "/private/tmp/cph/work",
        "toolCallId": "toolu_01MXJP6a8pb4x67wEZCiLLKa",
        "toolName": "bash",
        "toolArgs": {"command": "rm -rf /", "description": "Remove everything"},
    });
    let moment = read(&driver, "preToolUse", &before).await;
    assert_eq!(moment["external_id"], SESSION);
    assert_eq!(moment["at"], 1794310753515u64);
    assert_eq!(moment["cwd"], "/private/tmp/cph/work");
    assert_eq!(moment["call"]["id"], "toolu_01MXJP6a8pb4x67wEZCiLLKa");
    assert_eq!(moment["call"]["tool"], "bash");
    assert_eq!(moment["call"]["group"], "shell");
    assert_eq!(moment["call"]["input"]["command"], "rm -rf /");
    let denied = answer(
        &driver,
        "preToolUse",
        &before,
        json!({"decision": {"deny": {"reason": "Not on this machine"}}}),
    )
    .await;
    assert_eq!(denied["exit_code"], 0);
    assert_eq!(
        serde_json::from_str::<Value>(denied["stdout"].as_str().unwrap()).unwrap(),
        json!({"permissionDecision": "deny", "permissionDecisionReason": "Not on this machine"})
    );

    let permission = json!({
        "hookName": "permissionRequest",
        "sessionId": SESSION,
        "timestamp": 1794310753600u64,
        "cwd": "/private/tmp/cph/work",
        "toolName": "steery-sessions_describe",
        "toolInput": {"text": "Reading"},
        "permissionSuggestions": [],
    });
    let moment = read(&driver, "permissionRequest", &permission).await;
    assert_eq!(moment["call"]["tool"], "mcp__steery__sessions_describe");
    assert_eq!(moment["call"]["steery"], "sessions_describe");
    assert_eq!(moment["call"]["group"], "steery");
    assert_eq!(moment["call"]["input"]["text"], "Reading");
    let allowed = answer(
        &driver,
        "permissionRequest",
        &permission,
        json!({"decision": {"allow": {"reason": "Fine"}}}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(allowed["stdout"].as_str().unwrap()).unwrap(),
        json!({"behavior": "allow"})
    );

    let started = json!({
        "sessionId": SESSION,
        "timestamp": 1794310750326u64,
        "cwd": "/private/tmp/cph/work",
        "source": "new",
        "initialPrompt": "Run the shell command: echo probe-ok.",
    });
    let moment = read(&driver, "sessionStart", &started).await;
    assert_eq!(moment["external_id"], SESSION);
    assert!(moment.get("call").is_none_or(Value::is_null));
    let said = answer(
        &driver,
        "sessionStart",
        &started,
        json!({"say": "You run in Steery as harness copilot."}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(said["stdout"].as_str().unwrap()).unwrap(),
        json!({"additionalContext": "You run in Steery as harness copilot."})
    );

    // An event Steery is not hooked on reads as no moment; a document that is not JSON is
    // refused.
    assert!(read(&driver, "preCompact", &started).await.is_null());
    assert!(
        driver
            .call(
                method::HOOK_READ,
                json!({"event": "sessionStart", "document": "not json"}),
                Duration::from_secs(10),
            )
            .await
            .is_err()
    );
    driver.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
