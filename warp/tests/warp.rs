//! The driver, run as the runtime runs it, over standard input and output, against three
//! stores laid out as Warp 2026.11.11 lays them out (`tests/fixtures`, each task encoded
//! from its `.txtpb` with Warp's own `task.proto`): Stable's app store, with a conversation
//! whose agent ran tools and a sub-agent task, started a conversation of its own and is
//! streaming its next answer, beside a conversation Warp hides; Stable's terminal front
//! end's store, with a request that Warp's server failed; and Preview's store, with a
//! conversation older than Warp's `summary` column. A stand-in `oz` on `PATH` answers
//! `--version` only.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::{CallError, Client};
use steery_sdk::{method, wire};

const LEAD: &str = "conv-lead";
const SUB: &str = "task-sub";
const CHILD: &str = "conv-child";
const FAILED: &str = "conv-failed";
const OLD: &str = "conv-old";

/// A channel's state folder as Warp places it on this OS.
fn state(home: &Path, bundle: &str, linux: &str) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Group Containers/2BBY89MBSN.dev.warp/Library/Application Support")
            .join(bundle)
    } else {
        home.join(".local/state").join(linux)
    }
}

/// The store at `path`, holding `fixture`.
fn store(path: &Path, fixture: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let sql = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture),
    )
    .unwrap();
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch(&sql)
        .unwrap();
}

/// A home with Warp installed (the stand-in) and its three stores.
fn home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("steery-warp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        bin.join("oz"),
        "#!/bin/sh\n[ \"$1\" = --version ] && echo 'Oz v0.2026.11.11.08.17.stable_01'\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(bin.join("oz"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let stable = state(&home, "dev.warp.Warp-Stable", "warp-terminal");
    store(&stable.join("warp.sqlite"), "stable.sql");
    store(&stable.join("tui/warp.sqlite"), "tui.sql");
    let preview = state(&home, "dev.warp.Warp-Preview", "warp-terminal-preview");
    store(&preview.join("warp.sqlite"), "preview.sql");
    home
}

/// The driver, in an environment holding only `HOME` and `PATH`, initialized.
async fn start(home: &Path) -> Client {
    let client = Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            format!("PATH={}:/usr/bin:/bin", home.join("bin").display()),
            env!("CARGO_BIN_EXE_steery-driver-warp").into(),
        ],
        None,
    )
    .await
    .unwrap();
    let offered = wire::InitializeParams {
        protocol: vec![12, 13],
        runtime: wire::Runtime {
            release: "7.0.0".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        },
        home: home.to_owned(),
        config: home.join(".config/steery"),
        settings: Default::default(),
    };
    assert_eq!(client.initialize(&offered).await.unwrap().protocol, 13);
    client
}

async fn call(client: &Client, method: &str, params: Value) -> Result<Value, CallError> {
    client.call(method, params, Duration::from_secs(30)).await
}

fn events_of(read: &Value) -> &Vec<Value> {
    read["events"].as_array().unwrap()
}

fn said(read: &Value) -> Vec<&str> {
    events_of(read)
        .iter()
        .filter(|event| event["type"] == "words")
        .map(|event| event["text"].as_str().unwrap())
        .collect()
}

/// Every store's conversations are listed, a sub-agent task and a started conversation
/// under their lead and a hidden one nowhere. A read folds the lead's turns, calls, sub-agent
/// and tokens, and stops before the words Warp still streams; a cursor read is empty; each
/// sub-agent reads alone and locates to its lead; a failed request ends its turn `failed`;
/// a conversation no store has is `not-found`.
#[tokio::test]
async fn warp_lists_and_reads_its_conversations() {
    let home = home("sessions");
    let driver = start(&home).await;

    let listed = call(&driver, method::SESSIONS_LIST, json!({"transcripts": []}))
        .await
        .unwrap();
    assert_eq!(listed["complete"], true, "{listed}");
    let sessions = listed["sessions"].as_array().unwrap();
    let ids: Vec<&str> = sessions
        .iter()
        .map(|session| session["external_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [LEAD, FAILED, OLD], "{listed}");
    let lead = &sessions[0];
    assert_eq!(lead["cwd"], "/work", "{lead}");
    assert_eq!(lead["title"], "List the files", "{lead}");
    let subs = lead["sub_agents"].as_array().unwrap();
    assert_eq!(subs[0]["external_id"], SUB, "{lead}");
    assert_eq!(subs[0]["task"], "Look around", "{lead}");
    assert_eq!(subs[0]["started_by"], "call-sub", "{lead}");
    assert_eq!(subs[1]["external_id"], CHILD, "{lead}");
    assert_eq!(subs[1]["name"], "reviewer", "{lead}");
    assert_eq!(sessions[1]["cwd"], "/tui", "{listed}");
    assert_eq!(sessions[2]["title"], "Explain the old code", "{listed}");
    assert_eq!(sessions[2]["cwd"], "/old", "{listed}");
    assert!(!listed.to_string().contains("conv-hidden"), "{listed}");

    let read = call(&driver, method::SESSIONS_READ, json!({"external_id": LEAD}))
        .await
        .unwrap();
    assert_eq!(read["whole"], true, "{read}");
    assert_eq!(read["more"], false, "{read}");
    assert_eq!(
        said(&read),
        [
            "List the files.",
            "The person wants a listing.",
            "I'll run ls.",
            "a.txt\nb.txt",
            "denied",
            "Two files.",
            "Two files: a.txt and b.txt.",
            "And the sizes?",
        ],
        "{read}"
    );
    let events = events_of(&read);
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds.iter().filter(|kind| **kind == "turn_began").count(),
        2
    );
    assert_eq!(
        kinds.iter().filter(|kind| **kind == "turn_ended").count(),
        1
    );
    assert_eq!(kinds.iter().filter(|kind| **kind == "tokens").count(), 3);
    let model = events
        .iter()
        .find(|event| event["type"] == "model")
        .unwrap();
    assert_eq!(model["model"], "claude-4-sonnet", "{model}");
    let began: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "call_began")
        .map(|event| &event["call"])
        .collect();
    assert_eq!(began[0]["tool"], "run_shell_command", "{read}");
    assert_eq!(began[0]["group"], "shell", "{read}");
    assert_eq!(began[0]["input"], json!({"command": "ls"}), "{read}");
    assert_eq!(began[1]["group"], "steery", "{read}");
    assert_eq!(began[1]["steery"], "sessions_describe", "{read}");
    assert_eq!(began[1]["input"], json!({"text": "Listing"}), "{read}");
    assert_eq!(began[2]["group"], "agents", "{read}");
    let refused = events
        .iter()
        .find(|event| event["type"] == "call_ended" && event["ok"] == false)
        .unwrap();
    assert_eq!(refused["id"], "call-steery", "{refused}");
    let started = events
        .iter()
        .find(|event| event["type"] == "sub_agent")
        .unwrap();
    assert_eq!(started["external_id"], SUB, "{started}");
    assert_eq!(started["call"], "call-sub", "{started}");
    let tokens = events
        .iter()
        .find(|event| event["type"] == "tokens")
        .unwrap();
    assert_eq!(
        (
            &tokens["input"],
            &tokens["cached"],
            &tokens["cache_write"],
            &tokens["output"]
        ),
        (&json!(1200), &json!(300), &json!(50), &json!(80)),
        "{tokens}"
    );
    let ended = events
        .iter()
        .find(|event| event["type"] == "turn_ended")
        .unwrap();
    assert_eq!(ended["state"], "finished", "{ended}");
    assert_eq!(ended["turn"], "m1", "{ended}");
    assert_eq!(kinds.last(), Some(&"words"), "{read}");

    let again = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": LEAD, "cursor": read["cursor"]}),
    )
    .await
    .unwrap();
    assert_eq!(again["events"], json!([]), "{again}");

    for (sub, words) in [
        (SUB, ["List the folder.", "Two files."]),
        (CHILD, ["Review the diff.", "Looks fine."]),
    ] {
        let read = call(
            &driver,
            method::SESSIONS_READ,
            json!({"external_id": LEAD, "sub_agent": sub}),
        )
        .await
        .unwrap();
        assert_eq!(said(&read), words, "{read}");
        let located = call(
            &driver,
            method::SESSIONS_LOCATE,
            json!({"external_id": sub}),
        )
        .await
        .unwrap();
        assert_eq!(located["session"]["external_id"], LEAD, "{located}");
    }

    let read = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": FAILED}),
    )
    .await
    .unwrap();
    let ended = events_of(&read)
        .iter()
        .find(|event| event["type"] == "turn_ended")
        .unwrap();
    assert_eq!(ended["state"], "failed", "{ended}");
    assert_eq!(ended["failure"]["service"], true, "{ended}");

    let read = call(&driver, method::SESSIONS_READ, json!({"external_id": OLD}))
        .await
        .unwrap();
    let states: Vec<&Value> = events_of(&read)
        .iter()
        .filter(|event| event["type"] == "turn_ended")
        .map(|event| &event["state"])
        .collect();
    assert_eq!(states, [&json!("finished")], "{read}");

    let missing = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": "gone"}),
    )
    .await;
    assert!(
        matches!(&missing, Err(CallError::Refused { word, .. }) if word == "not-found"),
        "{missing:?}"
    );
    driver.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

/// The servers of each channel's `.mcp.json`, under the first key Warp reads there, and the
/// servers the person installed that Warp restores running; only a server's bare URL comes
/// back. The plan holds Steery's MCP entry in `~/.warp/.mcp.json`, under the key Warp reads
/// there, and the skill in Warp's own skills folder.
#[tokio::test]
async fn warp_lists_its_servers_and_plans_its_setup() {
    let home = home("mcp");
    let config = home.join(".warp");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join(".mcp.json"),
        json!({"mcp": {"servers": {
            "files": {"command": "npx", "args": ["--token", "SECRET"]},
            "remote": {"url": "https://bot:SECRET@remote.example.com/mcp?k=SECRET"},
        }}, "mcpServers": {"ignored": {"url": "https://ignored.example.com/mcp"}}})
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".warp-preview")).unwrap();
    std::fs::write(
        home.join(".warp-preview/.mcp.json"),
        json!({"servers": {"local": {"url": "http://127.0.0.1:9/sse"}}}).to_string(),
    )
    .unwrap();
    let driver = start(&home).await;

    let listed = call(&driver, method::MCP_LIST, json!({})).await.unwrap();
    assert!(!listed.to_string().contains("SECRET"), "{listed}");
    assert_eq!(
        listed["servers"],
        json!([
            {"server": "files"},
            {"server": "remote", "url": "https://remote.example.com/mcp"},
            {"server": "local", "url": "http://127.0.0.1:9/sse"},
            {"server": "tracker", "url": "https://mcp.example.com/mcp"},
        ]),
        "{listed}"
    );
    assert_eq!(listed["complete"], false, "{listed}");

    let planned = call(
        &driver,
        method::SETUP_PLAN,
        json!({"mcp_url": "http://127.0.0.1:7/mcp", "skill": "# Steery", "hook": "steery hook",
               "door": "http://127.0.0.1:7/door", "release": "7.0.0", "timeout": 60}),
    )
    .await
    .unwrap();
    assert_eq!(planned["found"]["release"], "2026.11.11", "{planned}");
    let entries = planned["entries"].as_array().unwrap();
    assert_eq!(
        entries[0]["path"],
        json!(config.join(".mcp.json")),
        "{planned}"
    );
    assert_eq!(entries[0]["at"], "/mcp/servers/steery", "{planned}");
    assert_eq!(
        entries[0]["value"],
        json!({"url": "http://127.0.0.1:7/mcp"})
    );
    assert_eq!(
        entries[1]["path"],
        json!(config.join("skills/steery/SKILL.md"))
    );
    assert!(planned.get("needs").is_none(), "{planned}");
    driver.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
