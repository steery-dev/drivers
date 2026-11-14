//! The Kiro driver, run as the runtime runs it, over standard input and output, against a
//! home of its own that holds real session files of Kiro CLI 3.3.0 (`fixtures/kiro/home`):
//! a conversation started, continued, stopped mid-call and denied a shell command through
//! `kiro-cli acp`; its fork; a `kiro-cli chat` run whose agent handed a task to a sub-agent;
//! one that ran a shell command and read files; and one Kiro opened that no prompt reached.
//! Then the documents Kiro handed its hooks (`fixtures/kiro/hooks`), the permission a turn
//! asked for (`fixtures/kiro/permission.json`), the setup of a home with a `kiro-cli` of its
//! own, and the launch plans of Kiro's turns.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use steery_sdk::client::Client;
use steery_sdk::{method, wire};

const WAIT: Duration = Duration::from_secs(10);

/// `kiro-smoke-ok`, `kiro-continue-ok`, a `sleep 60` stopped mid-call, and a `touch` whose
/// permission Steery's hook denied: four ACP turns.
const DRIVEN: &str = "sess_a456cf0f-d938-41c3-b4c7-848779197679";
/// A fork of [`DRIVEN`] after its first two turns.
const FORK: &str = "sess_aebe1256-7a1a-457d-97f6-47d7aa1d87d2";
/// A `kiro-cli chat` turn whose agent ran a shell command and a sub-agent.
const LEAD: &str = "sess_4390fec6-7476-4612-8a72-e444d748a51d";
/// The sub-agent of [`LEAD`].
const SUB: &str = "d571d276-26bd-46bc-b376-c9805622748b";
/// A `kiro-cli chat` turn that read a missing file, ran `echo hi` and read `a.txt`.
const HOOKED: &str = "sess_661c6ccf-c4e1-404c-91e6-9e2b82039ad2";
/// A conversation Kiro opened that no prompt reached: it holds no `messages.jsonl`.
const UNTYPED: &str = "sess_da721672-2737-41e5-9fc1-12f65dfe29a7";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/kiro")
}

/// A copy of `from` under `to`, every file last changed an hour ago.
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

/// A home of this test's own: the fixture store, a `kiro-cli` of its own that names
/// `release`, and MCP servers of the person and of a project.
fn home(name: &str, release: Option<&str>) -> PathBuf {
    let home = std::env::temp_dir().join(format!("steery-kiro-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    copy(&fixtures().join("home"), &home);
    if let Some(release) = release {
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let kiro = bin.join("kiro-cli");
        std::fs::write(&kiro, format!("#!/bin/sh\necho {release}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&kiro, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let settings = home.join(".kiro/settings");
    std::fs::create_dir_all(&settings).unwrap();
    let own = json!({"mcpServers": {
        "echo": {"command": "/usr/bin/python3", "args": ["/tmp/echo.py"]},
        "notes": {"url": "https://notes.example/mcp"},
    }});
    std::fs::write(settings.join("mcp.json"), own.to_string()).unwrap();
    let project = home.join("work/app/.kiro/settings");
    std::fs::create_dir_all(&project).unwrap();
    let off = json!({"mcpServers": {"notes": {"url": "https://n", "disabled": true}}});
    std::fs::write(project.join("mcp.json"), off.to_string()).unwrap();
    home
}

async fn start(home: &Path) -> Client {
    let client = Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            "PATH=/usr/bin:/bin".into(),
            env!("CARGO_BIN_EXE_steery-driver-kiro").into(),
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
            "sub_agent" => format!("sub_agent {}", event["state"].as_str().unwrap()),
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

fn setup() -> Value {
    json!({
        "mcp_url": "http://127.0.0.1:7777/mcp",
        "skill": "# Steery",
        "hook": "/opt/steery/hook",
        "door": "http://127.0.0.1:7777/door",
        "release": "7.0.0",
        "timeout": 60,
        "trusts": [],
    })
}

#[tokio::test]
async fn reads_sessions_kiro_wrote_and_the_hooks_kiro_runs() {
    let home = home("store", Some("kiro-cli 3.3.0"));
    let client = start(&home).await;

    let listed = call(&client, method::SESSIONS_LIST, json!({"transcripts": []})).await;
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 4, "{listed}");
    assert_eq!(listed["complete"], true);
    let session = |id: &str| {
        sessions
            .iter()
            .find(|session| session["external_id"] == id)
            .unwrap_or_else(|| panic!("no {id}: {listed}"))
            .clone()
    };
    let driven = session(DRIVEN);
    assert_eq!(driven["cwd"], "/private/tmp/kiro-smoke");
    assert_eq!(driven["title"], "kiro-smoke-ok");
    assert_eq!(driven["created_at"], 1_794_653_247_658_u64);
    assert!(driven.get("parent").is_none_or(Value::is_null));
    let transcript = home.join(format!(
        ".kiro/sessions/1b51937985dbf6f7/{DRIVEN}/messages.jsonl"
    ));
    assert_eq!(driven["transcripts"], json!([transcript]));
    assert_eq!(session(FORK)["parent"], DRIVEN);
    let lead = session(LEAD);
    let subs = lead["sub_agents"].as_array().unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0]["external_id"], SUB);
    assert_eq!(subs[0]["name"], "general-task-execution");

    let located = call(
        &client,
        method::SESSIONS_LOCATE,
        json!({"external_id": FORK}),
    )
    .await;
    assert_eq!(located["session"], session(FORK));
    let missing = client
        .call(method::SESSIONS_READ, json!({"external_id": UNTYPED}), WAIT)
        .await;
    assert!(
        missing.is_err(),
        "a conversation no person typed into is not read"
    );

    // Four turns: two that answered, one stopped in its call, one whose call was denied.
    let read = events(&client, DRIVEN).await;
    assert_eq!(
        lines(&read),
        [
            "model",
            "words person",
            "turn_began",
            "words agent",
            "turn_ended finished",
            "words person",
            "turn_began",
            "words agent",
            "turn_ended finished",
            "words person",
            "turn_began",
            "call_began shell",
            "call_ended",
            "turn_ended interrupted",
            "words person",
            "turn_began",
            "call_began shell",
            "call_ended",
            "words agent",
            "turn_ended finished",
        ]
    );
    assert_eq!(read[0]["model"], "claude-haiku-4.5");
    assert_eq!(read[11]["call"]["input"]["command"], "sleep 60");
    assert_eq!(read[12]["ok"], false);
    assert_eq!(read[17]["ok"], false);
    assert!(
        read[17]["tail"]
            .as_str()
            .unwrap()
            .contains("The user rejected this tool call."),
        "{}",
        read[17]
    );

    // The fork reads as its own conversation: the history it copied, then its own turn.
    let fork = events(&client, FORK).await;
    let at = fork
        .iter()
        .position(|event| event["type"] == "fork")
        .unwrap();
    assert_eq!(fork[at]["parent"], DRIVEN);
    assert_eq!(lines(&fork[..at]), lines(&read[..9]));
    assert_eq!(person(&fork[at..]), ["Reply with exactly: kiro-fork-ok"]);

    // A sub-agent's work, in the lead and in its own file.
    let led = events(&client, LEAD).await;
    assert!(
        lines(&led).contains(&"sub_agent running".to_owned()),
        "{led:?}"
    );
    assert!(lines(&led).contains(&"sub_agent finished".to_owned()));
    let sub = call(
        &client,
        method::SESSIONS_READ,
        json!({"external_id": LEAD, "sub_agent": SUB}),
    )
    .await;
    assert!(
        sub["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["voice"] == "agent"),
        "{sub}"
    );

    // The hooks' documents, as Kiro handed them in this conversation.
    let hooked = events(&client, HOOKED).await;
    let mut moments = Vec::new();
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PostToolUse",
        "Stop",
        "SessionEnd",
    ] {
        let document =
            std::fs::read_to_string(fixtures().join(format!("hooks/{event}.json"))).unwrap();
        let read = call(
            &client,
            method::HOOK_READ,
            json!({"event": event, "document": document, "env": {}}),
        )
        .await;
        let moment = read["moment"].clone();
        assert_eq!(moment["event"], event, "{read}");
        assert_eq!(moment["external_id"], HOOKED);
        assert_eq!(moment["cwd"], "/private/tmp/kiro-w2");
        moments.push(moment);
    }
    assert_eq!(
        moments[1]["prompt"],
        "Run the shell command: echo hi. Then read a.txt. Then answer: ok."
    );
    let first_read = hooked
        .iter()
        .find(|event| event["type"] == "call_began")
        .unwrap();
    assert_eq!(moments[2]["call"]["tool"], "read_file");
    assert_eq!(moments[2]["call"]["group"], "files-read");
    assert_eq!(
        moments[2]["call"]["id"], first_read["call"]["id"],
        "the store names the call"
    );
    // Steery gates no call before it runs: a turn's own permission asks do that.
    let pre = std::fs::read_to_string(fixtures().join("hooks/PreToolUse.json")).unwrap();
    let unhooked = call(
        &client,
        method::HOOK_READ,
        json!({"event": "PreToolUse", "document": pre, "env": {}}),
    )
    .await;
    assert!(unhooked.get("moment").is_none(), "{unhooked}");

    // Steery's answer at `Stop`, printed as Kiro reads it.
    let stop = std::fs::read_to_string(fixtures().join("hooks/Stop.json")).unwrap();
    let answered = call(
        &client,
        method::HOOK_ANSWER,
        json!({"event": "Stop", "document": stop, "env": {}, "answer": {"say": "go on"}}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(answered["stdout"].as_str().unwrap()).unwrap(),
        json!({"decision": "block", "reason": "go on"})
    );

    // The permission a turn asked for, and Steery's answers to it.
    let permission = std::fs::read_to_string(fixtures().join("permission.json")).unwrap();
    let asked = call(
        &client,
        method::HOOK_READ,
        json!({"event": "session/request_permission", "document": permission, "env": {}}),
    )
    .await;
    let call_asked = &asked["moment"]["call"];
    assert_eq!(asked["moment"]["external_id"], DRIVEN);
    assert_eq!(call_asked["tool"], "run_command");
    assert_eq!(call_asked["group"], "shell");
    assert_eq!(
        call_asked["input"]["command"],
        "touch /tmp/kiro-smoke/denied.txt"
    );
    for (decision, option) in [
        (json!({"deny": {"reason": "no"}}), "reject"),
        (json!({"allow": {"reason": "yes"}}), "accept"),
    ] {
        let answered = call(
            &client,
            method::HOOK_ANSWER,
            json!({"event": "session/request_permission", "document": permission, "env": {},
                   "answer": {"decision": decision}}),
        )
        .await;
        assert_eq!(
            serde_json::from_str::<Value>(answered["stdout"].as_str().unwrap()).unwrap(),
            json!({"outcome": {"outcome": "selected", "optionId": option}})
        );
    }

    // Setup with a `kiro-cli` 3.3.0 of this home's own.
    let plan = call(&client, method::SETUP_PLAN, setup()).await;
    assert_eq!(plan["found"]["release"], "3.3.0", "{plan}");
    assert!(plan.get("refused").is_none() && plan.get("install").is_none());
    let kiro = home.join(".kiro");
    let entries = plan["entries"].as_array().unwrap();
    let mcp = entries
        .iter()
        .find(|entry| entry["at"] == "/mcpServers/steery")
        .unwrap_or_else(|| panic!("no MCP entry: {plan}"));
    assert_eq!(mcp["path"], json!(kiro.join("settings/mcp.json")));
    assert_eq!(mcp["value"]["url"], "http://127.0.0.1:7777/mcp");
    let skill = entries
        .iter()
        .find(|entry| entry["path"] == json!(kiro.join("skills/steery/SKILL.md")))
        .unwrap();
    assert_eq!(skill["value"], "# Steery");
    assert!(
        entries
            .iter()
            .any(|entry| entry["path"] == json!(kiro.join("hooks/steery.json"))),
        "{plan}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry["path"] == json!(kiro.join("steery/hook"))),
        "the turn's permission hook: {plan}"
    );

    // The person's servers, with a project's `.kiro/settings/mcp.json` turning one off.
    let servers = call(
        &client,
        method::MCP_LIST,
        json!({"cwd": home.join("work/app")}),
    )
    .await;
    let names: Vec<_> = servers["servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|server| server["server"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["echo"], "{servers}");
    let watch = servers["watch"].as_array().unwrap();
    assert!(watch.contains(&json!(home.join("work/app/.kiro/settings/mcp.json"))));
    assert!(watch.contains(&json!(kiro.join("settings/mcp.json"))));

    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn plans_an_install_or_an_update() {
    let missing = home("missing", None);
    let client = start(&missing).await;
    let plan = call(&client, method::SETUP_PLAN, setup()).await;
    assert!(plan.get("found").is_none(), "{plan}");
    assert_eq!(plan["entries"], json!([]));
    assert_eq!(
        plan["install"]["argv"],
        json!([
            "bash",
            "-c",
            "curl -fsSL https://cli.kiro.dev/install | bash"
        ])
    );
    let _ = std::fs::remove_dir_all(&missing);

    let old = home("old", Some("kiro-cli 3.2.9"));
    let client = start(&old).await;
    let plan = call(&client, method::SETUP_PLAN, setup()).await;
    assert_eq!(plan["found"]["release"], "3.2.9");
    assert!(
        plan["refused"]
            .as_str()
            .unwrap()
            .contains("kiro-cli update"),
        "{plan}"
    );
    assert_eq!(plan["entries"], json!([]));
    let _ = std::fs::remove_dir_all(&old);
}

/// Steery plans Kiro's turns as this driver's own turn mode against `kiro-cli acp`, and
/// reads what such a turn wrote: the turn whose `touch` Steery's hook denied.
#[tokio::test]
async fn starts_and_reads_kiro_turns() {
    let home = home("drive", Some("kiro-cli 3.3.0"));
    let kiro = home.join(".local/bin/kiro-cli");
    let client = start(&home).await;
    let ask = |params: Value| {
        let client = &client;
        async move { client.call(method::START, params, WAIT).await }
    };
    let cwd = std::env::temp_dir();
    let driver = env!("CARGO_BIN_EXE_steery-driver-kiro");

    let launched = ask(json!({"act": "new", "cwd": cwd, "folder": cwd, "worktree": false}))
        .await
        .unwrap();
    assert_eq!(launched["argv"][0], driver, "{launched}");
    assert_eq!(launched["argv"][1], "turn");
    let argv: Vec<_> = launched["argv"].as_array().unwrap()[2..].to_vec();
    assert_eq!(argv[..3], [json!("--binary"), json!(kiro), json!("new")]);
    assert_eq!(launched["stdin"], "words");
    assert!(
        !argv.contains(&json!("--hook")),
        "no hook is set up yet: {launched}"
    );

    // With Steery's permission hook set up, the turn answers Kiro's asks through it.
    let hook = home.join(".kiro/steery/hook");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(&hook, "/opt/steery/hook").unwrap();
    let resumed = ask(json!({"act": "continue", "external_id": DRIVEN, "cwd": cwd,
        "folder": cwd, "worktree": false}))
    .await
    .unwrap();
    let argv = resumed["argv"].as_array().unwrap();
    assert_eq!(argv[4..6], [json!("continue"), json!(DRIVEN)], "{resumed}");
    assert_eq!(resumed["cwd"], json!(cwd));
    let at = argv.iter().position(|arg| arg == "--hook").unwrap();
    assert_eq!(argv[at + 1], "/opt/steery/hook");
    let forked = ask(json!({"act": "fork", "external_id": DRIVEN, "cwd": cwd,
        "folder": cwd, "worktree": false}))
    .await
    .unwrap();
    assert_eq!(forked["argv"][4], "fork", "{forked}");

    for (params, word) in [
        (
            json!({"act": "continue", "external_id": "sess_00000000-0000-4000-8000-00000000abcd",
                   "cwd": cwd, "folder": cwd, "worktree": false}),
            "not-found",
        ),
        (
            json!({"act": "new", "cwd": cwd, "folder": cwd, "worktree": true}),
            "selection-unsupported",
        ),
    ] {
        let error = ask(params).await.unwrap_err().to_string();
        assert!(error.contains(word), "{word}: {error}");
    }
    let terminal = call(
        &client,
        method::TERMINAL,
        json!({"external_id": LEAD, "cwd": cwd}),
    )
    .await;
    assert_eq!(
        terminal["command"],
        format!("cd '/private/tmp/kiro-w2' && kiro-cli chat --resume-id '{LEAD}'")
    );
    let held = call(&client, method::HELD, json!({"external_id": DRIVEN})).await;
    assert!(held.get("nobody").is_some(), "{held}");

    let text = std::fs::read_to_string(fixtures().join("turns/denied.stdout.jsonl")).unwrap();
    let mut read = Vec::new();
    for line in text.lines() {
        read.push(
            call(
                &client,
                method::TURN_READ,
                json!({"turn": "denied", "stream": "stdout", "line": line}),
            )
            .await,
        );
    }
    let named: Vec<_> = read.iter().filter_map(|line| line.get("named")).collect();
    assert_eq!(named, [&json!(DRIVEN)]);
    let ends: Vec<_> = read.iter().filter_map(|line| line.get("ended")).collect();
    assert_eq!(ends, [&json!({"state": "finished"})]);
    let _ = std::fs::remove_dir_all(&home);
}
