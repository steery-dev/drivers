//! The driver, run as the runtime runs it, over standard input and output: `initialize`,
//! then `mcp/list` against a home of its own holding Claude Code's configuration, and
//! `hook/read` and `hook/answer` for Claude Code's hooks and Cursor's runs of them, each
//! secret spelled `SECRET`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::Client;
use steery_sdk::{method, wire};

/// `value` as the JSON file at `path`, its folders made.
fn write(path: &Path, value: Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, value.to_string()).unwrap();
}

/// The driver, in an environment holding only `HOME`.
async fn start(home: &Path) -> Client {
    Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            env!("CARGO_BIN_EXE_steery-driver-claude-code").into(),
        ],
        None,
    )
    .await
    .unwrap()
}

fn offer(protocol: Vec<u32>, home: &Path) -> wire::InitializeParams {
    wire::InitializeParams {
        protocol,
        runtime: wire::Runtime {
            release: "7.0.0".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        },
        home: home.to_owned(),
        config: home.join(".config/steery"),
        settings: Default::default(),
    }
}

/// What `method` answers with `params`, which names no secret.
async fn ask(client: &Client, method: &str, params: Value) -> Value {
    let raw = client
        .call(method, params, Duration::from_secs(30))
        .await
        .unwrap();
    assert!(!raw.to_string().contains("SECRET"), "{raw}");
    raw
}

async fn list(client: &Client, params: Value) -> Value {
    ask(client, method::MCP_LIST, params).await
}

/// Everywhere: the user-scope servers, an HTTP one by its bare URL and one over standard
/// input and output by its name alone, an entry Claude Code skips left out. In a trusted
/// folder, its local-scope server and the one `.mcp.json` server approved come first, and
/// the server the folder turns off is gone; in a folder never trusted, its own approval of
/// its `.mcp.json` counts for nothing. A runtime offering 3 alone is refused. Cursor runs
/// the hooks of Claude Code's settings under its own chat id, its sign-in token in the
/// hook's environment: none of its documents is a moment, and none is answered.
#[tokio::test]
async fn claude_code_lists_the_servers_a_conversation_loads_and_no_secret() {
    let home = std::env::temp_dir().join(format!("steery-claude-mcp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let trusted: PathBuf = home.join("work/trusted");
    let untrusted: PathBuf = home.join("work/untrusted");
    write(
        &home.join(".claude.json"),
        json!({
            "mcpServers": {
                "tracker": {
                    "type": "http",
                    "url": "https://bot:SECRET@mcp.example.com/v1/mcp?token=SECRET#SECRET",
                    "headers": {"Authorization": "Bearer SECRET"},
                },
                "files": {"command": "npx", "args": ["--token", "SECRET"], "env": {"KEY": "SECRET"}},
                "untyped": {"url": "https://untyped.example.com/mcp"},
            },
            "projects": {
                trusted.to_str().unwrap(): {
                    "hasTrustDialogAccepted": true,
                    "mcpServers": {"local-db": {"type": "stdio", "command": "db", "env": {"PW": "SECRET"}}},
                    "disabledMcpServers": ["files"],
                },
            },
        }),
    );
    write(
        &home.join(".claude/settings.json"),
        json!({"disabledMcpjsonServers": ["rejected"]}),
    );
    let project = json!({"mcpServers": {
        "shared": {"type": "http", "url": "https://team.example.com/mcp", "headers": {"X-Key": "SECRET"}},
        "pending": {"type": "sse", "url": "https://pending.example.com/sse"},
        "rejected": {"type": "http", "url": "https://rejected.example.com/mcp"},
    }});
    write(&trusted.join(".mcp.json"), project.clone());
    write(
        &trusted.join(".claude/settings.local.json"),
        json!({"enabledMcpjsonServers": ["shared", "rejected"]}),
    );
    write(&untrusted.join(".mcp.json"), project);
    write(
        &untrusted.join(".claude/settings.local.json"),
        json!({"enableAllProjectMcpServers": true}),
    );

    let driver = start(&home).await;
    let initialized = driver.initialize(&offer(vec![3, 4], &home)).await.unwrap();
    assert_eq!(initialized.protocol, 4);
    let everywhere = list(&driver, json!({})).await;
    let user = json!([
        {"server": "files"},
        {"server": "tracker", "url": "https://mcp.example.com/v1/mcp"},
    ]);
    assert_eq!(everywhere["servers"], user);
    assert_eq!(everywhere["complete"], true);
    assert!(
        everywhere["watch"]
            .as_array()
            .unwrap()
            .contains(&json!(home.join(".claude.json")))
    );
    let in_trusted = list(&driver, json!({"cwd": trusted})).await;
    assert_eq!(
        in_trusted["servers"],
        json!([
            {"server": "local-db"},
            {"server": "shared", "url": "https://team.example.com/mcp"},
            {"server": "tracker", "url": "https://mcp.example.com/v1/mcp"},
        ])
    );
    let in_untrusted = list(&driver, json!({"cwd": untrusted})).await;
    assert_eq!(in_untrusted["servers"], user);

    // The runtime hands a hook only the variables its entry names: Cursor's version, never
    // its sign-in token.
    for hook in &initialized.harness.unwrap().hooks {
        let cursors: Vec<&str> = hook
            .env
            .iter()
            .map(String::as_str)
            .filter(|name| name.starts_with("CURSOR_"))
            .collect();
        assert_eq!(cursors, ["CURSOR_VERSION"], "{}", hook.event);
    }
    // Documents in the shapes Claude Code and cursor-agent 2026.07.01 hand a hook.
    let claude_start = json!({
        "session_id": "c1",
        "transcript_path": home.join(".claude/projects/-work/c1.jsonl"),
        "cwd": trusted,
        "hook_event_name": "SessionStart",
        "source": "startup",
    });
    let claude_call = json!({
        "session_id": "c1",
        "cwd": trusted,
        "hook_event_name": "PreToolUse",
        "tool_name": "Read",
        "tool_input": {"file_path": "/work/a.rs"},
        "tool_use_id": "toolu_1",
    });
    let cursor_start = json!({
        "conversation_id": "k1",
        "generation_id": "k1",
        "model": "default",
        "is_background_agent": false,
        "session_id": "k1",
        "hook_event_name": "sessionStart",
        "cursor_version": "2026.07.01-41b2de7",
        "workspace_roots": [trusted],
        "user_email": "person@example.com",
        "transcript_path": null,
    });
    let cursor_call = json!({
        "conversation_id": "k1",
        "generation_id": "k1",
        "model": "default",
        "tool_name": "Read",
        "tool_input": {"file_path": "/work/a.rs"},
        "tool_use_id": "call-1",
        "session_id": "k1",
        "hook_event_name": "preToolUse",
        "cursor_version": "2026.07.01-41b2de7",
        "workspace_roots": [trusted],
        "user_email": "person@example.com",
        "transcript_path": null,
    });
    // A Cursor document without its version is still told apart by the hook's environment.
    let mut bare_call = cursor_call.clone();
    bare_call.as_object_mut().unwrap().remove("cursor_version");
    let cursor_env =
        json!({"CURSOR_VERSION": "2026.07.01-41b2de7", "CURSOR_ACCESS_TOKEN": "SECRET"});
    // Grok Build 1.0.77 hands a hook Claude Code's own field names, so only its environment
    // tells it apart.
    let grok_call = json!({
        "session_id": "01a1c56b-5ea1-7c72-89f2-ebe06b634618",
        "sessionId": "01a1c56b-5ea1-7c72-89f2-ebe06b634618",
        "cwd": trusted,
        "workspaceRoot": trusted,
        "hook_event_name": "PreToolUse",
        "hookEventName": "pre_tool_use",
        "permission_mode": "bypassPermissions",
        "tool_name": "Bash",
        "tool_input": {"command": "ls"},
        "tool_use_id": "call-19634a0c-cbd5-4ed9-bfe1-da2f776c3269-0",
        "transcript_path": home.join(".grok/sessions/%2Fwork/01a1c56b-5ea1-7c72-89f2-ebe06b634618/updates.jsonl"),
    });
    let grok_env = json!({"GROK_HOOK_EVENT": "pre_tool_use"});

    let read = |event: &str, document: &Value, env: &Value| json!({"event": event, "document": document.to_string(), "env": env});
    let started = ask(
        &driver,
        method::HOOK_READ,
        read("SessionStart", &claude_start, &json!({})),
    )
    .await;
    assert_eq!(started["moment"]["external_id"], "c1");
    for (event, document, env) in [
        ("SessionStart", &cursor_start, json!({})),
        ("PreToolUse", &cursor_call, json!({})),
        ("PreToolUse", &bare_call, cursor_env.clone()),
        ("PreToolUse", &grok_call, grok_env.clone()),
    ] {
        let read = ask(&driver, method::HOOK_READ, read(event, document, &env)).await;
        assert_eq!(read, json!({}), "{event}");
    }

    let answer = |event: &str, document: &Value, env: &Value, answer: Value| json!({"event": event, "document": document.to_string(), "env": env, "answer": answer});
    let deny = || json!({"decision": {"deny": {"reason": "not now"}}});
    let denied = ask(
        &driver,
        method::HOOK_ANSWER,
        answer("PreToolUse", &claude_call, &json!({}), deny()),
    )
    .await;
    assert!(
        denied["stdout"].as_str().unwrap().contains("deny"),
        "{denied}"
    );
    let env_file = home.join("cursor.env");
    let silent = json!({"stdout": "", "exit_code": 0});
    for (event, document, env, said) in [
        ("PreToolUse", &cursor_call, json!({}), deny()),
        ("PreToolUse", &bare_call, cursor_env.clone(), deny()),
        ("PreToolUse", &grok_call, grok_env.clone(), deny()),
        (
            "SessionStart",
            &cursor_start,
            json!({"CLAUDE_ENV_FILE": env_file}),
            json!({"say": "Report your lines", "lead": "claude-code:k1"}),
        ),
    ] {
        let answered = ask(
            &driver,
            method::HOOK_ANSWER,
            answer(event, document, &env, said),
        )
        .await;
        assert_eq!(answered, silent, "{event}");
    }
    assert!(!env_file.exists());
    driver.shutdown().await.unwrap();

    write(
        &home.join(".claude/settings.json"),
        json!({"enabledPlugins": {"tools@market": true}}),
    );
    let old = start(&home).await;
    assert!(old.initialize(&offer(vec![3], &home)).await.is_err());
    old.shutdown().await.unwrap();
    let plugged = start(&home).await;
    plugged.initialize(&offer(vec![3, 4], &home)).await.unwrap();
    assert_eq!(list(&plugged, json!({})).await["complete"], false);
    plugged.shutdown().await.unwrap();

    let _ = std::fs::remove_dir_all(&home);
}
