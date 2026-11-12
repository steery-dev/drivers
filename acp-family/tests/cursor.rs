//! The Cursor driver, run as the runtime runs it, over a home holding one chat that Cursor
//! wrote (`fixtures/cursor`): a turn that ran a shell command, a turn whose `cursor-agent`
//! was killed during its call, and a turn after the chat resumed. Cursor's system prompt
//! and the environment it tells the model are taken out of the fixture. Then the setup of
//! Cursor's hooks, and its hooks' documents as `cursor-agent` 2026.11.05 handed them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::Client;
use steery_sdk::{method, wire};

const CHAT: &str = "70895076-1186-4e4d-918d-239ace528a16";

/// A home of this test's own (`name`), with the fixture chat filed twice: as a person's
/// chat, and as a `-p` run's, which holds no `prompt_history.json`.
fn home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("steery-cursor-{name}-{}", std::process::id()));
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
async fn reads_a_chat_cursor_wrote_and_the_hooks_cursor_runs() {
    let home = home("observe");
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

    hooks(&client, &home).await;
    std::fs::remove_dir_all(&home).unwrap();
}

async fn ask(client: &Client, method: &str, params: Value) -> Result<Value, String> {
    client
        .call(method, params, Duration::from_secs(10))
        .await
        .map_err(|error| error.to_string())
}

/// Setup writes one hook per event into `~/.cursor/hooks.json`, beside the person's own;
/// each document reads as its moment, and each answer prints as Cursor parses it.
async fn hooks(client: &Client, home: &Path) {
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("cursor-agent");
    std::fs::write(&fake, "#!/bin/sh\necho 2026.11.05-86047df\n").unwrap();
    let mut mode = std::fs::metadata(&fake).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut mode, 0o755);
    std::fs::set_permissions(&fake, mode).unwrap();
    let setup = json!({
        "mcp_url": "http://127.0.0.1:7/mcp",
        "skill": "skill",
        "hook": "/usr/local/bin/steery hook cursor",
        "door": "http://127.0.0.1:7/door",
        "release": "7.0.0",
        "timeout": 30,
    });
    let planned = ask(client, method::SETUP_PLAN, setup).await.unwrap();
    let file = home.join(".cursor/hooks.json");
    let written: Vec<(Value, Value)> = planned["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["path"] == json!(file))
        .map(|entry| (entry["at"].clone(), entry["value"].clone()))
        .collect();
    let hook = |event: &str, timeout: u32| {
        (
            json!(format!("/hooks/{event}/-")),
            json!({"command": format!("/usr/local/bin/steery hook cursor {event}"), "timeout": timeout}),
        )
    };
    assert_eq!(
        written,
        [
            (json!("/version"), json!(1)),
            hook("sessionStart", 5),
            hook("beforeSubmitPrompt", 5),
            hook("preToolUse", 86_400),
            hook("postToolUse", 5),
            hook("postToolUseFailure", 5),
            hook("stop", 5),
            hook("sessionEnd", 5),
        ],
        "{planned}"
    );

    // Documents as `cursor-agent` 2026.11.05 handed them, the person's email taken out.
    let conversation = "d4d47873-016f-4fc5-9378-850bccc64e90";
    let base = |event: &str, fields: Value| {
        let mut document = json!({
            "conversation_id": conversation,
            "generation_id": "8ac5b704-3725-4067-8232-55d18a3c5a12",
            "model": "default",
            "session_id": conversation,
            "hook_event_name": event,
            "cursor_version": "2026.11.05-86047df",
            "workspace_roots": ["/private/tmp/cursor-hook-repro"],
            "user_email": null,
            "transcript_path": null,
        });
        document
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        document
    };
    let read = |event: &str, document: &Value| {
        let params = json!({"event": event, "document": document.to_string(), "env": {}});
        async move { ask(client, method::HOOK_READ, params).await.unwrap()["moment"].clone() }
    };

    let started = read(
        "sessionStart",
        &base("sessionStart", json!({"is_background_agent": false})),
    )
    .await;
    assert_eq!(started["external_id"], conversation);
    assert_eq!(started["cwd"], "/private/tmp/cursor-hook-repro");
    assert_eq!(started["model"], "default");

    let prompt = base(
        "beforeSubmitPrompt",
        json!({"prompt": "Run `echo ok`.", "attachments": []}),
    );
    assert_eq!(
        read("beforeSubmitPrompt", &prompt).await["prompt"],
        "Run `echo ok`."
    );

    let mcp = base(
        "preToolUse",
        json!({
            "tool_name": "MCP:ping",
            "tool_input": {"word": "kiwi"},
            "tool_use_id": "f34fe47d-a7a3-4918-a25c-4a18a7b46d6e",
        }),
    );
    let call = read("preToolUse", &mcp).await["call"].clone();
    assert_eq!(
        (&call["id"], &call["tool"], &call["group"], &call["input"]),
        (
            &json!("f34fe47d-a7a3-4918-a25c-4a18a7b46d6e"),
            &json!("MCP:ping"),
            &json!("mcp"),
            &json!({"word": "kiwi"})
        )
    );

    // A call that a `preToolUse` hook denied; Cursor's own ids can hold a line break.
    let failed = base(
        "postToolUseFailure",
        json!({
            "tool_name": "Shell",
            "tool_input": {"command": "echo forbidden", "cwd": "", "timeout": 30000},
            "tool_use_id": "call-841ff0cb-2\nfc_dcdabc3d_0",
            "error_message": "Shell blocked by preToolUse hook",
            "failure_type": "permission_denied",
            "cwd": "",
        }),
    );
    let failed = read("postToolUseFailure", &failed).await;
    assert_eq!(failed["ok"], false);
    assert_eq!(failed["call"]["id"], "call-841ff0cb-2\nfc_dcdabc3d_0");
    assert_eq!(failed["call"]["group"], "shell");

    let stop =
        |status: &str, loops: u32| base("stop", json!({"status": status, "loop_count": loops}));
    let followed = read("stop", &stop("completed", 1)).await;
    assert_eq!(
        (&followed["outcome"], &followed["continued"]),
        (&json!("finished"), &json!(true))
    );
    let aborted = read("stop", &stop("aborted", 0)).await;
    assert_eq!(
        (&aborted["outcome"], &aborted["continued"]),
        (&json!("interrupted"), &json!(false))
    );

    let ended = base(
        "sessionEnd",
        json!({"reason": "completed", "final_status": "completed"}),
    );
    assert_eq!(
        read("sessionEnd", &ended).await["external_id"],
        conversation
    );
    let response = base("afterAgentResponse", json!({"text": "ok"}));
    assert_eq!(read("afterAgentResponse", &response).await, Value::Null);

    let answer = |event: &str, answer: Value| {
        let document = base(event, json!({})).to_string();
        let params = json!({"event": event, "document": document, "env": {}, "answer": answer});
        async move { ask(client, method::HOOK_ANSWER, params).await }
    };
    let printed = |answered: Result<Value, String>| {
        let answered = answered.unwrap();
        assert_eq!(answered["exit_code"], 0, "{answered}");
        let stdout = answered["stdout"].as_str().unwrap();
        (!stdout.is_empty()).then(|| serde_json::from_str::<Value>(stdout).unwrap())
    };
    assert_eq!(
        printed(
            answer(
                "preToolUse",
                json!({"decision": {"deny": {"reason": "not now"}}})
            )
            .await
        ),
        Some(json!({"permission": "deny", "user_message": "not now"}))
    );
    assert_eq!(
        printed(
            answer(
                "preToolUse",
                json!({"decision": {"allow": {"reason": "ok"}}, "say": "Words wait."})
            )
            .await
        ),
        Some(json!({"permission": "allow", "additional_context": "Words wait."}))
    );
    assert_eq!(
        printed(answer("postToolUse", json!({"say": "Words wait."})).await),
        Some(json!({"additional_context": "Words wait."}))
    );
    assert_eq!(
        printed(answer("stop", json!({"say": "Go on."})).await),
        Some(json!({"followup_message": "Go on."}))
    );
    // A lead has nowhere to go in Cursor: nothing is printed.
    assert_eq!(
        printed(answer("sessionStart", json!({"lead": "cursor:k1"})).await),
        None
    );
    for (event, refused) in [
        ("beforeSubmitPrompt", json!({"say": "Words wait."})),
        (
            "preToolUse",
            json!({"decision": {"allow": {"reason": "ok", "always": "tool"}}}),
        ),
        ("stop", json!({"decision": {"deny": {"reason": "no"}}})),
    ] {
        let error = answer(event, refused).await.unwrap_err();
        assert!(error.contains("Cursor does not"), "{event}: {error}");
    }
}

/// Each line of a recorded turn, read in order as one run, and what the last read said.
async fn turn(client: &Client, turn: &str, lines: &[(&str, &str)]) -> Vec<Value> {
    let mut read = Vec::new();
    for (stream, line) in lines {
        read.push(
            client
                .call(
                    method::TURN_READ,
                    json!({"turn": turn, "stream": stream, "line": line}),
                    Duration::from_secs(10),
                )
                .await
                .unwrap(),
        );
    }
    read
}

/// The lines of a recorded turn, `stream` each.
fn recorded<'a>(text: &'a str, stream: &'a str) -> Vec<(&'a str, &'a str)> {
    text.lines().map(|line| (stream, line)).collect()
}

/// Steery plans Cursor's turns, refuses a continue of a chat Cursor does not keep, and reads
/// what `cursor-agent -p --output-format stream-json` 2026.11.05 wrote: a turn that ran a
/// shell command and finished, and turns that Cursor refused for its usage limit and for a
/// folder its person did not trust.
#[tokio::test]
async fn starts_and_reads_cursor_turns() {
    let home = home("drive");
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let cursor = bin.join("cursor-agent");
    std::fs::write(&cursor, "#!/bin/sh\n").unwrap();
    let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&cursor, mode).unwrap();
    let client = start(&home).await;
    let call = |method: &'static str, params: Value| {
        let client = &client;
        async move { client.call(method, params, Duration::from_secs(10)).await }
    };
    let cwd = std::env::temp_dir();

    let new = call(
        method::START,
        json!({"act": "new", "cwd": cwd, "folder": cwd, "worktree": false}),
    )
    .await
    .unwrap();
    assert_eq!(
        new["argv"],
        json!([cursor, "-p", "--output-format", "stream-json"])
    );
    assert_eq!(new["stdin"], "words");
    let resumed = call(
        method::START,
        json!({"act": "continue", "external_id": CHAT, "cwd": cwd, "folder": cwd,
               "worktree": false, "model": "auto"}),
    )
    .await
    .unwrap();
    assert_eq!(
        resumed["argv"],
        json!([
            cursor,
            "-p",
            "--output-format",
            "stream-json",
            "--resume",
            CHAT,
            "--model",
            "auto"
        ])
    );
    // `--resume` of an id Cursor does not know starts a new chat under it.
    let unknown = call(
        method::START,
        json!({"act": "continue", "external_id": "0c1d2e3f-0000-4000-8000-000000000000",
               "cwd": cwd, "folder": cwd, "worktree": false}),
    )
    .await
    .unwrap_err();
    assert!(unknown.to_string().contains("not-found"), "{unknown}");
    let thinking = call(
        method::START,
        json!({"act": "new", "cwd": cwd, "folder": cwd, "worktree": false, "thinking": "high"}),
    )
    .await
    .unwrap_err();
    assert!(
        thinking.to_string().contains("selection-unsupported"),
        "{thinking}"
    );

    let turns = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/cursor/turns");
    let text = |name: &str| std::fs::read_to_string(turns.join(name)).unwrap();

    let finished = turn(
        &client,
        "finished",
        &recorded(&text("finished.stdout.jsonl"), "stdout"),
    )
    .await;
    assert_eq!(finished[0]["named"], "d53c73cb-9604-457b-9d6c-ecb3f82fee69");
    assert_eq!(finished[0]["took"], false);
    assert_eq!(finished[1]["took"], true);
    let ended: Vec<&Value> = finished
        .iter()
        .filter_map(|line| line.get("ended"))
        .collect();
    assert_eq!(ended, [&json!({"state": "finished"})]);
    assert!(finished.last().unwrap().get("ended").is_some());

    let stdout = text("usage-limit.stdout.jsonl");
    let stderr = text("usage-limit.stderr");
    let mut lines = recorded(&stdout, "stdout");
    lines.extend(recorded(&stderr, "stderr"));
    let limit = turn(&client, "usage-limit", &lines).await;
    assert_eq!(limit[1]["took"], true);
    assert_eq!(
        limit[2],
        json!({"ended": {"state": "failed", "error": stderr.trim()}, "refused": "refused"})
    );

    let untrusted = turn(
        &client,
        "untrusted",
        &recorded(&text("untrusted.stderr"), "stderr"),
    )
    .await;
    let refused: Vec<&Value> = untrusted
        .iter()
        .filter_map(|line| line.get("refused"))
        .collect();
    assert_eq!(refused, [&json!("untrusted")]);
    assert_eq!(untrusted[1]["ended"]["state"], "failed");
}
