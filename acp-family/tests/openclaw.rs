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
    assert_eq!(ids, [COMPACTED, FAILED], "{listed}");
    assert_eq!(sessions[0]["cwd"], "/tmp/oc-work");
    assert_eq!(sessions[0]["updated_at"], 1794487290000u64);
    assert_eq!(sessions[0]["sub_agents"][0]["external_id"], "helper");

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

    let _ = std::fs::remove_dir_all(&root);
}
