//! The OpenHands driver, run as the runtime runs it, over standard input and output, against a
//! home of its own that holds real conversations of OpenHands 1.16.0 on local Ollama models
//! (`fixtures/openhands/home`; `agent.agent_context`, the skill texts the driver never reads,
//! cut from each `base_state.json`): a conversation this driver's turns ran, its first
//! batch of actions allowed and its continued turn's rejected; a turn whose calls failed
//! validation; a turn whose model does not exist; a turn stopped while the model wrote; and
//! a conversation no prompt reached. Then the permission OpenHands asked in the continued
//! turn, with the conversation as it stood at that moment (`fixtures/openhands/hooks`), the
//! setup of a home with an `openhands` of its own, and the launch of a turn.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use steery_sdk::client::Client;
use steery_sdk::{method, wire};

const WAIT: Duration = Duration::from_secs(10);

/// `echo smoke > smoke.txt`, allowed; then `Now run ls with the terminal tool.`, rejected.
const DRIVEN: &str = "7be62222-933c-4839-ab17-ea0ece4c4b4c";
/// `echo hi > hello.txt`, with calls that failed OpenHands' validation.
const INVALID: &str = "404f88e4-32d5-4f15-82db-c46bb0f811ec";
/// `Say hi.` on `ollama_chat/nope:1b`.
const FAILED: &str = "b08af9fe-a6b9-4d18-abf4-b39424a1d57e";
/// An essay, its turn stopped while the model wrote.
const STOPPED: &str = "2571949e-936f-4ddd-8025-c0fa37a7bc32";
/// A conversation OpenHands opened that no prompt reached.
const UNTYPED: &str = "9cbb3f15-f649-4167-93f6-6ba5463ac8de";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/openhands")
}

/// A copy of `from` under `to`, every file last changed an hour ago, as a store whose
/// turns stood still.
fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    let old = SystemTime::now() - Duration::from_secs(3600);
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&target)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
    }
}

/// A home of this test's own: the fixture store, and an `openhands` of its own that names
/// `release`.
fn home(name: &str, release: Option<&str>) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "steery-openhands-test-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    copy(&fixtures().join("home"), &home);
    if let Some(release) = release {
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let openhands = bin.join("openhands");
        std::fs::write(&openhands, format!("#!/bin/sh\necho '{release}'\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&openhands, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    home
}

/// The driver, in the time zone the fixtures were written in: OpenHands writes local times
/// without their offset.
async fn start(home: &Path) -> Client {
    let client = Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            "PATH=/usr/bin:/bin".into(),
            "TZ=Europe/Berlin".into(),
            env!("CARGO_BIN_EXE_steery-driver-openhands").into(),
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

async fn call(client: &Client, method: &str, params: Value) -> Value {
    client.call(method, params, WAIT).await.unwrap()
}

/// The refusal a call ends in.
async fn refused(client: &Client, method: &str, params: Value) -> String {
    let error = client.call(method, params, WAIT).await.unwrap_err();
    format!("{error:?}")
}

async fn events(client: &Client, id: &str) -> Vec<Value> {
    let read = call(client, method::SESSIONS_READ, json!({"external_id": id})).await;
    assert_eq!(read["whole"], true, "{read}");
    read["events"].as_array().unwrap().clone()
}

/// Each event as one short line.
fn lines(events: &[Value]) -> Vec<String> {
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

/// What the person said in `events`.
fn person(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter(|event| event["voice"] == "person")
        .map(|event| event["text"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn reads_conversations_openhands_wrote() {
    let home = home("store", None);
    let client = start(&home).await;

    let listed = call(&client, method::SESSIONS_LIST, json!({"transcripts": []})).await;
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 4, "{listed}");
    assert_eq!(listed["excluded"], json!([UNTYPED]), "{listed}");
    let session = |id: &str| {
        sessions
            .iter()
            .find(|session| session["external_id"] == id)
            .unwrap_or_else(|| panic!("no {id}: {listed}"))
            .clone()
    };
    let invalid = session(INVALID);
    assert_eq!(invalid["cwd"], "/tmp/oh-home/proj");
    assert_eq!(
        invalid["title"],
        "Run this shell command with the terminal tool: echo hi > hello.txt"
    );
    // `2026-11-14T10:05:23.346433`, in Berlin.
    assert_eq!(invalid["created_at"], 1_794_647_123_346_u64);
    let folder = home.join(".openhands/conversations/404f88e432d54f1582dbc46bb0f811ec");
    assert_eq!(
        invalid["transcripts"],
        json!([folder.join("base_state.json"), folder.join("events")])
    );
    let located = call(
        &client,
        method::SESSIONS_LOCATE,
        json!({"external_id": DRIVEN}),
    )
    .await;
    assert_eq!(located["session"], session(DRIVEN));

    // Two turns this driver ran: the first batch of actions allowed, the second rejected.
    // OpenHands' nudge to a model that answered with nothing is no word of the person.
    let driven = events(&client, DRIVEN).await;
    let turn = [
        "turn_began",
        "words person",
        "tokens",
        "words thought",
        "call_began agents",
        "tokens",
        "call_ended",
        "words thought",
        "call_began harness",
        "tokens",
        "call_ended",
        "turn_ended finished",
    ];
    let mut expected = vec!["model"];
    expected.extend(turn);
    expected.extend(turn);
    assert_eq!(lines(&driven), expected);
    assert_eq!(driven[0]["model"], "ollama_chat/qwen3:4b");
    assert_eq!(
        person(&driven),
        [
            "Use the terminal tool to run: echo smoke > smoke.txt. Then reply done.",
            "Now run ls with the terminal tool.",
        ]
    );
    assert_eq!(driven[3]["input"], 2050);
    assert_eq!(driven[3]["output"], 1990);
    assert_eq!(driven[5]["call"]["tool"], "task");
    assert_eq!(driven[7]["id"], driven[5]["call"]["id"]);
    assert_eq!(driven[7]["ok"], true);
    assert_eq!(driven[9]["call"]["tool"], "finish");
    let rejected = &driven[19];
    assert_eq!(rejected["id"], driven[17]["call"]["id"]);
    assert_eq!(rejected["ok"], false);
    assert_eq!(
        rejected["tail"],
        "User rejected the action. Please ask the user how they want to proceed."
    );

    // A call OpenHands refused to run ends failed; one it ran ends with its output.
    let invalid = events(&client, INVALID).await;
    let terminal = &invalid[4]["call"];
    assert_eq!(terminal["tool"], "terminal");
    assert_eq!(terminal["group"], "shell");
    assert_eq!(terminal["input"]["command"], "echo hi > hello.txt");
    assert_eq!(invalid[6]["ok"], false);
    assert!(
        invalid[6]["tail"]
            .as_str()
            .unwrap()
            .starts_with("Error validating tool 'terminal'"),
        "{}",
        invalid[6]
    );
    assert_eq!(
        lines(&invalid).last().unwrap(),
        "turn_ended finished",
        "{invalid:?}"
    );

    let failed = events(&client, FAILED).await;
    assert_eq!(
        lines(&failed),
        ["model", "turn_began", "words person", "turn_ended failed"]
    );
    let failure = &failed[3]["failure"];
    assert_eq!(failure["code"], "NotFoundError");
    assert_eq!(failure["service"], true);
    assert!(
        failure["line"]
            .as_str()
            .unwrap()
            .contains("model 'nope:1b' not found"),
        "{failure}"
    );

    // Stopped as the model wrote: `base_state.json` still says it runs, so the turn stays
    // open, and `idle` reads it cut off.
    let stopped = events(&client, STOPPED).await;
    assert_eq!(lines(&stopped), ["model", "turn_began", "words person"]);
    let held = json!({"external_id": STOPPED});
    let idle = call(&client, method::IDLE, held.clone()).await;
    assert_eq!(idle["sample"]["state"], "interrupted", "{idle}");
    assert_eq!(idle["sample"]["started_at"], stopped[1]["at"]);
    let finished = call(&client, method::IDLE, json!({"external_id": DRIVEN})).await;
    assert_eq!(finished["sample"]["state"], "finished", "{finished}");
    // An ACP session opened and never prompted is no conversation a person typed into.
    let untyped = call(&client, method::IDLE, json!({"external_id": UNTYPED})).await;
    assert_eq!(untyped, json!({}));
    let look = call(&client, method::HELD, held).await;
    let folder = home.join(".openhands/conversations/2571949e936f4ddd8025c0fa37a7bc32");
    assert_eq!(
        look["look"]["transcripts"],
        json!([folder.join("base_state.json"), folder.join("events")])
    );
    assert_eq!(look["look"]["beyond"][0]["arg"], "acp");

    let terminal = call(
        &client,
        method::TERMINAL,
        json!({"external_id": STOPPED, "cwd": "/tmp"}),
    )
    .await;
    assert_eq!(
        terminal["command"],
        format!("cd '/private/tmp/oh-home/proj' && openhands --resume '{STOPPED}'")
    );

    let _ = std::fs::remove_dir_all(&home);
}

/// The permission OpenHands asked of a turn this driver ran, read against the conversation
/// as it stood then: the call it asks for is the action OpenHands wrote before it asked.
#[tokio::test]
async fn reads_the_permission_openhands_asks() {
    let home = home("hooks", None);
    let conversations = home.join(".openhands/conversations");
    let id = "7be62222933c4839ab17ea0ece4c4b4c";
    std::fs::remove_dir_all(conversations.join(id)).unwrap();
    copy(&fixtures().join("hooks").join(id), &conversations.join(id));
    let client = start(&home).await;

    let document =
        std::fs::read_to_string(fixtures().join("hooks/request_permission.json")).unwrap();
    let read = call(
        &client,
        method::HOOK_READ,
        json!({"event": "session/request_permission", "document": document, "env": {}}),
    )
    .await;
    let moment = &read["moment"];
    assert_eq!(moment["external_id"], DRIVEN);
    assert_eq!(moment["cwd"], "/private/tmp/oh-home/proj");
    assert_eq!(moment["call"]["id"], "call_zte5h6da");
    assert_eq!(moment["call"]["tool"], "task");
    assert_eq!(moment["call"]["group"], "agents");
    assert_eq!(moment["call"]["input"]["subagent_type"], "general-purpose");
    assert_eq!(
        moment["suggestions"][0]["label"],
        "Always proceed (don't ask again)"
    );

    let ended = json!({"sessionId": DRIVEN, "stopReason": "end_turn", "continued": false});
    let read = call(
        &client,
        method::HOOK_READ,
        json!({"event": "session/prompt", "document": ended.to_string(), "env": {}}),
    )
    .await;
    assert_eq!(read["moment"]["outcome"], "finished", "{read}");
    let unhooked = call(
        &client,
        method::HOOK_READ,
        json!({"event": "PreToolUse", "document": "{}", "env": {}}),
    )
    .await;
    assert!(unhooked.get("moment").is_none(), "{unhooked}");

    let answer = |decision: Value| {
        json!({"event": "session/request_permission", "document": document, "env": {},
               "answer": {"decision": decision}})
    };
    let allowed = call(
        &client,
        method::HOOK_ANSWER,
        answer(json!({"allow": {"reason": "ok"}})),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(allowed["stdout"].as_str().unwrap()).unwrap(),
        json!({"outcome": {"outcome": "selected", "optionId": "accept"}})
    );
    let denied = call(
        &client,
        method::HOOK_ANSWER,
        answer(json!({"deny": {"reason": "no"}})),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(denied["stdout"].as_str().unwrap()).unwrap(),
        json!({"outcome": {"outcome": "selected", "optionId": "reject"}})
    );

    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn sets_up_and_launches_turns() {
    let home = home("setup", Some("OpenHands CLI 1.16.0"));
    let client = start(&home).await;
    let root = home.join(".openhands");

    let plan = call(
        &client,
        method::SETUP_PLAN,
        json!({
            "mcp_url": "http://127.0.0.1:7777/mcp",
            "skill": "# Steery",
            "hook": "/opt/steery/hook",
            "door": "http://127.0.0.1:7777/door",
            "release": "7.0.0",
            "timeout": 60,
            "trusts": [],
        }),
    )
    .await;
    assert_eq!(plan["found"]["release"], "1.16.0", "{plan}");
    assert_eq!(plan["found"]["root"], json!(root));
    assert!(plan.get("refused").is_none() && plan.get("install").is_none());
    let entries = plan["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3, "{plan}");
    assert_eq!(entries[0]["path"], json!(root.join("mcp.json")));
    assert_eq!(entries[0]["at"], "/mcpServers/steery");
    assert_eq!(entries[0]["value"]["url"], "http://127.0.0.1:7777/mcp");
    assert_eq!(entries[0]["value"]["transport"], "http");
    assert_eq!(entries[1]["path"], json!(root.join("steery/hook")));
    assert_eq!(entries[1]["value"], "/opt/steery/hook");
    assert_eq!(
        entries[2]["path"],
        json!(home.join(".openhands/skills/steery/SKILL.md"))
    );
    assert_eq!(entries[2]["value"], "# Steery");

    let models = call(&client, method::MODELS, json!({})).await;
    assert_eq!(models["signed_in"], true);
    assert_eq!(models["models"][0]["model"], "ollama_chat/qwen3:4b");
    assert_eq!(models["watch"], json!([root.join("agent_settings.json")]));

    // A continue loads the conversation in the folder it ran in, with the hook setup wrote.
    std::fs::create_dir_all(root.join("steery")).unwrap();
    std::fs::write(root.join("steery/hook"), "/opt/steery/hook\n").unwrap();
    let proj = home.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let state = root
        .join("conversations")
        .join(DRIVEN.replace('-', ""))
        .join("base_state.json");
    let mut base: Value = serde_json::from_slice(&std::fs::read(&state).unwrap()).unwrap();
    base["workspace"]["working_dir"] = json!(proj);
    std::fs::write(&state, base.to_string()).unwrap();
    let turn = |act: &str, extra: Value| {
        let mut start = json!({"act": act, "cwd": "/tmp", "folder": "/tmp/turn",
                               "worktree": false, "reads": []});
        start
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        start
    };
    let launch = call(
        &client,
        method::START,
        turn(
            "continue",
            json!({"external_id": DRIVEN, "model": "ollama_chat/qwen3:4b"}),
        ),
    )
    .await;
    let argv: Vec<_> = launch["argv"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .map(|arg| arg.as_str().unwrap())
        .collect();
    let binary = home.join(".local/bin/openhands");
    assert_eq!(
        argv,
        [
            "turn",
            "--binary",
            binary.to_str().unwrap(),
            "continue",
            DRIVEN,
            "--inbox",
            "/tmp/turn/inbox",
            "--hook",
            "/opt/steery/hook",
        ]
    );
    assert_eq!(launch["cwd"], json!(proj));
    assert_eq!(launch["stdin"], "words");

    std::fs::remove_dir(&proj).unwrap();
    let gone = refused(
        &client,
        method::START,
        turn("continue", json!({"external_id": DRIVEN})),
    )
    .await;
    assert!(gone.contains("is gone"), "{gone}");

    for (start, why) in [
        (turn("fork", json!({"external_id": DRIVEN})), "no fork"),
        (
            turn("new", json!({"thinking": "high"})),
            "no thinking level",
        ),
        (
            turn("new", json!({"model": "gpt-5"})),
            "lists no model gpt-5",
        ),
    ] {
        let error = refused(&client, method::START, start).await;
        assert!(error.contains(why), "{why}: {error}");
    }

    // Without a model set up, OpenHands runs nothing.
    std::fs::remove_file(root.join("agent_settings.json")).unwrap();
    let models = call(&client, method::MODELS, json!({})).await;
    assert_eq!(models["signed_in"], false);
    let error = refused(&client, method::START, turn("new", json!({}))).await;
    assert!(error.contains("OpenHands has no model"), "{error}");

    let _ = std::fs::remove_dir_all(&home);
}
