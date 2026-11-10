//! The Cursor driver, run as the runtime runs it, over a home holding one chat that Cursor
//! wrote (`fixtures/cursor`): a turn that ran a shell command, a turn whose `cursor-agent`
//! was killed during its call, and a turn after the chat resumed. Cursor's system prompt
//! and the environment it tells the model are taken out of the fixture.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::Client;
use steery_sdk::{method, wire};

const CHAT: &str = "70895076-1186-4e4d-918d-239ace528a16";

/// A home of this test's own, with the fixture chat filed twice: as a person's chat, and
/// as a `-p` run's, which holds no `prompt_history.json`.
fn home() -> PathBuf {
    let home = std::env::temp_dir().join(format!("steery-cursor-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/cursor")
        .join(CHAT);
    for (id, person) in [(CHAT, true), ("headless", false)] {
        let chat = home.join(".cursor/chats/a1b2").join(id);
        std::fs::create_dir_all(&chat).unwrap();
        for file in ["store.db", "meta.json", "prompt_history.json"] {
            if file != "prompt_history.json" || person {
                std::fs::copy(fixture.join(file), chat.join(file)).unwrap();
            }
        }
    }
    home
}

async fn start(home: &Path) -> Client {
    let client = Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            env!("CARGO_BIN_EXE_steery-driver-cursor").into(),
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

async fn read(client: &Client, params: Value) -> Value {
    client
        .call(method::SESSIONS_READ, params, Duration::from_secs(10))
        .await
        .unwrap()
}

/// Each event as one short line.
fn lines(events: &Value) -> Vec<String> {
    let events = events.as_array().unwrap();
    events
        .iter()
        .map(|event| match event["type"].as_str().unwrap() {
            "words" => format!("words {}", event["voice"].as_str().unwrap()),
            "turn_ended" => format!("turn_ended {}", event["state"].as_str().unwrap()),
            "call_began" => format!("call_began {}", event["call"]["group"].as_str().unwrap()),
            kind => kind.to_owned(),
        })
        .collect()
}

#[tokio::test]
async fn reads_a_chat_cursor_wrote_and_ends_its_killed_turn_interrupted() {
    let home = home();
    let client = start(&home).await;

    let listed = client
        .call(
            method::SESSIONS_LIST,
            json!({"transcripts": []}),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{listed}");
    assert_eq!(sessions[0]["external_id"], CHAT);
    assert_eq!(sessions[0]["cwd"], "/private/tmp/cursor-fixture-ws");
    assert_eq!(listed["excluded"], json!(["headless"]), "{listed}");

    let whole = read(&client, json!({"external_id": CHAT})).await;
    assert_eq!(whole["whole"], true);
    assert_eq!(
        lines(&whole["events"]),
        [
            "turn_began",
            "words person",
            "words thought",
            "words agent",
            "call_began shell",
            "call_ended",
            "words tool",
            "words thought",
            "words agent",
            "turn_ended finished",
            // `cursor-agent` killed while `sleep 40` ran: Cursor wrote no step of the call.
            "turn_began",
            "words person",
            "words thought",
            "words agent",
            "turn_ended interrupted",
            "turn_began",
            "words person",
            "words thought",
            "words agent",
            "turn_ended finished",
        ]
    );
    let events = whole["events"].as_array().unwrap();
    assert_eq!(
        events[1]["text"],
        "Run the shell command `echo one` with your shell tool, then reply with the single word done."
    );
    let call = &events[4]["call"];
    assert_eq!(call["tool"], "Shell");
    assert_eq!(call["input"]["command"], "echo one");
    assert_eq!(events[5]["id"], call["id"]);
    assert_eq!(events[5]["ok"], true);
    assert_eq!(events[6]["call"], call["id"]);
    assert!(events[6]["text"].as_str().unwrap().contains("one"));
    assert_eq!(events[18]["text"], "ok");

    // Read on from where it ended: nothing new. A cursor of no turn here reads it all again.
    let on = read(
        &client,
        json!({"external_id": CHAT, "cursor": whole["cursor"]}),
    )
    .await;
    assert_eq!(
        (on["events"].clone(), on["reset"].clone()),
        (json!([]), json!(false))
    );
    let lost = read(&client, json!({"external_id": CHAT, "cursor": "9:0:00"})).await;
    assert_eq!(lost["reset"], true);
    assert_eq!(lost["events"], whole["events"]);

    std::fs::remove_dir_all(&home).unwrap();
}
