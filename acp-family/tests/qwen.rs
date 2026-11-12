//! The Qwen Code driver, run as the runtime runs it, over a `$QWEN_HOME` holding chats that
//! Qwen Code 0.27.2 wrote (`tests/qwen/root`): turns that ran a shell command, a turn whose
//! conversation was compressed, a conversation of two prompts, and two turns whose model
//! refused them (a sign-in failure and a connection failure).
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::Client;
use steery_sdk::{method, wire};

const COMPRESSED: &str = "f331863d-818d-4359-bc6c-bb62df427236";
const FAILED_SHELL: &str = "6022f578-09cb-4247-85bd-a1ed7fd6ffd3";
const REFUSED: &str = "beaa9903-0b5a-4be4-bcde-2b6515e54ed4";
const TWO_PROMPTS: &str = "5ce1cae9-3180-43b0-93ef-01cbca61d62b";

/// A home of this test's own, whose `.qwen` holds a copy of the fixture.
fn home() -> PathBuf {
    let home = std::env::temp_dir().join(format!("steery-qwen-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/qwen/root"),
        &home.join(".qwen"),
    );
    home
}

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

async fn start(home: &Path) -> Client {
    let client = Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            env!("CARGO_BIN_EXE_steery-driver-qwen").into(),
        ],
        None,
    )
    .await
    .unwrap();
    let offered = wire::InitializeParams {
        protocol: vec![13],
        runtime: wire::Runtime {
            release: "7.0.0".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        },
        home: home.to_owned(),
        config: home.join(".config/steery"),
        settings: Default::default(),
    };
    client.initialize(&offered).await.unwrap();
    client
}

async fn ask(client: &Client, method: &str, params: Value) -> Result<Value, String> {
    client
        .call(method, params, Duration::from_secs(10))
        .await
        .map_err(|error| error.to_string())
}

async fn read(client: &Client, params: Value) -> Value {
    ask(client, method::SESSIONS_READ, params).await.unwrap()
}

/// Each event as one short line.
fn lines(events: &Value) -> Vec<String> {
    events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| match event["type"].as_str().unwrap() {
            "words" => format!("words {}", event["voice"].as_str().unwrap()),
            "turn_ended" => format!("turn_ended {}", event["state"].as_str().unwrap()),
            "call_began" => format!(
                "call_began {} {}",
                event["call"]["group"].as_str().unwrap(),
                event["call"]["tool"].as_str().unwrap()
            ),
            "call_ended" => format!("call_ended {}", event["ok"]),
            kind => kind.to_owned(),
        })
        .collect()
}

fn chat(home: &Path, project: &str, id: &str) -> PathBuf {
    home.join(".qwen/projects")
        .join(project)
        .join("chats")
        .join(format!("{id}.jsonl"))
}

#[tokio::test]
async fn reads_the_chats_qwen_wrote() {
    let home = home();
    let client = start(&home).await;

    let listed = ask(&client, method::SESSIONS_LIST, json!({})).await.unwrap();
    assert_eq!(listed["complete"], true);
    let sessions = listed["sessions"].as_array().unwrap();
    let ids: Vec<&str> = sessions
        .iter()
        .map(|session| session["external_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [REFUSED, FAILED_SHELL, COMPRESSED, TWO_PROMPTS]);
    let two = &sessions[3];
    assert_eq!(two["cwd"], "/tmp/qwh/work");
    assert_eq!(
        two["transcripts"][0].as_str().unwrap(),
        chat(&home, "-tmp-qwh-work", TWO_PROMPTS).to_str().unwrap()
    );
    assert!(two["title"].as_str().unwrap().starts_with("Reply with the single word: pong"));
    assert_eq!(sessions[0]["cwd"], "/private/tmp/qwtest");

    let located = ask(&client, method::SESSIONS_LOCATE, json!({"external_id": COMPRESSED}))
        .await
        .unwrap();
    assert_eq!(located["session"]["external_id"], COMPRESSED);
    let missing = ask(&client, method::SESSIONS_LOCATE, json!({"external_id": "nope"})).await;
    let missing = missing.unwrap_err();
    assert!(missing.contains("not-found"), "{missing}");

    // A shell command that ran, then the compression of the conversation after the turn.
    let read_compressed = read(&client, json!({"external_id": COMPRESSED})).await;
    assert_eq!(
        lines(&read_compressed["events"]),
        [
            "turn_began",
            "words person",
            "model",
            "words thought",
            "call_began shell run_shell_command",
            "tokens",
            "call_ended true",
            "words tool",
            "words thought",
            "words agent",
            "tokens",
            "turn_ended finished",
            "compacted",
        ]
    );
    let events = read_compressed["events"].as_array().unwrap();
    assert_eq!(events[2]["model"], "qwen3:1.7b");
    let compacted = events.last().unwrap();
    assert_eq!(compacted["tokens_before"], 2050);
    assert_eq!(compacted["tokens_after"], 508);

    // A shell command that failed to start.
    let failed = read(&client, json!({"external_id": FAILED_SHELL})).await;
    let events = lines(&failed["events"]);
    assert!(events.contains(&"call_ended false".to_owned()), "{events:?}");

    // The model's service refused both turns: its failure says why.
    let refused = read(&client, json!({"external_id": REFUSED})).await;
    assert_eq!(
        lines(&refused["events"]),
        [
            "turn_began",
            "words person",
            "turn_ended failed",
            "turn_began",
            "words person",
            "turn_ended failed",
        ]
    );
    let sign_in = &refused["events"][2]["failure"];
    assert_eq!(sign_in["kind"], "sign_in");
    assert_eq!(sign_in["http_status"], 401);
    assert_eq!(refused["events"][5]["failure"]["kind"], "other");

    // Two prompts, two turns.
    let two = read(&client, json!({"external_id": TWO_PROMPTS})).await;
    let turns = lines(&two["events"])
        .into_iter()
        .filter(|line| line == "turn_began")
        .count();
    assert_eq!(turns, 2);

    // A read from the cursor says nothing until Qwen appends; then it says the new prompt.
    let cursor = two["cursor"].as_str().unwrap().to_owned();
    let again = read(&client, json!({"external_id": TWO_PROMPTS, "cursor": cursor})).await;
    assert_eq!(lines(&again["events"]), Vec::<String>::new());
    let file = chat(&home, "-tmp-qwh-work", TWO_PROMPTS);
    let last: Value = serde_json::from_str(
        std::fs::read_to_string(&file).unwrap().lines().last().unwrap(),
    )
    .unwrap();
    let prompt = json!({
        "uuid": "11111111-2222-4333-8444-555555555555",
        "parentUuid": last["uuid"],
        "sessionId": TWO_PROMPTS,
        "timestamp": "2026-11-12T12:00:00.000Z",
        "type": "user",
        "provenance": "real_user",
        "cwd": "/tmp/qwh/work",
        "message": {"role": "user", "parts": [{"text": "third"}]},
    });
    let mut open = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
    writeln!(open, "{prompt}").unwrap();
    let on = read(
        &client,
        json!({"external_id": TWO_PROMPTS, "cursor": again["cursor"]}),
    )
    .await;
    assert_eq!(lines(&on["events"]), ["turn_began", "words person"]);
    assert_eq!(on["events"][1]["text"], "third");

    let fold = ask(&client, method::SESSIONS_FOLD, json!({"harness": "qwen"})).await;
    assert!(fold.is_err());

    hooks(&client).await;

    let _ = std::fs::remove_dir_all(&home);
}

/// Each document Qwen Code 0.27.2 handed its hooks in the fixture's ACP conversation reads
/// as its moment, and each answer prints as Qwen parses it.
async fn hooks(client: &Client) {
    let documents = include_str!("qwen/hooks/documents.tsv");
    let mut said = Vec::new();
    for line in documents.lines() {
        let (event, document) = line.split_once('\t').unwrap();
        let params = json!({"event": event, "document": document, "env": {}});
        let moment = ask(client, method::HOOK_READ, params).await.unwrap()["moment"].clone();
        assert_eq!(moment["external_id"], TWO_PROMPTS, "{event}");
        assert_eq!(moment["cwd"], "/tmp/qwh/work", "{event}");
        assert!(moment["at"].is_u64(), "{event}");
        said.push(moment);
    }
    let shape: Vec<String> = said
        .iter()
        .map(|moment| {
            let mut line = moment["event"].as_str().unwrap().to_owned();
            if let Some(call) = moment["call"]["id"].as_str() {
                line += &format!(" {call} {}", moment["call"]["group"].as_str().unwrap());
            }
            if let Some(ok) = moment["ok"].as_bool() {
                line += &format!(" ok={ok}");
            }
            line
        })
        .collect();
    assert_eq!(
        shape,
        [
            "SessionStart",
            "Notification",
            "UserPromptSubmit",
            "Stop",
            "UserPromptSubmit",
            "PreToolUse call_ina3yuq4 files-edit",
            "PreToolUse call_yfsm13l1 files-edit",
            "PostToolUseFailure call_ina3yuq4 files-edit ok=false",
            "PostToolUseFailure call_yfsm13l1 files-edit ok=false",
            "PreToolUse call_x5bkxwf5 files-read",
            "PostToolUse call_x5bkxwf5 files-read ok=true",
            "Stop",
            "SessionEnd",
        ]
    );
    assert_eq!(said[0]["model"], "qwen3:1.7b");
    assert!(
        said[0]["transcript"]
            .as_str()
            .unwrap()
            .ends_with(&format!("chats/{TWO_PROMPTS}.jsonl"))
    );
    assert_eq!(said[2]["prompt"], "Reply with the single word: pong /no_think");
    assert_eq!(said[3]["outcome"], "finished");
    assert_eq!(said[3]["continued"], false);
    assert!(said[3]["context_left"].as_f64().unwrap() > 0.99);
    assert_eq!(said[12]["person_exited"], true);
    let unhooked = json!({"event": "MessageDisplay", "document": "{\"session_id\":\"x\"}", "env": {}});
    assert_eq!(
        ask(client, method::HOOK_READ, unhooked).await.unwrap()["moment"],
        Value::Null
    );

    let answer = |event: &str, answer: Value| {
        let params = json!({"event": event, "document": "{}", "env": {}, "answer": answer});
        async move { ask(client, method::HOOK_ANSWER, params).await }
    };
    let printed = |answered: Result<Value, String>| {
        let answered = answered.unwrap();
        assert_eq!(answered["exit_code"], 0, "{answered}");
        let stdout = answered["stdout"].as_str().unwrap();
        (!stdout.is_empty()).then(|| serde_json::from_str::<Value>(stdout).unwrap())
    };
    assert_eq!(
        printed(answer("PreToolUse", json!({"decision": {"deny": {"reason": "not now"}}})).await),
        Some(json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": "not now",
        }}))
    );
    assert_eq!(
        printed(answer("PreToolUse", json!({"decision": {"allow": {"reason": "ok"}}})).await),
        None
    );
    assert_eq!(
        printed(answer("PostToolUse", json!({"say": "Words wait."})).await),
        Some(json!({"hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": "Words wait.",
        }}))
    );
    assert_eq!(
        printed(answer("Stop", json!({"say": "Go on."})).await),
        Some(json!({"decision": "block", "reason": "Go on."}))
    );
    assert_eq!(
        printed(answer("SessionStart", json!({"lead": "qwen:k1"})).await),
        None
    );
    for (event, refused) in [
        ("Notification", json!({"say": "Words wait."})),
        ("Stop", json!({"decision": {"deny": {"reason": "no"}}})),
    ] {
        let error = answer(event, refused).await.unwrap_err();
        assert!(error.contains("Qwen Code does not"), "{event}: {error}");
    }
}
