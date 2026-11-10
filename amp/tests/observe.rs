//! The driver, run as the runtime runs it, over standard input and output, with a stand-in
//! `amp` on `PATH` that answers from two made-up threads (`tests/fixtures`): one a person
//! ran, one `amp -x` ran with nobody at it. Its `amp mcp list` spells each secret `SECRET`.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::{CallError, Client};
use steery_sdk::{method, wire};

const PERSON: &str = "T-00000000-0000-4000-8000-000000000001";
const HEADLESS: &str = "T-00000000-0000-4000-8000-000000000002";

/// What Amp 0.0.1794225648 answers, signed out where `$HOME/signed-out` exists, and with
/// `AMP_API_KEY=sgamp_good` alone where it does.
const AMP: &str = r#"#!/bin/sh
fixtures=@FIXTURES@
if [ -e "$HOME/signed-out" ] && [ "$AMP_API_KEY" != sgamp_good ] && [ "$1" = threads ]; then
  echo "Error: Invalid or missing API key. Run 'amp login' to authenticate." >&2
  exit 1
fi
case "$*" in
  "--version") echo "0.0.1794225648-gtest (released 2026-11-09T12:00:48.000Z)" ;;
  "threads list --json --limit 1") echo '[]' ;;
  "threads list --json --include-archived --limit 500 --installation-id install-1")
    cat <<EOF
[{"id": "@PERSON@", "title": "Count the files", "updated": "2025-11-09T15:00:30.000Z",
  "tree": "file:///tmp/work", "messageCount": 4},
 {"id": "@HEADLESS@", "title": null, "updated": "2025-11-09T15:02:00.000Z",
  "tree": "file:///tmp/batch", "messageCount": 1}]
EOF
    ;;
  "threads export @PERSON@") cat "$fixtures/thread.json" ;;
  "threads export @HEADLESS@") cat "$fixtures/headless.json" ;;
  "threads export "*) echo "Error: Thread $3 does not exist" >&2; exit 1 ;;
  "mcp list --json")
    cat <<EOF
[{"name": "tracker", "type": "url", "source": "global",
  "spec": {"url": "https://bot:SECRET@mcp.example.com/mcp?k=SECRET", "headers": {"Authorization": "Bearer SECRET"}}},
 {"name": "files", "type": "command", "source": "workspace",
  "spec": {"command": "npx", "args": ["--token", "SECRET"], "env": {"KEY": "SECRET"}}},
 {"name": "where", "type": "url", "source": "workspace", "spec": {"url": "https://where.example.com$PWD"}}]
EOF
    ;;
  *) exit 2 ;;
esac
"#;

/// A home with Amp installed (the stand-in), its installation id, and `signed_out` or not.
fn home(name: &str, signed_out: bool) -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("steery-amp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let script = AMP
        .replace("@FIXTURES@", &fixtures.display().to_string())
        .replace("@PERSON@", PERSON)
        .replace("@HEADLESS@", HEADLESS);
    std::fs::write(bin.join("amp"), script).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(bin.join("amp"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let data = home.join(".local/share/amp");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("device-id.json"),
        r#"{"installationID": "install-1"}"#,
    )
    .unwrap();
    if signed_out {
        std::fs::write(home.join("signed-out"), "").unwrap();
    }
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
            env!("CARGO_BIN_EXE_steery-driver-amp").into(),
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

/// A thread a person ran is a session, where it ran; one `amp -x` ran is excluded. A read
/// folds the person's thread into its turn, its call and its tokens, whole; a thread Amp
/// does not have is `not-found`.
#[tokio::test]
async fn amp_lists_and_reads_its_threads() {
    let home = home("threads", false);
    let driver = start(&home).await;

    let listed = call(&driver, method::SESSIONS_LIST, json!({"transcripts": []}))
        .await
        .unwrap();
    assert_eq!(listed["excluded"], json!([HEADLESS]), "{listed}");
    assert_eq!(listed["complete"], true, "{listed}");
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{listed}");
    assert_eq!(sessions[0]["external_id"], PERSON);
    assert_eq!(sessions[0]["cwd"], "/tmp/work");
    assert_eq!(sessions[0]["title"], "Count the files");
    assert_eq!(sessions[0]["transcripts"], json!([]));

    let read = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": PERSON}),
    )
    .await
    .unwrap();
    assert_eq!(read["whole"], true, "{read}");
    assert_eq!(read["more"], false, "{read}");
    let kinds: Vec<&str> = read["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    for kind in [
        "model",
        "turn_began",
        "words",
        "call_began",
        "call_ended",
        "tokens",
        "turn_ended",
    ] {
        assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    let ended = read["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["type"] == "call_ended")
        .unwrap();
    assert_eq!(ended["ok"], true, "{ended}");

    let again = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": PERSON, "cursor": read["cursor"]}),
    )
    .await
    .unwrap();
    assert_eq!(again["events"], json!([]), "{again}");

    let missing = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": "T-00000000-0000-4000-8000-00000000dead"}),
    )
    .await;
    assert!(
        matches!(&missing, Err(CallError::Refused { word, .. }) if word == "not-found"),
        "{missing:?}"
    );

    let hook = call(&driver, "hook/read", json!({})).await;
    assert!(matches!(hook, Err(CallError::Unknown)), "{hook:?}");
    driver.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

/// Amp is asked in the folder asked about, and of each server only its name and its bare
/// URL come back.
#[tokio::test]
async fn amp_lists_the_servers_a_thread_loads_and_no_secret() {
    let home = home("mcp", false);
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let driver = start(&home).await;
    let listed = call(&driver, method::MCP_LIST, json!({"cwd": work}))
        .await
        .unwrap();
    assert!(!listed.to_string().contains("SECRET"), "{listed}");
    let work = work.canonicalize().unwrap();
    assert_eq!(
        listed["servers"],
        json!([
            {"server": "tracker", "url": "https://mcp.example.com/mcp"},
            {"server": "files"},
            {"server": "where", "url": format!("https://where.example.com{}", work.display())},
        ]),
        "{listed}"
    );
    assert_eq!(listed["complete"], false);
    assert!(
        listed["watch"]
            .as_array()
            .unwrap()
            .contains(&json!(home.join(".config/amp/settings.json")))
    );
    driver.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
}

/// Signed out, the plan holds Steery's MCP entry and skill and asks for a sign-in; an Amp
/// API key that Amp takes meets the need and comes back as `AMP_API_KEY`, and one it does
/// not is refused without the key in the line.
#[tokio::test]
async fn amp_signed_out_needs_a_sign_in_that_a_key_gives() {
    let home = home("sign-in", true);
    let driver = start(&home).await;
    let planned = call(
        &driver,
        method::SETUP_PLAN,
        json!({"mcp_url": "http://127.0.0.1:7/mcp", "skill": "# Steery", "hook": "steery hook",
               "door": "http://127.0.0.1:7/door", "release": "7.0.0", "timeout": 60}),
    )
    .await
    .unwrap();
    assert_eq!(planned["found"]["release"], "0.0.1794225648", "{planned}");
    let entries = planned["entries"].as_array().unwrap();
    assert_eq!(entries[0]["at"], "/amp.mcpServers/steery", "{planned}");
    assert_eq!(
        entries[0]["value"],
        json!({"url": "http://127.0.0.1:7/mcp"})
    );
    assert_eq!(
        entries[1]["path"],
        json!(home.join(".config/amp/skills/steery/SKILL.md"))
    );
    assert_eq!(planned["needs"][0]["id"], "sign-in", "{planned}");

    let refused = call(
        &driver,
        method::NEEDS_GIVE,
        json!({"need": "sign-in", "way": "api-key", "values": {"AMP_API_KEY": "sgamp_bad"}}),
    )
    .await;
    match &refused {
        Err(CallError::Refused { word, line }) => {
            assert_eq!(word, "refused");
            assert!(
                !line.as_deref().unwrap_or_default().contains("sgamp_bad"),
                "{line:?}"
            );
        }
        other => panic!("{other:?}"),
    }
    let given = call(
        &driver,
        method::NEEDS_GIVE,
        json!({"need": "sign-in", "way": "api-key", "values": {"AMP_API_KEY": "sgamp_good"}}),
    )
    .await
    .unwrap();
    assert_eq!(
        given["done"]["env"],
        json!({"AMP_API_KEY": "sgamp_good"}),
        "{given}"
    );
    driver.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
