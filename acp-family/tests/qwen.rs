//! The Qwen Code driver, run as the runtime runs it, over a `$QWEN_HOME` holding chats that
//! Qwen Code 0.27.2 wrote (`tests/qwen/root`): turns that ran a shell command, a turn whose
//! conversation was compressed, a conversation of two prompts, and two turns whose model
//! refused them (a sign-in failure and a connection failure). Its turns read what
//! `qwen -o stream-json` 0.27.2 printed (`tests/qwen/turns`).
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
fn home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("steery-qwen-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/qwen/root"),
        &home.join(".qwen"),
    );
    home
}

/// Copies the fixture, giving each chat the file time Qwen left on it: its last record's
/// `timestamp`. A checkout gives every file the same time, and the listing orders by it.
fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
            continue;
        }
        std::fs::copy(entry.path(), &target).unwrap();
        if target
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            let text = std::fs::read_to_string(&target).unwrap();
            let last: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
            let at =
                chrono::DateTime::parse_from_rfc3339(last["timestamp"].as_str().unwrap()).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&target)
                .unwrap()
                .set_modified(at.into())
                .unwrap();
        }
    }
}

async fn start(home: &Path) -> Client {
    let client = Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            format!("PATH={}", home.join("bin").display()),
            format!(
                "QWEN_CODE_SYSTEM_SETTINGS_PATH={}",
                home.join("system/settings.json").display()
            ),
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
    let home = home("chats");
    let client = start(&home).await;

    let listed = ask(&client, method::SESSIONS_LIST, json!({}))
        .await
        .unwrap();
    assert_eq!(listed["complete"], true);
    let sessions = listed["sessions"].as_array().unwrap();
    let ids: Vec<&str> = sessions
        .iter()
        .map(|session| session["external_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [TWO_PROMPTS, COMPRESSED, FAILED_SHELL, REFUSED]);
    let two = &sessions[0];
    assert_eq!(two["cwd"], "/tmp/qwh/work");
    assert_eq!(
        two["transcripts"][0].as_str().unwrap(),
        chat(&home, "-tmp-qwh-work", TWO_PROMPTS).to_str().unwrap()
    );
    assert!(
        two["title"]
            .as_str()
            .unwrap()
            .starts_with("Reply with the single word: pong")
    );
    assert_eq!(sessions[3]["cwd"], "/private/tmp/qwtest");

    let located = ask(
        &client,
        method::SESSIONS_LOCATE,
        json!({"external_id": COMPRESSED}),
    )
    .await
    .unwrap();
    assert_eq!(located["session"]["external_id"], COMPRESSED);
    let missing = ask(
        &client,
        method::SESSIONS_LOCATE,
        json!({"external_id": "nope"}),
    )
    .await;
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
    assert!(
        events.contains(&"call_ended false".to_owned()),
        "{events:?}"
    );

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
    let again = read(
        &client,
        json!({"external_id": TWO_PROMPTS, "cursor": cursor}),
    )
    .await;
    assert_eq!(lines(&again["events"]), Vec::<String>::new());
    let file = chat(&home, "-tmp-qwh-work", TWO_PROMPTS);
    let last: Value = serde_json::from_str(
        std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .last()
            .unwrap(),
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
    let mut open = std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap();
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
    mcp(&client, &home).await;

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
    assert_eq!(
        said[2]["prompt"],
        "Reply with the single word: pong /no_think"
    );
    assert_eq!(said[3]["outcome"], "finished");
    assert_eq!(said[3]["continued"], false);
    assert!(said[3]["context_left"].as_f64().unwrap() > 0.99);
    assert_eq!(said[12]["person_exited"], true);
    let unhooked =
        json!({"event": "MessageDisplay", "document": "{\"session_id\":\"x\"}", "env": {}});
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
        printed(
            answer(
                "PreToolUse",
                json!({"decision": {"deny": {"reason": "not now"}}})
            )
            .await
        ),
        Some(json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": "not now",
        }}))
    );
    assert_eq!(
        printed(
            answer(
                "PreToolUse",
                json!({"decision": {"allow": {"reason": "ok"}}})
            )
            .await
        ),
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

/// The servers Qwen starts in a folder: the person's, the system's, the project's
/// `.mcp.json` and settings where `mcpApprovals.json` approves them under the hash Qwen
/// 0.27.2's `hashMcpServerConfig` gives (computed by its own code under Node), and an
/// extension's `.mcp.json`; `mcp.excluded` takes a glob.
async fn mcp(client: &Client, home: &Path) {
    let put = |path: PathBuf, text: Value| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text.to_string()).unwrap();
    };
    let root = home.join(".qwen");
    let cwd = home.join("work");
    put(
        root.join("settings.json"),
        json!({"mcpServers": {
            "person": {"url": "http://127.0.0.1:1/sse"},
            "noisy-1": {"command": "noisy"},
        }}),
    );
    put(
        home.join("system/settings.json"),
        json!({
            "mcpServers": {"sys": {"httpUrl": "http://127.0.0.1:2/mcp"}},
            "mcp": {"excluded": ["noisy-*"]},
        }),
    );
    let approved = json!({"command": "node", "args": ["s.js", "é"],
        "env": {"Z": "1", "A": "x\u{1}"}, "timeout": 30000.0, "trust": true, "type": "stdio"});
    put(
        cwd.join(".mcp.json"),
        json!({"mcpServers": {
            "approved": approved,
            "waiting": {"command": "w"},
            "stale": {"command": "changed"},
        }}),
    );
    put(
        cwd.join(".qwen/settings.json"),
        json!({"mcpServers": {"workspace": {"httpUrl": "http://127.0.0.1:9/mcp",
            "headers": {"b": "2", "a": "1"}, "description": "d"}}}),
    );
    let record = |hash: &str| json!({"hash": hash, "status": "approved"});
    put(
        root.join("mcpApprovals.json"),
        json!({cwd.to_str().unwrap(): {
            "approved": record("af6099a313a9c1f3b5287a67f315f58a098b999b329a6d633c613d00858f6f88"),
            "workspace": record("4999c58c9aeccc26f2dfaea4d37a3fb2e002390a50a57b352a36f4b317aca83c"),
            "stale": record("af6099a313a9c1f3b5287a67f315f58a098b999b329a6d633c613d00858f6f88"),
        }}),
    );
    let extension = root.join("extensions/ext");
    put(
        extension.join("qwen-extension.json"),
        json!({"name": "ext", "version": "1.0.0"}),
    );
    put(
        extension.join(".mcp.json"),
        json!({"mcpServers": {
            "fromext": {"command": "e"},
            "person": {"url": "http://127.0.0.1:3/ext"},
        }}),
    );

    let listed = ask(client, method::MCP_LIST, json!({"cwd": cwd}))
        .await
        .unwrap();
    let servers: Vec<(&str, Option<&str>)> = listed["servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["server"].as_str().unwrap(), s["url"].as_str()))
        .collect();
    assert_eq!(
        servers,
        [
            ("approved", None),
            ("fromext", None),
            ("person", Some("http://127.0.0.1:1/sse")),
            ("sys", Some("http://127.0.0.1:2/mcp")),
            ("workspace", Some("http://127.0.0.1:9/mcp")),
        ]
    );
    assert_eq!(listed["complete"], false);

    // Asked of no folder: the person's own servers, and the system's.
    let own = ask(client, method::MCP_LIST, json!({})).await.unwrap();
    let names: Vec<&str> = own["servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["server"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["fromext", "person", "sys"]);
}

/// What each read of a recorded turn said, a short line each: the conversation it named,
/// its events, how it ended and the refusal.
async fn turn(client: &Client, turn: &str, lines_of: &[(&str, String)]) -> Vec<String> {
    let mut said = Vec::new();
    for (stream, line) in lines_of {
        let read = ask(
            client,
            method::TURN_READ,
            json!({"turn": turn, "stream": stream, "line": line}),
        )
        .await
        .unwrap();
        if let Some(named) = read["named"].as_str() {
            said.push(format!("named {named}"));
        }
        if let Some(events) = read.get("events") {
            said.extend(lines(events));
        }
        if let Some(ended) = read.get("ended") {
            said.push(format!("ended {}", ended["state"].as_str().unwrap()));
        }
        if let Some(refused) = read["refused"].as_str() {
            said.push(format!("refused {refused}"));
        }
    }
    said
}

/// The lines of a recorded turn's file, `stream` each.
fn recorded(name: &str, stream: &'static str) -> Vec<(&'static str, String)> {
    let turns = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/qwen/turns");
    std::fs::read_to_string(turns.join(name))
        .unwrap()
        .lines()
        .map(|line| (stream, line.to_owned()))
        .collect()
}

/// Steery plans Qwen's turns only where its guarded `PreToolUse` hook is in the settings
/// Qwen loads, refuses a continue of a conversation Qwen does not keep, and reads what
/// `qwen -o stream-json` 0.27.2 printed: a turn that ran a shell command and finished, one
/// whose call Steery's hook denied, one stopped with SIGINT, and turns Qwen would not run
/// (an unknown conversation, a model its provider does not serve).
#[tokio::test]
async fn starts_and_reads_qwen_turns() {
    let home = home("drive");
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let qwen = bin.join("qwen");
    std::fs::write(&qwen, "#!/bin/sh\necho 0.27.2\n").unwrap();
    let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&qwen, mode).unwrap();
    let client = start(&home).await;
    let cwd = home.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let new = json!({"act": "new", "cwd": cwd, "folder": cwd, "worktree": false});

    let ungated = ask(&client, method::START, new.clone()).await.unwrap_err();
    assert!(ungated.contains("refused"), "{ungated}");

    let planned = ask(
        &client,
        method::SETUP_PLAN,
        json!({"mcp_url": "http://127.0.0.1:1/mcp", "skill": "", "hook": "/opt/steery hook",
               "door": "", "release": "7.0.0", "timeout": 60, "trusts": []}),
    )
    .await
    .unwrap();
    let before_call = planned["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["at"] == "/hooks/PreToolUse/-")
        .unwrap();
    std::fs::write(
        home.join(".qwen/settings.json"),
        json!({"hooks": {"PreToolUse": [before_call["value"]]}}).to_string(),
    )
    .unwrap();

    let started = ask(&client, method::START, new).await.unwrap();
    assert_eq!(
        started["argv"],
        json!([qwen, "-o", "stream-json", "--approval-mode", "yolo"])
    );
    assert_eq!(started["stdin"], "words");
    let resumed = ask(
        &client,
        method::START,
        json!({"act": "continue", "external_id": TWO_PROMPTS, "cwd": cwd, "folder": cwd,
               "worktree": false}),
    )
    .await
    .unwrap();
    assert_eq!(
        resumed["argv"],
        json!([
            qwen,
            "-o",
            "stream-json",
            "--approval-mode",
            "yolo",
            "-r",
            TWO_PROMPTS
        ])
    );
    let unknown = ask(
        &client,
        method::START,
        json!({"act": "continue", "external_id": "0c1d2e3f-0000-4000-8000-000000000000",
               "cwd": cwd, "folder": cwd, "worktree": false}),
    )
    .await
    .unwrap_err();
    assert!(unknown.contains("not-found"), "{unknown}");
    let thinking = ask(
        &client,
        method::START,
        json!({"act": "new", "cwd": cwd, "folder": cwd, "worktree": false, "thinking": "high"}),
    )
    .await
    .unwrap_err();
    assert!(thinking.contains("selection-unsupported"), "{thinking}");

    let finished = recorded("finished.stdout.jsonl", "stdout");
    assert_eq!(
        turn(&client, "finished", &finished).await,
        [
            "named b8bcaca4-04a6-42d0-987d-c6e17ad38798",
            "model",
            "call_began shell run_shell_command",
            "call_ended true",
            "words tool",
            "ended finished",
        ]
    );
    let denied = recorded("denied.stdout.jsonl", "stdout");
    let denied = turn(&client, "denied", &denied).await;
    assert_eq!(
        denied[..5],
        [
            "named b8bcaca4-04a6-42d0-987d-c6e17ad38798",
            "model",
            "call_began shell run_shell_command",
            "call_ended false",
            "words tool",
        ]
    );
    assert_eq!(denied.last().unwrap(), "ended finished");

    // A stopped turn prints no `result`: the runtime ends it on its own signal.
    let mut stopped = recorded("stopped.stdout.jsonl", "stdout");
    stopped.extend(recorded("stopped.stderr", "stderr"));
    let stopped = turn(&client, "stopped", &stopped).await;
    assert!(
        stopped.iter().all(|line| !line.starts_with("ended")),
        "{stopped:?}"
    );
    assert!(stopped.contains(&"call_began shell run_shell_command".to_owned()));

    let unknown = recorded("unknown.stderr", "stderr");
    assert_eq!(
        turn(&client, "unknown", &unknown).await,
        ["ended failed", "refused not-found"]
    );
    let mut missing = recorded("model-missing.stdout.jsonl", "stdout");
    missing.extend(recorded("model-missing.stderr", "stderr"));
    let missing = turn(&client, "model-missing", &missing).await;
    assert_eq!(
        missing[missing.len() - 2..],
        ["ended failed", "refused model-missing"]
    );
}
