//! The driver, run as the runtime runs it, over standard input and output: `initialize`,
//! then `mcp/list` against a home of its own holding Claude Code's configuration, each
//! secret in it spelled `SECRET`.

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

/// What `mcp/list` answers with `params`, which names no secret.
async fn list(client: &Client, params: Value) -> Value {
    let raw = client
        .call(method::MCP_LIST, params, Duration::from_secs(30))
        .await
        .unwrap();
    assert!(!raw.to_string().contains("SECRET"), "{raw}");
    raw
}

/// Everywhere: the user-scope servers, an HTTP one by its bare URL and one over standard
/// input and output by its name alone, an entry Claude Code skips left out. In a trusted
/// folder, its local-scope server and the one `.mcp.json` server approved come first, and
/// the server the folder turns off is gone; in a folder never trusted, its own approval of
/// its `.mcp.json` counts for nothing. A runtime offering 3 alone is refused.
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
    assert_eq!(
        driver
            .initialize(&offer(vec![3, 4], &home))
            .await
            .unwrap()
            .protocol,
        4
    );
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
