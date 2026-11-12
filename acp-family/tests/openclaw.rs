//! The OpenClaw driver, run as the runtime runs it, over standard input and output, against
//! a home of its own whose agent store holds real records of OpenClaw 2026.11.3
//! (`fixtures/openclaw/events.jsonl`, one of them zstd-packed): a conversation that read a
//! file and then failed with no reply, and one that ran `echo openclaw-ok` and was then
//! compacted. A window that continues the second after a compaction, and a sub-agent that it
//! spawned, are made up here: no live store has either.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use steery_sdk::client::Client;
use steery_sdk::{method, wire};

const WAIT: Duration = Duration::from_secs(30);

const FAILED: &str = "steery-probe-2";
const COMPACTED: &str = "87807aeb-2f90-4904-ad36-9613c308d123";
const BRIDGED: &str = "82d116be-c2d2-4d50-bf13-f6c6bae2e921";
const BRIDGE_ACP: &str = "dc92b83b-3f94-4c11-8fce-3a3a476036c2";
const BRIDGE_KEY: &str = "agent:main:acp-bridge:dc92b83b-3f94-4c11-8fce-3a3a476036c2";

/// The columns of OpenClaw's tables that the driver reads.
const SCHEMA: &str = "
CREATE TABLE session_nodes (
  session_key TEXT PRIMARY KEY, current_session_id TEXT NOT NULL, entry_json TEXT NOT NULL,
  status TEXT, created_via TEXT, parent_session_key TEXT, spawned_by TEXT, label TEXT,
  display_name TEXT, fork_source_session_id TEXT);
CREATE TABLE session_windows (
  session_id TEXT PRIMARY KEY, session_key TEXT NOT NULL, previous_session_id TEXT,
  reason TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  parent_session_key TEXT, spawned_by TEXT);
CREATE TABLE transcript_events (
  session_id TEXT NOT NULL, seq INTEGER NOT NULL, event_json TEXT, created_at INTEGER NOT NULL,
  event_zstd BLOB, PRIMARY KEY (session_id, seq));
";

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
        .collect()
}

fn node(db: &Connection, key: &str, current: &str, via: Option<&str>, spawned_by: Option<&str>) {
    db.execute(
        "INSERT INTO session_nodes (session_key, current_session_id, entry_json, status,
                                    created_via, spawned_by)
         VALUES (?1, ?2, '{\"contextTokens\":32768}', 'done', ?3, ?4)",
        params![key, current, via, spawned_by],
    )
    .unwrap();
}

fn window(db: &Connection, id: &str, key: &str, previous: Option<&str>, at: i64) {
    db.execute(
        "INSERT INTO session_windows (session_id, session_key, previous_session_id, reason,
                                      created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        params![id, key, previous, previous.map(|_| "compaction"), at],
    )
    .unwrap();
}

fn record(db: &Connection, session: &str, seq: i64, record: Value) {
    db.execute(
        "INSERT INTO transcript_events (session_id, seq, event_json, created_at)
         VALUES (?1, ?2, ?3, 1794487300000)",
        params![session, seq, record.to_string()],
    )
    .unwrap();
}

fn home(root: &Path) {
    let agent = root.join(".openclaw/agents/main/agent");
    std::fs::create_dir_all(&agent).unwrap();
    let db = Connection::open(agent.join("openclaw-agent.sqlite")).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    let probe3 = "agent:main:explicit:steery-probe-3";
    node(
        &db,
        "agent:main:explicit:steery-probe-2",
        FAILED,
        Some("run"),
        None,
    );
    node(&db, probe3, "probe-3-next", Some("run"), None);
    node(
        &db,
        "agent:main:subagent:helper",
        "helper",
        Some("spawn"),
        Some(probe3),
    );
    window(
        &db,
        FAILED,
        "agent:main:explicit:steery-probe-2",
        None,
        1794486535284,
    );
    window(&db, COMPACTED, probe3, None, 1794487199884);
    window(&db, "probe-3-next", probe3, Some(COMPACTED), 1794487290000);
    window(
        &db,
        "helper",
        "agent:main:subagent:helper",
        None,
        1794487295000,
    );
    // A key `openclaw acp` made, as OpenClaw 2026.11.3 writes it: its requester is named in
    // `parent_session_key` alone, and it is no sub-agent's.
    db.execute(
        "INSERT INTO session_nodes (session_key, current_session_id, entry_json, status,
                                    created_via, parent_session_key)
         VALUES (?1, ?2, '{}', 'done', 'operator', 'agent:main:main')",
        params![BRIDGE_KEY, BRIDGED],
    )
    .unwrap();
    db.execute(
        "INSERT INTO session_windows (session_id, session_key, created_at, updated_at,
                                      parent_session_key)
         VALUES (?1, ?2, 1794487400000, 1794487400000, 'agent:main:main')",
        params![BRIDGED, BRIDGE_KEY],
    )
    .unwrap();
    record(
        &db,
        BRIDGED,
        0,
        json!({"type": "session", "version": 4, "id": BRIDGED, "cwd": "/tmp/oc-acp"}),
    );

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/openclaw/events.jsonl");
    for line in std::fs::read_to_string(fixture).unwrap().lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        db.execute(
            "INSERT INTO transcript_events (session_id, seq, event_json, event_zstd, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                row["session_id"].as_str(),
                row["seq"].as_i64(),
                row["event_json"].as_str(),
                row["event_zstd"].as_str().map(unhex),
                row["created_at"].as_i64(),
            ],
        )
        .unwrap();
    }
    record(
        &db,
        "probe-3-next",
        0,
        json!({"type": "session", "version": 4, "id": "probe-3-next", "cwd": "/tmp/oc-work"}),
    );
    record(
        &db,
        "probe-3-next",
        1,
        json!({"type": "message", "id": "m-next", "parentId": null,
               "message": {"role": "user", "content": "And once more.", "timestamp": 1794487291000u64}}),
    );
    record(
        &db,
        "probe-3-next",
        2,
        json!({"type": "message", "id": "m-poll", "parentId": "m-next",
               "message": {"role": "user", "content": "[OpenClaw heartbeat poll]",
                           "provenance": {"kind": "internal_system", "sourceTool": "heartbeat"},
                           "timestamp": 1794487292000u64}}),
    );
    record(
        &db,
        "helper",
        1,
        json!({"type": "message", "id": "h1", "parentId": null,
               "message": {"role": "user", "timestamp": 1794487295000u64,
                           "content": "<<<BEGIN_OPENCLAW_INTERNAL_CONTEXT>>>\n\n[Subagent Context] You are running as a subagent (depth 1/5).\n\n[Subagent Task]\n\n<<<END_OPENCLAW_INTERNAL_CONTEXT>>>\n\nHelp.\n\n<<<BEGIN_OPENCLAW_INTERNAL_CONTEXT>>>\n\nBegin.\n\n<<<END_OPENCLAW_INTERNAL_CONTEXT>>>",
                           "__openclaw": {"senderIsOwner": false}}}),
    );
}

async fn start(root: &Path) -> Client {
    let client = Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", root.display()),
            env!("CARGO_BIN_EXE_steery-driver-openclaw").into(),
        ],
        None,
    )
    .await
    .unwrap();
    let offered = wire::InitializeParams {
        protocol: vec![steery_sdk::YAML],
        runtime: wire::Runtime {
            release: "7.0.0".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        },
        home: root.to_owned(),
        config: root.join(".config/steery"),
        settings: Default::default(),
    };
    client.initialize(&offered).await.unwrap();
    client
}

fn of_type<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["type"] == kind)
        .collect()
}

fn said(events: &[Value], voice: &str) -> Vec<String> {
    of_type(events, "words")
        .into_iter()
        .filter(|event| event["voice"] == voice)
        .map(|event| event["text"].as_str().unwrap().to_owned())
        .collect()
}

async fn read(driver: &Client, id: &str) -> Vec<Value> {
    let read = driver
        .call(method::SESSIONS_READ, json!({"external_id": id}), WAIT)
        .await
        .unwrap();
    assert_eq!(read["whole"], true);
    assert_eq!(read["more"], false);
    read["events"].as_array().unwrap().clone()
}

#[tokio::test]
async fn openclaw_reads_its_conversations_from_the_agent_store() {
    let root = std::env::temp_dir().join(format!("steery-openclaw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    home(&root);
    let driver = start(&root).await;

    let listed = driver
        .call(method::SESSIONS_LIST, json!({"transcripts": []}), WAIT)
        .await
        .unwrap();
    assert_eq!(listed["excluded"], json!(["helper"]));
    let sessions = listed["sessions"].as_array().unwrap();
    let ids: Vec<&str> = sessions
        .iter()
        .map(|session| session["external_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [BRIDGED, COMPACTED, FAILED], "{listed}");
    let compacted = &sessions[1];
    assert_eq!(compacted["cwd"], "/tmp/oc-work");
    assert_eq!(compacted["updated_at"], 1794487290000u64);
    assert_eq!(compacted["sub_agents"][0]["external_id"], "helper");
    assert_eq!(sessions[0]["sub_agents"], json!([]));

    let store = root.join(".openclaw/agents/main/agent/openclaw-agent.sqlite");
    let located = driver
        .call(
            method::SESSIONS_LOCATE,
            json!({"external_id": COMPACTED}),
            WAIT,
        )
        .await
        .unwrap();
    assert_eq!(located["session"]["transcripts"], json!([store]));
    for id in ["probe-3-next", "helper"] {
        let refused = driver
            .call(method::SESSIONS_LOCATE, json!({"external_id": id}), WAIT)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("not-found"), "{refused}");
    }

    let events = read(&driver, COMPACTED).await;
    assert_eq!(
        said(&events, "person"),
        [
            "Use the exec tool to run: echo openclaw-ok. Then reply with its output.",
            "And once more.",
        ]
    );
    let call = of_type(&events, "call_began")[0];
    assert_eq!(call["call"]["tool"], "exec");
    assert_eq!(call["call"]["group"], "shell");
    assert_eq!(call["call"]["input"]["command"], "echo openclaw-ok");
    let ended = of_type(&events, "call_ended")[0];
    assert_eq!(ended["id"], call["call"]["id"]);
    assert_eq!(ended["ok"], true);
    assert_eq!(ended["tail"], "openclaw-ok");
    assert!(said(&events, "agent")[0].contains("openclaw-ok"));
    assert_eq!(of_type(&events, "turn_ended")[0]["state"], "finished");
    let compacted = of_type(&events, "compacted")[0];
    assert_eq!(compacted["tokens_before"], 13023);
    assert_eq!(compacted["tokens_after"], 403);
    assert!(said(&events, "summary")[0].contains("echo openclaw-ok"));
    // The heartbeat's poll begins a turn but says nothing as the person.
    assert_eq!(of_type(&events, "turn_began").len(), 3);

    let events = read(&driver, FAILED).await;
    // The read's result was packed in zstd.
    let ended = of_type(&events, "call_ended")[0];
    assert_eq!(ended["chars"], 6450);
    assert!(said(&events, "tool")[0].starts_with("# AGENTS.md - Your Workspace"));
    let last = of_type(&events, "turn_ended").pop().unwrap().clone();
    assert_eq!(last["state"], "failed");
    assert!(
        last["failure"]["line"]
            .as_str()
            .unwrap()
            .contains("couldn't generate a response"),
        "{last}"
    );

    let helper = driver
        .call(
            method::SESSIONS_READ,
            json!({"external_id": COMPACTED, "sub_agent": "helper"}),
            WAIT,
        )
        .await
        .unwrap();
    assert_eq!(
        said(helper["events"].as_array().unwrap(), "person"),
        ["Help."]
    );

    // Drive. A conversation goes on under its key, which OpenClaw's gateway keeps.
    let key = "agent:main:explicit:steery-probe-3";
    let call = |method: &'static str, params: Value| {
        let driver = &driver;
        async move { driver.call(method, params, WAIT).await }
    };
    let held = call(method::HELD, json!({"external_id": COMPACTED}))
        .await
        .unwrap();
    assert!(held["nobody"].is_object(), "{held}");
    // The heartbeat's poll began a turn that never ended.
    let idle = call(method::IDLE, json!({"external_id": COMPACTED}))
        .await
        .unwrap();
    assert_eq!(idle["sample"]["if_unheld"], true, "{idle}");
    assert_eq!(idle["sample"]["state"], "interrupted", "{idle}");

    // A run that another client began writes the key: beyond Steery's stop.
    let db = Connection::open(&store).unwrap();
    db.execute(
        "UPDATE session_nodes SET status = NULL,
                entry_json = '{\"activeWriterRunId\":\"run-1\"}' WHERE session_key = ?1",
        params![key],
    )
    .unwrap();
    let held = call(method::HELD, json!({"external_id": COMPACTED}))
        .await
        .unwrap();
    assert_eq!(
        held["person"]["stop"]["beyond"], "OpenClaw's gateway runs a turn of it",
        "{held}"
    );
    let idle = call(method::IDLE, json!({"external_id": COMPACTED}))
        .await
        .unwrap();
    assert_eq!(idle["sample"]["state"], "running", "{idle}");
    db.execute(
        "UPDATE session_nodes SET status = 'done' WHERE session_key = ?1",
        params![key],
    )
    .unwrap();

    // Without `openclaw` nothing starts; with it, a continue names the key and a fork is refused.
    let asked = |act: &str, id: &str| {
        json!({"act": act, "external_id": id, "cwd": "/elsewhere", "folder": root.join("turn"),
               "worktree": false})
    };
    let missing = call(method::START, asked("continue", COMPACTED))
        .await
        .unwrap_err();
    assert!(missing.to_string().contains("no-binary"), "{missing}");
    let bin = root.join(".npm-global/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("openclaw"), "#!/bin/sh\n").unwrap();
    let mut permissions = std::fs::metadata(bin.join("openclaw"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(bin.join("openclaw"), permissions).unwrap();
    let launch = call(method::START, asked("continue", COMPACTED))
        .await
        .unwrap();
    let argv: Vec<&str> = launch["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|arg| arg.as_str().unwrap())
        .collect();
    assert!(
        argv.windows(2).any(|pair| pair == ["continue", key]),
        "{launch}"
    );
    let fork = call(method::START, asked("fork", COMPACTED))
        .await
        .unwrap_err();
    assert!(fork.to_string().contains("selection-unsupported"), "{fork}");
    let unknown = call(method::START, asked("continue", "nope"))
        .await
        .unwrap_err();
    assert!(unknown.to_string().contains("not-found"), "{unknown}");

    // The turn's prompt names the key; Steery reads the conversation back by its first window.
    let prompt = json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                        "params": {"sessionId": key, "prompt": []}});
    let line = call(
        method::TURN_READ,
        json!({"turn": "t", "stream": "stdout", "line": prompt.to_string()}),
    )
    .await
    .unwrap();
    assert_eq!(line["named"], COMPACTED, "{line}");
    // A session `openclaw acp` made anew is named by the id it drew, under its bridge key.
    let prompt = json!({"jsonrpc": "2.0", "id": 3, "method": "session/prompt",
                        "params": {"sessionId": BRIDGE_ACP, "prompt": []}});
    let line = call(
        method::TURN_READ,
        json!({"turn": "t", "stream": "stdout", "line": prompt.to_string()}),
    )
    .await
    .unwrap();
    assert_eq!(line["named"], BRIDGED, "{line}");

    // A command approval that `openclaw acp` relays is the `exec` call it asks for.
    let permission = json!({
        "sessionId": key,
        "toolCall": {"toolCallId": "call_1", "title": "echo hi", "kind": "execute",
                     "status": "pending",
                     "rawInput": {"name": "exec", "approvalId": "a1", "command": "echo hi",
                                  "host": "gateway"}},
        "options": [
            {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
            {"optionId": "allow-always", "name": "Always allow", "kind": "allow_always"},
            {"optionId": "deny", "name": "Deny", "kind": "reject_once"},
        ],
    });
    let moment = call(
        method::HOOK_READ,
        json!({"event": "session/request_permission", "document": permission.to_string()}),
    )
    .await
    .unwrap()["moment"]
        .clone();
    assert_eq!(moment["external_id"], COMPACTED, "{moment}");
    assert_eq!(moment["cwd"], "/tmp/oc-work");
    assert_eq!(moment["call"]["tool"], "exec");
    assert_eq!(moment["call"]["group"], "shell");
    assert_eq!(
        moment["call"]["input"],
        json!({"command": "echo hi", "host": "gateway"})
    );
    assert_eq!(moment["suggestions"][0]["label"], "Always allow");

    // A delivery is found once among the person's words after the mark.
    let mark = call(method::RECEIPT_MARK, json!({"external_id": COMPACTED}))
        .await
        .unwrap()["mark"]
        .clone();
    let find = |mark: Value| json!({"external_id": COMPACTED, "mark": mark, "delivery": "d1"});
    let before = call(method::RECEIPT_FIND, find(mark.clone()))
        .await
        .unwrap();
    assert_eq!(before["found"], false, "{before}");
    record(
        &db,
        "probe-3-next",
        3,
        json!({"type": "message", "id": "m-said", "parentId": "m-poll",
               "message": {"role": "user", "content": "Go on. steery-delivery:d1",
                           "timestamp": 1794487293000u64}}),
    );
    let after = call(method::RECEIPT_FIND, find(mark)).await.unwrap();
    assert_eq!(after["found"], true, "{after}");

    let terminal = call(
        method::TERMINAL,
        json!({"external_id": COMPACTED, "cwd": "/elsewhere"}),
    )
    .await
    .unwrap();
    assert_eq!(
        terminal["command"],
        format!("cd '/tmp/oc-work' && openclaw tui --session '{key}'")
    );

    let _ = std::fs::remove_dir_all(&root);
}
