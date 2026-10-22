//! Each driver, run as the runtime runs it, over standard input and output: `initialize`,
//! then `mcp/list` against a home of its own holding the harness's `mcp.json` files, each
//! secret in them spelled `SECRET`.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::Client;
use steery_sdk::{method, wire};

/// `value` as the JSON file at `path`, its folders made.
fn write(path: &Path, value: Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, value.to_string()).unwrap();
}

/// The driver `binary`, in an environment holding only `HOME`, and started with protocols
/// 3 and 4 offered, of which it picks 4.
async fn start(binary: &str, home: &Path) -> Client {
    let client = Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            binary.into(),
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

/// The servers `mcp/list` answers with `params`, after checking it names no secret and
/// calls itself incomplete.
async fn servers(client: &Client, params: Value) -> Value {
    let raw = client
        .call(method::MCP_LIST, params, Duration::from_secs(30))
        .await
        .unwrap();
    assert!(!raw.to_string().contains("SECRET"), "{raw}");
    assert_eq!(raw["complete"], false, "{raw}");
    raw["servers"].clone()
}

/// omp: everywhere, the agent folder's servers, a URL one by its bare URL, one turned off
/// in its entry but on in `enabledServers`, and none `disabledServers` names. In a folder,
/// its `.omp/mcp.json` comes first and wins a name, and its root `mcp.json` comes last.
/// pi: a folder's `.pi/mcp.json` counts only where `trust.json` trusts that folder.
#[tokio::test]
async fn omp_and_pi_list_the_servers_a_conversation_loads_and_no_secret() {
    let home = std::env::temp_dir().join(format!("steery-pi-mcp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let work = home.join("work");
    let other = home.join("other");

    write(
        &home.join(".omp/agent/mcp.json"),
        json!({
            "mcpServers": {
                "tracker": {
                    "type": "http",
                    "url": "https://bot:SECRET@mcp.example.com/mcp?token=SECRET",
                    "headers": {"Authorization": "Bearer SECRET"},
                },
                "files": {"command": "npx", "args": ["SECRET"], "env": {"KEY": "SECRET"}},
                "quiet": {"type": "http", "url": "https://quiet.example.com/mcp", "enabled": false},
                "forced": {"command": "forced", "enabled": false},
            },
            "disabledServers": ["files"],
            "enabledServers": ["forced"],
        }),
    );
    write(
        &work.join(".omp/mcp.json"),
        json!({"mcpServers": {
            "tracker": {"type": "http", "url": "https://project.example.com/mcp"},
            "project": {"command": "project", "args": ["--key", "SECRET"]},
        }}),
    );
    write(
        &work.join(".mcp.json"),
        json!({"mcpServers": {"fallback": {"url": "https://fallback.example.com/mcp"}}}),
    );
    let omp = start(env!("CARGO_BIN_EXE_steery-driver-omp"), &home).await;
    assert_eq!(
        servers(&omp, json!({})).await,
        json!([
            {"server": "forced"},
            {"server": "tracker", "url": "https://mcp.example.com/mcp"},
        ])
    );
    assert_eq!(
        servers(&omp, json!({"cwd": work})).await,
        json!([
            {"server": "project"},
            {"server": "tracker", "url": "https://project.example.com/mcp"},
            {"server": "forced"},
            {"server": "fallback", "url": "https://fallback.example.com/mcp"},
        ])
    );
    omp.shutdown().await.unwrap();

    write(
        &home.join(".pi/agent/mcp.json"),
        json!({"mcpServers": {
            "tracker": {"url": "https://SECRET@mcp.example.com/mcp", "headers": {"X-Key": "SECRET"}},
            "files": {"command": "npx", "env": {"KEY": "SECRET"}},
        }}),
    );
    write(
        &home.join(".pi/agent/trust.json"),
        json!({work.to_str().unwrap(): true, other.to_str().unwrap(): false}),
    );
    let project =
        json!({"mcpServers": {"project": {"url": "https://user:SECRET@p.example.com/mcp"}}});
    write(&work.join(".pi/mcp.json"), project.clone());
    write(&other.join(".pi/mcp.json"), project);
    let pi = start(env!("CARGO_BIN_EXE_steery-driver-pi"), &home).await;
    let user = json!([
        {"server": "files"},
        {"server": "tracker", "url": "https://mcp.example.com/mcp"},
    ]);
    assert_eq!(servers(&pi, json!({})).await, user);
    assert_eq!(
        servers(&pi, json!({"cwd": work})).await,
        json!([
            {"server": "project", "url": "https://p.example.com/mcp"},
            {"server": "files"},
            {"server": "tracker", "url": "https://mcp.example.com/mcp"},
        ])
    );
    assert_eq!(servers(&pi, json!({"cwd": other})).await, user);
    pi.shutdown().await.unwrap();

    let _ = std::fs::remove_dir_all(&home);
}
