//! The Hermes driver, run as the runtime runs it, over standard input and output, against a
//! Hermes home of its own: a `state.db` holding a conversation compacted in place and then
//! rotated into a continuation, a sub-agent it delegated to and a run with no person; and a
//! `config.yaml` whose MCP servers carry secrets spelled `SECRET`; and, where Hermes's
//! installer puts it, a `hermes` that answers `--version`.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use steery_sdk::client::Client;
use steery_sdk::{method, wire};

const WAIT: Duration = Duration::from_secs(30);

/// The columns of Hermes's tables that the driver reads (`hermes_state_common.py`).
const SCHEMA: &str = "
CREATE TABLE sessions (
    id TEXT PRIMARY KEY, source TEXT NOT NULL, created_source TEXT, parent_session_id TEXT,
    model TEXT, billing_provider TEXT, model_config TEXT, started_at REAL NOT NULL,
    ended_at REAL, last_activity_at REAL, title TEXT, cwd TEXT,
    input_tokens INTEGER DEFAULT 0, cache_read_tokens INTEGER DEFAULT 0,
    cache_write_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0,
    reasoning_tokens INTEGER DEFAULT 0);
CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL,
    content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT, timestamp REAL NOT NULL,
    finish_reason TEXT, reasoning TEXT, reasoning_content TEXT,
    _compressed_summary INTEGER NOT NULL DEFAULT 0, active INTEGER NOT NULL DEFAULT 1,
    compacted INTEGER NOT NULL DEFAULT 0, display_kind TEXT, display_metadata TEXT,
    api_content TEXT, message_uid TEXT);
";

struct Message<'a> {
    session: &'a str,
    role: &'a str,
    content: &'a str,
    uid: &'a str,
    at: f64,
}

fn message(db: &Connection, m: Message, extra: &[(&str, rusqlite::types::Value)]) -> i64 {
    db.execute(
        "INSERT INTO messages (session_id, role, content, message_uid, timestamp)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![m.session, m.role, m.content, m.uid, m.at],
    )
    .unwrap();
    let id = db.last_insert_rowid();
    for (column, value) in extra {
        db.execute(
            &format!("UPDATE messages SET {column} = ?1 WHERE id = ?2"),
            params![value, id],
        )
        .unwrap();
    }
    id
}

fn text(value: &str) -> rusqlite::types::Value {
    rusqlite::types::Value::Text(value.to_owned())
}

fn int(value: i64) -> rusqlite::types::Value {
    rusqlite::types::Value::Integer(value)
}

fn call(id: &str, name: &str, arguments: Value) -> String {
    json!([{"id": id, "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()}}])
    .to_string()
}

/// Hermes's home, as a conversation with each kind of row leaves it.
fn home(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("config.yaml"),
        "model:\n  default: m1\nmcp_servers:\n  steery:\n    url: https://user:SECRET@mcp.steery.dev/mcp?token=SECRET\n    headers:\n      Authorization: Bearer SECRET\n  files:\n    command: npx\n    env:\n      KEY: SECRET\n  off:\n    url: http://127.0.0.1:9/x\n    enabled: false\n",
    )
    .unwrap();
    let hermes = root.join(".local/bin/hermes");
    std::fs::create_dir_all(hermes.parent().unwrap()).unwrap();
    std::fs::write(
        &hermes,
        "#!/bin/sh\necho 'Hermes Agent v0.23.1 (2026.9.24)'\n",
    )
    .unwrap();
    std::fs::set_permissions(&hermes, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let db = Connection::open(root.join("state.db")).unwrap();
    db.execute_batch(SCHEMA).unwrap();
    let session = |id: &str, source: &str, parent: Option<&str>, config: Value, at: f64| {
        db.execute(
            "INSERT INTO sessions (id, source, parent_session_id, model, billing_provider,
                                   model_config, started_at, ended_at, title, cwd,
                                   input_tokens, cache_read_tokens, output_tokens)
             VALUES (?1, ?2, ?3, 'm1', 'nous', ?4, ?5, ?6, 'Fix the build', '/work', 100, 40, 7)",
            params![id, source, parent, config.to_string(), at, at + 50.0],
        )
        .unwrap();
    };
    session("conv", "cli", None, json!({}), 1000.0);
    session(
        "conv-2",
        "cli",
        None,
        json!({"_compressed_from": "conv"}),
        1030.0,
    );
    session(
        "child",
        "subagent",
        Some("conv"),
        json!({"_delegate_from": "conv", "_subagent_id": "sa-1"}),
        1006.0,
    );
    session("one", "oneshot", None, json!({}), 1040.0);

    let m = |session, role, content, uid, at| Message {
        session,
        role,
        content,
        uid,
        at,
    };
    // A turn with a call to Steery's own MCP tool, compacted in place below.
    let compacted = [("active", int(0)), ("compacted", int(1))];
    message(&db, m("conv", "user", "hello", "u1", 1001.0), &compacted);
    message(
        &db,
        m("conv", "assistant", "", "a1", 1002.0),
        &[
            (
                "tool_calls",
                text(&call(
                    "c1",
                    "mcp__steery__requests_create",
                    json!({"title": "t"}),
                )),
            ),
            ("reasoning", text("think first")),
            ("active", int(0)),
            ("compacted", int(1)),
        ],
    );
    message(
        &db,
        m("conv", "tool", "created", "t1", 1003.0),
        &[
            ("tool_call_id", text("c1")),
            ("tool_name", text("mcp__steery__requests_create")),
            ("active", int(0)),
            ("compacted", int(1)),
        ],
    );
    message(
        &db,
        m("conv", "assistant", "done", "a2", 1004.0),
        &[
            ("finish_reason", text("stop")),
            ("active", int(0)),
            ("compacted", int(1)),
        ],
    );
    // Words the person took back: never read.
    message(
        &db,
        m("conv", "user", "taken back", "u-gone", 1004.5),
        &[("active", int(0))],
    );
    // A delegation and a failed shell call.
    message(&db, m("conv", "user", "delegate", "u2", 1005.0), &[]);
    message(
        &db,
        m("conv", "assistant", "", "a3", 1005.5),
        &[(
            "tool_calls",
            text(&call("c2", "delegate_task", json!({"goal": "child"}))),
        )],
    );
    message(
        &db,
        m(
            "conv",
            "tool",
            r#"{"status": "dispatched", "subagent_ids": ["sa-1"]}"#,
            "t2",
            1006.5,
        ),
        &[
            ("tool_call_id", text("c2")),
            ("tool_name", text("delegate_task")),
        ],
    );
    message(
        &db,
        m("conv", "assistant", "", "a4", 1007.0),
        &[(
            "tool_calls",
            text(&call("c3", "terminal", json!({"command": "false"}))),
        )],
    );
    message(
        &db,
        m(
            "conv",
            "tool",
            r#"{"output": "", "exit_code": 1, "error": null}"#,
            "t3",
            1008.0,
        ),
        &[
            ("tool_call_id", text("c3")),
            ("tool_name", text("terminal")),
        ],
    );
    message(
        &db,
        m(
            "conv",
            "assistant",
            "Your request was not processed.",
            "a5",
            1009.0,
        ),
        &[
            ("display_kind", text("failed_turn")),
            (
                "display_metadata",
                text(
                    &json!({"error": "HTTP 429: slow down",
                            "error_surface": {"layer": "provider", "code": "rate_limit",
                                              "provider": "nous"}})
                    .to_string(),
                ),
            ),
        ],
    );
    // An interrupted turn: Hermes closes it with a hidden stand-in.
    message(&db, m("conv", "user", "stop me", "u3", 1010.0), &[]);
    message(
        &db,
        m("conv", "assistant", "", "a6", 1011.0),
        &[
            ("display_kind", text("hidden")),
            (
                "api_content",
                text("[interrupt: no assistant output for this turn]"),
            ),
        ],
    );
    // The in-place compaction: its summary keeps the uid of the message it is written over,
    // then the kept tail again under its old uid.
    message(
        &db,
        m("conv", "user", "SUMMARY of the earlier turns", "a2", 1004.0),
        &[("_compressed_summary", int(1))],
    );
    message(&db, m("conv", "user", "stop me", "u3", 1010.0), &[]);
    // The continuation the conversation rotated into.
    message(
        &db,
        m("conv-2", "user", "after compress", "u4", 1031.0),
        &[],
    );
    message(
        &db,
        m("conv-2", "assistant", "fine", "a7", 1032.0),
        &[("finish_reason", text("stop"))],
    );
    // The sub-agent's own rows.
    message(
        &db,
        m("child", "user", "Say hello\nfrom the child", "cu1", 1006.1),
        &[],
    );
    message(
        &db,
        m("child", "assistant", "hello", "ca1", 1006.2),
        &[("finish_reason", text("stop"))],
    );
    message(&db, m("one", "user", "one shot", "ou1", 1041.0), &[]);
}

async fn start(root: &Path, protocol: u32) -> Client {
    let client = Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", root.display()),
            format!("HERMES_HOME={}", root.display()),
            env!("CARGO_BIN_EXE_steery-driver-hermes").into(),
        ],
        None,
    )
    .await
    .unwrap();
    let offered = wire::InitializeParams {
        protocol: vec![protocol],
        runtime: wire::Runtime {
            release: "7.0.0".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        },
        home: root.to_owned(),
        config: root.join(".config/steery"),
        settings: Default::default(),
    };
    assert_eq!(
        client.initialize(&offered).await.unwrap().protocol,
        protocol
    );
    client
}

fn words(events: &[Value]) -> Vec<(String, String)> {
    events
        .iter()
        .filter(|event| event["type"] == "words")
        .map(|event| {
            (
                event["voice"].as_str().unwrap().to_owned(),
                event["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn hermes_reads_its_conversations_from_state_db() {
    let root = std::env::temp_dir().join(format!("steery-hermes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    home(&root);
    let driver = start(&root, steery_sdk::YAML).await;

    let listed = driver
        .call(method::SESSIONS_LIST, json!({"transcripts": []}), WAIT)
        .await
        .unwrap();
    assert_eq!(listed["excluded"], json!(["one"]));
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{listed}");
    let session = &sessions[0];
    assert_eq!(session["external_id"], "conv");
    assert_eq!(session["cwd"], "/work");
    assert_eq!(session["sub_agents"][0]["external_id"], "child");
    assert_eq!(session["sub_agents"][0]["started_by"], "c2");
    assert_eq!(session["sub_agents"][0]["task"], "Say hello");

    // The continuation is the same conversation; a sub-agent is not one of its own.
    let located = driver
        .call(
            method::SESSIONS_LOCATE,
            json!({"external_id": "conv-2"}),
            WAIT,
        )
        .await
        .unwrap();
    assert_eq!(located["session"]["external_id"], "conv");
    for id in ["child", "nobody"] {
        let refused = driver
            .call(method::SESSIONS_LOCATE, json!({"external_id": id}), WAIT)
            .await
            .unwrap_err();
        assert!(refused.to_string().contains("not-found"), "{refused}");
    }

    let read = driver
        .call(method::SESSIONS_READ, json!({"external_id": "conv"}), WAIT)
        .await
        .unwrap();
    assert_eq!(read["whole"], true);
    assert_eq!(read["more"], false);
    let events = read["events"].as_array().unwrap();
    assert_eq!(events[0]["type"], "model");
    assert_eq!(events[0]["model"], "m1");
    assert_eq!(
        words(events),
        [
            ("person", "hello"),
            ("thought", "think first"),
            ("tool", "created"),
            ("agent", "done"),
            ("person", "delegate"),
            (
                "tool",
                r#"{"status": "dispatched", "subagent_ids": ["sa-1"]}"#
            ),
            ("tool", r#"{"output": "", "exit_code": 1, "error": null}"#),
            ("person", "stop me"),
            ("summary", "SUMMARY of the earlier turns"),
            ("person", "after compress"),
            ("agent", "fine"),
        ]
        .map(|(voice, text)| (voice.to_owned(), text.to_owned()))
    );
    let mut ids: Vec<&str> = events
        .iter()
        .filter(|event| event["type"] == "words")
        .map(|event| event["id"].as_str().unwrap())
        .collect();
    let all = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), all, "a words id twice");

    let began: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "call_began")
        .collect();
    assert_eq!(began[0]["call"]["group"], "steery");
    assert_eq!(began[0]["call"]["server"], "steery");
    assert_eq!(began[1]["call"]["group"], "agents");
    assert_eq!(began[2]["call"]["group"], "shell");
    let ended: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "call_ended")
        .collect();
    assert_eq!(ended[2]["ok"], false);
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "sub_agent" && e["external_id"] == "child")
    );

    let states: Vec<&Value> = events
        .iter()
        .filter(|e| e["type"] == "turn_ended")
        .map(|e| &e["state"])
        .collect();
    assert_eq!(states, ["finished", "failed", "interrupted", "finished"]);
    let failed = events
        .iter()
        .find(|e| e["type"] == "turn_ended" && e["state"] == "failed")
        .unwrap();
    assert_eq!(failed["failure"]["kind"], "rate_limit");
    assert_eq!(failed["failure"]["line"], "HTTP 429: slow down");
    assert_eq!(
        events.iter().filter(|e| e["type"] == "compacted").count(),
        1
    );
    let tokens = events.last().unwrap();
    assert_eq!(tokens["type"], "tokens");
    assert_eq!(tokens["count"], "total");
    // Both sessions of the chain; Hermes keeps cache reads out of `input_tokens`.
    assert_eq!(tokens["input"], 280);
    assert_eq!(tokens["cached"], 80);

    // Nothing new since the cursor.
    let again = driver
        .call(
            method::SESSIONS_READ,
            json!({"external_id": "conv", "cursor": read["cursor"]}),
            WAIT,
        )
        .await
        .unwrap();
    assert_eq!(again["events"], json!([]));
    assert_eq!(again["reset"], false);

    let child = driver
        .call(
            method::SESSIONS_READ,
            json!({"external_id": "conv", "sub_agent": "child"}),
            WAIT,
        )
        .await
        .unwrap();
    assert_eq!(
        words(child["events"].as_array().unwrap()),
        [("person", "Say hello\nfrom the child"), ("agent", "hello")]
            .map(|(voice, text)| (voice.to_owned(), text.to_owned()))
    );

    let mcp = driver
        .call(method::MCP_LIST, json!({}), WAIT)
        .await
        .unwrap();
    assert_eq!(
        mcp["servers"],
        json!([
            {"server": "files"},
            {"server": "steery", "url": "https://mcp.steery.dev/mcp"},
        ])
    );
    assert!(!mcp.to_string().contains("SECRET"));

    // Picked 14: Steery's server is a `yaml` entry under `mcp_servers.steery`, beside the skill.
    let asked = json!({
        "mcp_url": "http://127.0.0.1:7/mcp",
        "skill": "# Steery",
        "hook": "steery hook hermes",
        "door": "http://127.0.0.1:7/door",
        "release": "7.1.212",
        "timeout": 600,
    });
    let planned = driver
        .call(method::SETUP_PLAN, asked.clone(), WAIT)
        .await
        .unwrap();
    assert_eq!(planned["found"]["release"], "0.23.1", "{planned}");
    assert_eq!(
        planned["entries"],
        json!([
            {
                "path": root.join("skills/steery/SKILL.md"),
                "format": "text",
                "value": "# Steery",
                "carries": ["skill"],
            },
            {
                "path": root.join("config.yaml"),
                "format": "yaml",
                "at": "mcp_servers.steery",
                "value": {
                    "url": "http://127.0.0.1:7/mcp",
                    "headers": {"Steery-Harness": "hermes"},
                    "timeout": 600,
                },
                "carries": ["mcp"],
            },
        ])
    );
    driver.shutdown().await.unwrap();

    // Picked 13: no `yaml` entry, which the runtime would refuse; the plan says the command.
    let older = start(&root, 13).await;
    let planned = older.call(method::SETUP_PLAN, asked, WAIT).await.unwrap();
    let formats: Vec<_> = planned["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["format"].clone())
        .collect();
    assert_eq!(formats, [json!("text")]);
    assert!(
        planned["notes"]
            .to_string()
            .contains("hermes mcp add steery --url http://127.0.0.1:7/mcp"),
        "{planned}"
    );
    older.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
