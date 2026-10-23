//! The driver, run as the runtime runs it, over standard input and output: `initialize`,
//! then `mcp/list`, with a stand-in `codex` on `PATH` printing what `codex mcp list --json`
//! prints, each secret in it spelled `SECRET`.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::{CallError, Client};
use steery_sdk::{method, wire};

/// What Codex 0.163's `codex mcp list --json` prints, with the folder it ran in and its
/// `CODEX_HOME` in two servers' URLs.
const CODEX: &str = r#"#!/bin/sh
[ "$*" = "mcp list --json" ] || exit 2
cat <<EOF
[{"name": "tracker", "enabled": true, "disabled_reason": null,
  "transport": {"type": "streamable_http", "url": "https://bot:SECRET@mcp.example.com/mcp?k=SECRET",
    "bearer_token_env_var": "SECRET_TOKEN", "http_headers": {"Authorization": "Bearer SECRET"},
    "env_http_headers": null, "http_headers_helper": "<redacted>"},
  "startup_timeout_sec": null, "tool_timeout_sec": null, "auth_status": "unsupported"},
 {"name": "files", "enabled": true, "disabled_reason": null,
  "transport": {"type": "stdio", "command": "npx", "args": ["--token", "SECRET"],
    "env": {"KEY": "SECRET"}, "env_vars": ["SECRET_VAR"], "cwd": null},
  "startup_timeout_sec": null, "tool_timeout_sec": null, "auth_status": "unsupported"},
 {"name": "off", "enabled": false, "disabled_reason": "disabled in config",
  "transport": {"type": "stdio", "command": "off", "args": [], "env": null, "env_vars": [], "cwd": null},
  "startup_timeout_sec": null, "tool_timeout_sec": null, "auth_status": "unsupported"},
 {"name": "where", "enabled": true,
  "transport": {"type": "streamable_http", "url": "https://where.example.com$PWD"}},
 {"name": "home", "enabled": true,
  "transport": {"type": "streamable_http", "url": "https://home.example.com$CODEX_HOME"}}]
EOF
"#;

/// The driver, in an environment holding `HOME`, `CODEX_HOME` and `PATH`.
async fn start(home: &Path, path: &str) -> Client {
    let client = Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            format!("CODEX_HOME={}", home.join(".codex").display()),
            format!("PATH={path}"),
            env!("CARGO_BIN_EXE_steery-driver-codex").into(),
        ],
        None,
    )
    .await
    .unwrap();
    let offered = wire::InitializeParams {
        protocol: vec![3, 4],
        runtime: wire::Runtime {
            release: "7.0.0".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        },
        home: home.to_owned(),
        config: home.join(".config/steery"),
        settings: Default::default(),
    };
    assert_eq!(client.initialize(&offered).await.unwrap().protocol, 4);
    client
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

/// Codex is asked in the folder asked about, under its own home, and of each server only
/// its name and its bare URL come back; one it lists turned off does not. With no `codex`
/// here, `mcp/list` is refused `no-binary`.
#[tokio::test]
async fn codex_lists_the_servers_a_conversation_loads_and_no_secret() {
    let home = std::env::temp_dir().join(format!("steery-codex-mcp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let bin = home.join("bin");
    let work = home.join("work");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let written = bin.join("codex.new");
        std::fs::write(&written, CODEX).unwrap();
        std::fs::set_permissions(&written, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&written, bin.join("codex")).unwrap();
    }
    // The folder as the stand-in's shell names it, symbolic links resolved.
    let work = work.canonicalize().unwrap();
    let codex_home = home.join(".codex");

    let driver = start(&home, &format!("{}:/usr/bin:/bin", bin.display())).await;
    let servers = |folder: &str| {
        json!([
            {"server": "tracker", "url": "https://mcp.example.com/mcp"},
            {"server": "files"},
            {"server": "where", "url": format!("https://where.example.com{folder}")},
            {"server": "home", "url": format!("https://home.example.com{}", codex_home.display())},
        ])
    };
    let everywhere = list(&driver, json!({})).await;
    assert_eq!(everywhere["servers"], servers("/"));
    assert_eq!(everywhere["complete"], false);
    let in_work = list(&driver, json!({"cwd": work})).await;
    assert_eq!(in_work["servers"], servers(work.to_str().unwrap()));
    assert!(
        in_work["watch"]
            .as_array()
            .unwrap()
            .contains(&json!(work.join(".codex/config.toml")))
    );
    driver.shutdown().await.unwrap();

    let missing = start(&home, "/usr/bin:/bin").await;
    let refused = missing
        .ask::<method::McpList>(&wire::harness::McpList { cwd: None })
        .await;
    assert!(
        matches!(&refused, Err(CallError::Refused { word, .. }) if word == "no-binary"),
        "{refused:?}"
    );
    missing.shutdown().await.unwrap();

    let _ = std::fs::remove_dir_all(&home);
}
