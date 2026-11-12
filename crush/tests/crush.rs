//! The driver, run as the runtime runs it, over standard input and output, against two
//! project stores that Crush 0.100.0 wrote (`tests/fixtures`, each a `sqlite3 .dump`): one a
//! live turn with an Ollama model that ran tools, with a sub-agent and Crush's own title
//! session added to it, one a turn that the provider refused. A stand-in `crush` on `PATH`
//! answers `--version`, and `run` as Crush 0.100.0 does: it logs the session it continues
//! and leaves the project's store as `$HOME/answered.db` holds it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::{CallError, Client};
use steery_sdk::{method, wire};

const LEAD: &str = "34ab414b-f784-479a-8b51-ca382fd4c734";
const EARLIER: &str = "dddc5db0-c8d5-45ea-9b8f-69aac032878a";
const SUB: &str = "lead-agent-1$$call_sub1";
const FAILED: &str = "96194384-084a-4271-a8f1-4fe264e9f6d5";

/// A project at `home/<name>` whose store holds `fixture`.
fn project(home: &Path, name: &str, fixture: &str) -> PathBuf {
    let path = home.join(name);
    std::fs::create_dir_all(path.join(".crush")).unwrap();
    let sql = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(fixture),
    )
    .unwrap();
    rusqlite::Connection::open(path.join(".crush/crush.db"))
        .unwrap()
        .execute_batch(&sql)
        .unwrap();
    path
}

/// A home with Crush installed (the stand-in) and the two projects that `projects.json` lists.
fn home(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let home = std::env::temp_dir().join(format!("steery-crush-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        bin.join("crush"),
        format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo 'crush version v0.100.0';;\n\
             run) cat > \"$HOME/words\"; cp \"$HOME/answered.db\" .crush/crush.db; \
             echo 'INFO Continuing session for non-interactive run session_id={LEAD}' >&2;;\n\
             esac\n"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(bin.join("crush"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let work = project(&home, "work", "live.sql");
    let failed = project(&home, "failed", "failed.sql");
    let data = home.join(".local/share/crush");
    std::fs::create_dir_all(&data).unwrap();
    let listed = json!({"projects": [
        {"path": work, "data_dir": work.join(".crush"), "last_accessed": "2026-11-12T14:45:40Z"},
        {"path": failed, "data_dir": failed.join(".crush"), "last_accessed": "2026-11-12T08:48:03Z"},
        {"path": "relative", "data_dir": "relative/.crush", "last_accessed": "2026-11-12T08:48:03Z"},
    ]});
    std::fs::write(data.join("projects.json"), listed.to_string()).unwrap();
    (home, work, failed)
}

/// The driver, in an environment holding only `HOME` and `PATH`, initialized.
async fn start(home: &Path) -> Client {
    let client = Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            format!("PATH={}:/usr/bin:/bin", home.join("bin").display()),
            env!("CARGO_BIN_EXE_steery-driver-crush").into(),
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

fn kinds(read: &Value) -> Vec<&str> {
    read["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect()
}

/// Each project's sessions are listed where they ran, a sub-agent under its lead and Crush's
/// own title session nowhere. A read folds the lead into its turn, calls and tokens; a
/// cursor read is empty; the sub-agent reads alone and locates to its lead; a refused turn
/// ends `failed` with the provider's words; a session no store has is `not-found`.
#[tokio::test]
async fn crush_lists_and_reads_each_projects_sessions() {
    let (home, work, failed) = home("sessions");
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
    assert_eq!(ids, [EARLIER, LEAD, FAILED], "{listed}");
    let lead = &sessions[1];
    assert_eq!(lead["cwd"], json!(work));
    assert_eq!(lead["sub_agents"][0]["external_id"], SUB, "{lead}");
    assert_eq!(lead["sub_agents"][0]["task"], "Look around", "{lead}");
    assert_eq!(sessions[2]["cwd"], json!(failed));
    assert!(!listed.to_string().contains("title-"), "{listed}");

    let read = call(&driver, method::SESSIONS_READ, json!({"external_id": LEAD}))
        .await
        .unwrap();
    assert_eq!(read["whole"], true, "{read}");
    assert_eq!(read["more"], false, "{read}");
    let kinds = kinds(&read);
    for kind in [
        "turn_began",
        "words",
        "model",
        "call_began",
        "call_ended",
        "sub_agent",
        "turn_ended",
        "tokens",
    ] {
        assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    let events = read["events"].as_array().unwrap();
    let bash = events
        .iter()
        .find(|event| event["type"] == "call_began")
        .unwrap();
    assert_eq!(bash["call"]["tool"], "bash", "{bash}");
    assert_eq!(bash["call"]["group"], "shell", "{bash}");
    let refused = events
        .iter()
        .find(|event| event["type"] == "call_ended" && event["ok"] == false)
        .unwrap();
    assert_eq!(refused["id"], "call_4xpo9i6i", "{refused}");
    let started = events
        .iter()
        .find(|event| event["type"] == "sub_agent")
        .unwrap();
    assert_eq!(started["external_id"], SUB, "{started}");
    assert_eq!(started["call"], "call_sub1", "{started}");
    let ended = events
        .iter()
        .find(|event| event["type"] == "turn_ended")
        .unwrap();
    assert_eq!(ended["state"], "finished", "{ended}");

    let again = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": LEAD, "cursor": read["cursor"]}),
    )
    .await
    .unwrap();
    assert_eq!(again["events"], json!([]), "{again}");

    let sub = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": LEAD, "sub_agent": SUB}),
    )
    .await
    .unwrap();
    let said: Vec<&str> = sub["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|event| event["text"].as_str())
        .collect();
    assert_eq!(said, ["List the folder.", "Two files."], "{sub}");
    let located = call(
        &driver,
        method::SESSIONS_LOCATE,
        json!({"external_id": SUB}),
    )
    .await
    .unwrap();
    assert_eq!(located["session"]["external_id"], LEAD, "{located}");

    let read = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": FAILED}),
    )
    .await
    .unwrap();
    let ended = read["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "turn_ended")
        .unwrap();
    assert_eq!(ended["state"], "failed", "{ended}");
    assert_eq!(
        ended["failure"]["line"], "Unauthorized: User not found.",
        "{ended}"
    );
    assert_eq!(ended["failure"]["service"], true, "{ended}");

    for missing in [
        json!({"external_id": "title-".to_owned() + LEAD}),
        json!({"external_id": "gone"}),
    ] {
        let missing = call(&driver, method::SESSIONS_READ, missing).await;
        assert!(
            matches!(&missing, Err(CallError::Refused { word, .. }) if word == "not-found"),
            "{missing:?}"
        );
    }
    driver.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

/// The servers of every file Crush reads, a later file's entry over an earlier's, and the
/// person's toggles in the project's store over both; only a server's bare URL comes back.
/// The plan holds Steery's MCP entry and the skill in Crush's own skills folder.
#[tokio::test]
async fn crush_lists_its_servers_and_plans_its_setup() {
    let (home, work, _) = home("mcp");
    let config = home.join(".config/crush");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("crush.json"),
        json!({"mcp": {
            "tracker": {"type": "http", "url": "https://bot:SECRET@mcp.example.com/mcp?k=SECRET",
                        "headers": {"Authorization": "Bearer SECRET"}},
            "files": {"type": "stdio", "command": "npx", "args": ["--token", "SECRET"]},
            "later": {"type": "stdio", "command": "later", "disabled": true},
        }})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        work.join(".crush.json"),
        json!({"mcp": {"files": {"disabled": true}, "local": {"type": "sse", "url": "http://127.0.0.1:9/sse"}}})
            .to_string(),
    )
    .unwrap();
    rusqlite::Connection::open(work.join(".crush/crush.db"))
        .unwrap()
        .execute_batch(
            "insert into mcp_enabled_servers values ('later'); \
             insert into mcp_disabled_servers values ('tracker');",
        )
        .unwrap();
    let driver = start(&home).await;

    let everywhere = call(&driver, method::MCP_LIST, json!({})).await.unwrap();
    assert!(!everywhere.to_string().contains("SECRET"), "{everywhere}");
    assert_eq!(
        everywhere["servers"],
        json!([{"server": "files"}, {"server": "tracker", "url": "https://mcp.example.com/mcp"}]),
        "{everywhere}"
    );
    let here = call(&driver, method::MCP_LIST, json!({"cwd": work}))
        .await
        .unwrap();
    assert_eq!(
        here["servers"],
        json!([{"server": "later"}, {"server": "local", "url": "http://127.0.0.1:9/sse"}]),
        "{here}"
    );
    assert_eq!(here["complete"], true, "{here}");
    assert!(
        here["watch"]
            .as_array()
            .unwrap()
            .contains(&json!(work.join(".crush.json")))
    );

    std::fs::write(work.join("crushrc"), "echo '{}'\n").unwrap();
    let shell = call(&driver, method::MCP_LIST, json!({"cwd": work}))
        .await
        .unwrap();
    assert_eq!(shell["complete"], false, "{shell}");

    let planned = call(
        &driver,
        method::SETUP_PLAN,
        json!({"mcp_url": "http://127.0.0.1:7/mcp", "skill": "# Steery", "hook": "steery hook",
               "door": "http://127.0.0.1:7/door", "release": "7.0.0", "timeout": 60}),
    )
    .await
    .unwrap();
    assert_eq!(planned["found"]["release"], "0.100.0", "{planned}");
    let entries = planned["entries"].as_array().unwrap();
    assert_eq!(
        entries[0]["path"],
        json!(config.join("crush.json")),
        "{planned}"
    );
    assert_eq!(entries[0]["at"], "/mcp/steery", "{planned}");
    assert_eq!(
        entries[0]["value"],
        json!({"type": "http", "url": "http://127.0.0.1:7/mcp"})
    );
    assert_eq!(
        entries[1]["path"],
        json!(config.join("skills/steery/SKILL.md"))
    );
    assert!(planned.get("needs").is_none(), "{planned}");
    driver.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

/// A turn starts only behind Steery's `PreToolUse` hook, which `crush run`'s own approval
/// of every call leaves as its gate: without the hook the start is refused; with the hooks
/// the plan wrote, the turn mode runs `crush run -s`, names the session and ends with the
/// answer Crush wrote. A hook reads as a call of its group and answers as Crush parses it;
/// the guard blocks a call whose hook failed only in a turn Steery started.
#[tokio::test]
async fn crush_drives_a_turn_behind_its_hook() {
    let (home, work, _) = home("drive");
    let answered = home.join("answered.db");
    std::fs::copy(work.join(".crush/crush.db"), &answered).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    // The fixture's messages, moved to just after this run began.
    let store = rusqlite::Connection::open(&answered).unwrap();
    let first: i64 = store
        .query_row("select min(created_at) from messages", [], |row| row.get(0))
        .unwrap();
    let shift = now + 5 - first;
    store
        .execute_batch(&format!(
            "update messages set created_at = created_at + {shift}, \
             updated_at = updated_at + {shift}, finished_at = finished_at + {shift};"
        ))
        .unwrap();
    drop(store);
    let driver = start(&home).await;
    let continued = json!({"act": "continue", "external_id": LEAD, "cwd": work,
                           "folder": home.join("turn"), "worktree": false});

    let ungated = call(&driver, method::START, continued.clone()).await;
    assert!(
        matches!(&ungated, Err(CallError::Refused { word, .. }) if word == "refused"),
        "{ungated:?}"
    );

    let planned = call(
        &driver,
        method::SETUP_PLAN,
        json!({"mcp_url": "http://127.0.0.1:7/mcp", "skill": "# Steery", "hook": "steery hook",
               "door": "http://127.0.0.1:7/door", "release": "7.0.0", "timeout": 60}),
    )
    .await
    .unwrap();
    let hooks: Vec<&Value> = planned["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["carries"] == json!(["hooks"]))
        .collect();
    let at: Vec<&str> = hooks
        .iter()
        .map(|hook| hook["at"].as_str().unwrap())
        .collect();
    assert_eq!(
        at,
        [
            "/hooks/PreToolUse/-",
            "/hooks/PostToolUse/-",
            "/hooks/PostToolUseFailure/-"
        ]
    );
    assert_eq!(hooks[1]["value"]["command"], "steery hook PostToolUse");
    assert_eq!(hooks[0]["value"]["timeout"], 86_400);
    // As the runtime writes them, beside a hook of the person's own.
    let mut settings = json!({"hooks": {"PreToolUse": [{"command": "mine", "matcher": "bash"}]}});
    for hook in &hooks {
        let event = hook["at"].as_str().unwrap().split('/').nth(2).unwrap();
        settings["hooks"][event]
            .as_array_mut()
            .map(|list| list.push(hook["value"].clone()))
            .unwrap_or_else(|| settings["hooks"][event] = json!([hook["value"]]));
    }
    std::fs::create_dir_all(home.join(".config/crush")).unwrap();
    std::fs::write(home.join(".config/crush/crush.json"), settings.to_string()).unwrap();

    let launch = call(&driver, method::START, continued).await.unwrap();
    let argv: Vec<String> = serde_json::from_value(launch["argv"].clone()).unwrap();
    assert_eq!(argv[1..2], ["turn"]);
    assert_eq!(argv[4..], ["run", "-q", "-v", "-s", LEAD]);
    let mut turn = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(&work)
        .env_clear()
        .env("HOME", &home)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", home.join("bin").display()),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    turn.stdin
        .take()
        .unwrap()
        .write_all(b"Reply with PONG")
        .unwrap();
    let output = turn.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        std::fs::read_to_string(home.join("words")).unwrap(),
        "Reply with PONG"
    );
    let mut lines = Vec::new();
    for line in String::from_utf8(output.stdout).unwrap().lines() {
        lines.push(
            call(
                &driver,
                method::TURN_READ,
                json!({"turn": "t1", "line": line, "stream": "stdout"}),
            )
            .await
            .unwrap(),
        );
    }
    assert_eq!(lines[0]["named"], LEAD, "{lines:?}");
    assert_eq!(lines[1]["ended"]["state"], "finished", "{lines:?}");

    let before = json!({"hook_event_name": "PreToolUse", "event": "PreToolUse",
                        "session_id": LEAD, "cwd": work, "tool_name": "bash",
                        "tool_input": {"command": "rm -rf build"}, "tool_use_id": "call_1"});
    let moment = call(
        &driver,
        method::HOOK_READ,
        json!({"event": "PreToolUse", "document": before.to_string()}),
    )
    .await
    .unwrap();
    assert_eq!(moment["moment"]["external_id"], LEAD, "{moment}");
    assert_eq!(moment["moment"]["call"]["group"], "shell", "{moment}");
    assert_eq!(moment["moment"]["call"]["input"]["command"], "rm -rf build");
    let steery = json!({"hook_event_name": "PostToolUse", "session_id": LEAD, "cwd": work,
                        "tool_name": "mcp_steery_sessions_describe", "tool_input": {}});
    let moment = call(
        &driver,
        method::HOOK_READ,
        json!({"event": "PostToolUse", "document": steery.to_string()}),
    )
    .await
    .unwrap();
    assert_eq!(
        moment["moment"]["call"]["steery"], "sessions_describe",
        "{moment}"
    );
    assert_eq!(moment["moment"]["ok"], true, "{moment}");

    let denied = call(
        &driver,
        method::HOOK_ANSWER,
        json!({"event": "PreToolUse", "document": before.to_string(),
               "answer": {"decision": {"deny": {"reason": "Not in build"}}}}),
    )
    .await
    .unwrap();
    assert_eq!(denied["exit_code"], 0);
    assert_eq!(
        serde_json::from_str::<Value>(denied["stdout"].as_str().unwrap()).unwrap(),
        json!({"decision": "deny", "reason": "Not in build"})
    );
    let said = call(
        &driver,
        method::HOOK_ANSWER,
        json!({"event": "PostToolUse", "document": steery.to_string(),
               "answer": {"say": "Words wait for you"}}),
    )
    .await
    .unwrap();
    assert_eq!(
        said["stdout"],
        json!({"context": "Words wait for you"}).to_string()
    );
    let decided = call(
        &driver,
        method::HOOK_ANSWER,
        json!({"event": "PostToolUse", "document": steery.to_string(),
               "answer": {"decision": {"allow": {"reason": "ok"}}}}),
    )
    .await;
    assert!(decided.is_err(), "{decided:?}");
    driver.shutdown().await.unwrap();

    // The guard, with a hook that fails: `$PPID` is this test, as Crush is the hook's parent
    // in a turn, and `STEERY_STARTED` names the pid it trusts.
    let guarded =
        hooks[0]["value"]["command"]
            .as_str()
            .unwrap()
            .replacen("steery hook", "false", 1);
    let guard = |started: Option<String>| {
        let mut sh = std::process::Command::new("/bin/sh");
        sh.args(["-c", &guarded])
            .env_clear()
            .env("PATH", "/usr/bin:/bin");
        if let Some(started) = started {
            sh.env("STEERY_STARTED", started);
        }
        sh.output().unwrap()
    };
    let blocked = guard(Some(format!("{}:turn@x", std::process::id())));
    assert_eq!(blocked.status.code(), Some(2), "{blocked:?}");
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("the call is blocked"));
    assert_eq!(guard(None).status.code(), Some(1));
    assert_eq!(guard(Some("1:turn@x".into())).status.code(), Some(1));
    let _ = std::fs::remove_dir_all(&home);
}
