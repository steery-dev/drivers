//! The driver, run as the runtime runs it, over standard input and output, against a
//! transcript the canvas's contract describes (`fixtures/sessions`): a finished turn with
//! both voices' lines and tokens, then one that failed on a usage limit. A stand-in `canvas`
//! on `PATH` answers `--version`, `models` (not signed in), and a turn as the contract says:
//! it appends a turn that draws one line and replies, and prints its JSON lines, each
//! naming its record.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::{CallError, Client};
use steery_sdk::{method, wire};

const FIXTURE: &str = "019ac4a0-1000-7000-8000-000000000001";

const STAND_IN: &str = r#"#!/bin/sh
case "$1" in
--version) echo 'canvas 0.3.1'; exit 0 ;;
models)
  printf '%s\n' '{"source":"none","signed_in":false,"line":"No Claude sign-in: Claude Code is not signed in.","models":[{"id":"claude-haiku-5-5","role":"sketcher"},{"id":"claude-sonnet-5-5","role":"plotter"},{"id":"claude-opus-5-5","role":"navigator"}]}'
  exit 0 ;;
turn) shift ;;
*) exit 2 ;;
esac
session=""; cwd=$PWD
while [ $# -gt 0 ]; do
  case "$1" in --session) session=$2; shift ;; --cwd) cwd=$2; shift ;; esac
  shift
done
new=""
[ -n "$session" ] || { session="019ac4a0-2000-7000-8000-$(printf %012d $$)"; new=1; }
file="$CANVAS_HOME/sessions/$session.jsonl"
words=$(cat | awk '{gsub(/\\/,"\\\\");gsub(/"/,"\\\"");printf "%s%s",(NR>1?"\\n":""),$0}')
at=2026-11-27T11:00:00.000Z
n=0
record() { n=$((n+1)); printf '{"id":"r-%s-%s","at":"%s",%s}\n' "$$" "$n" "$at" "$1" >> "$file"; }
printf '{"named":"%s"}\n' "$session"
[ -n "$new" ] && record "\"kind\":\"session\",\"cwd\":\"$cwd\",\"title\":\"$words\""
echo $$ > "$CANVAS_HOME/sessions/$session.pid"
turn="t-$$"
record "\"kind\":\"turn_began\",\"turn\":\"$turn\""
record "\"kind\":\"words\",\"text\":\"$words\""
record "\"kind\":\"line\",\"message\":\"$turn/plotter\",\"voice\":\"plotter\",\"text\":\"reply \\\"PONG\\\"\""
printf '{"line":{"id":"r-%s-%s","message":"%s/plotter","voice":"plotter","text":"reply \\"PONG\\""}}\n' "$$" "$n" "$turn"
record '"kind":"tokens","voice":"plotter","model":"claude-sonnet-5-5","in":10,"cached":4,"out":2'
printf '{"tokens":{"id":"r-%s-%s","voice":"plotter","model":"claude-sonnet-5-5","in":10,"cached":4,"out":2}}\n' "$$" "$n"
record '"kind":"turn_ended","state":"finished"'
rm -f "$CANVAS_HOME/sessions/$session.pid"
printf '{"ended":{"id":"r-%s-%s","state":"finished"}}\n' "$$" "$n"
"#;

/// A home with the canvas installed (the stand-in), and its folder holding the fixture.
fn home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("steery-canvas-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("canvas"), STAND_IN).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(bin.join("canvas"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let sessions = canvas_home(&home).join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::copy(fixture(), sessions.join(format!("{FIXTURE}.jsonl"))).unwrap();
    home
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/sessions")
        .join(format!("{FIXTURE}.jsonl"))
}

fn canvas_home(home: &Path) -> PathBuf {
    home.join("canvas")
}

fn environment(home: &Path) -> Vec<(String, String)> {
    vec![
        ("HOME".into(), home.display().to_string()),
        (
            "PATH".into(),
            format!("{}:/usr/bin:/bin", home.join("bin").display()),
        ),
        ("CANVAS_HOME".into(), canvas_home(home).display().to_string()),
    ]
}

/// The driver, in an environment holding only `HOME`, `PATH` and `CANVAS_HOME`, initialized.
async fn start(home: &Path) -> Client {
    let mut argv = vec!["-i".to_owned()];
    argv.extend(environment(home).into_iter().map(|(k, v)| format!("{k}={v}")));
    argv.push(env!("CARGO_BIN_EXE_steery-driver-canvas").into());
    let client = Client::spawn(Path::new("/usr/bin/env"), &argv, None)
        .await
        .unwrap();
    let offered = wire::InitializeParams {
        protocol: vec![14, 15, 19],
        runtime: wire::Runtime {
            release: "7.0.0".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        },
        home: home.to_owned(),
        config: home.join(".config/steery"),
        settings: Default::default(),
    };
    let initialized = client.initialize(&offered).await.unwrap();
    assert_eq!(initialized.protocol, 19);
    let harness = initialized.harness.unwrap();
    assert_eq!(harness.levels.last(), Some(&wire::Level::Live));
    assert_eq!(harness.facts.door, Some(wire::harness::LiveDoor::Plugin));
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

fn of_type<'a>(read: &'a Value, kind: &str) -> Vec<&'a Value> {
    read["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == kind)
        .collect()
}

/// The fixture is listed with its session record's title and folder, and reads as two
/// turns: the person's words, each stroke line as the agent's words of its model call, the
/// plotter's model, both calls' tokens, and the failed turn's kind and line. The door's
/// records fold to the same events, one sent as its id alone read from the transcript, and a
/// read goes on from the fold's cursor. A cursor read gives only what was appended. The
/// `.pid` file says who holds the session, and `idle` how its last turn ended.
#[tokio::test]
async fn canvas_lists_reads_and_holds_its_sessions() {
    let home = home("sessions");
    let driver = start(&home).await;

    let listed = call(&driver, method::SESSIONS_LIST, json!({"transcripts": []}))
        .await
        .unwrap();
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{listed}");
    assert_eq!(sessions[0]["external_id"], FIXTURE);
    assert_eq!(sessions[0]["title"], "Sketch the login flow");
    assert_eq!(sessions[0]["cwd"], "/tmp/canvas-project");

    let read = call(&driver, method::SESSIONS_READ, json!({"external_id": FIXTURE}))
        .await
        .unwrap();
    assert_eq!(read["whole"], true);
    assert_eq!(
        kinds(&read),
        [
            "turn_began", "words", "words", "tokens", "words", "words", "model", "tokens",
            "turn_ended", "turn_began", "words", "turn_ended"
        ],
        "{read}"
    );
    let words: Vec<(&str, &str)> = of_type(&read, "words")
        .iter()
        .map(|w| (w["voice"].as_str().unwrap(), w["text"].as_str().unwrap()))
        .collect();
    assert_eq!(words[0], ("person", "Sketch the login flow"));
    assert_eq!(words[3], ("agent", "reply \"Drew the login flow.\""));
    assert_eq!(
        of_type(&read, "words")[3]["message"],
        "019ac4a0-1000-7000-8000-0000000000t1/plotter"
    );
    assert_eq!(of_type(&read, "model")[0]["model"], "claude-sonnet-5-5");
    let tokens = of_type(&read, "tokens");
    assert_eq!((&tokens[1]["input"], &tokens[1]["cached"]), (&json!(1800), &json!(1200)));
    assert_eq!(tokens[1]["response"], "019ac4a0-1000-7000-8000-000000000a08");
    let ended = of_type(&read, "turn_ended");
    assert_eq!(ended[0]["state"], "finished");
    assert_eq!(ended[0]["turn"], "019ac4a0-1000-7000-8000-0000000000t1");
    assert_eq!(ended[1]["failure"]["kind"], "rate_limit");
    assert_eq!(ended[1]["failure"]["service"], true);
    assert_eq!(ended[1]["failure"]["line"], "Claude's usage limit is reached until 15:00");
    assert_eq!(read["after"], "019ac4a0-1000-7000-8000-000000000a0c");

    let records: Vec<Value> = std::fs::read_to_string(fixture())
        .unwrap()
        .lines()
        .map(|line| {
            let value: Value = serde_json::from_str(line).unwrap();
            json!({"id": value["id"], "value": value})
        })
        .collect();
    let first = call(
        &driver,
        method::SESSIONS_FOLD,
        json!({"external_id": FIXTURE, "records": records[..5], "quiet": 0}),
    )
    .await
    .unwrap();
    assert_eq!(first["consumed"], 5);
    let mut rest = records[5..].to_vec();
    rest[0] = json!({"id": rest[0]["id"]});
    let second = call(
        &driver,
        method::SESSIONS_FOLD,
        json!({"external_id": FIXTURE, "cursor": first["cursor"], "records": rest, "quiet": 0}),
    )
    .await
    .unwrap();
    assert_eq!(second["consumed"], 7);
    let mut folded = first["events"].as_array().unwrap().clone();
    folded.extend(second["events"].as_array().unwrap().iter().cloned());
    assert_eq!(json!(folded), read["events"], "a fold gives what a read gives");
    let after_fold = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": FIXTURE, "cursor": second["cursor"]}),
    )
    .await
    .unwrap();
    assert_eq!(after_fold["events"], json!([]), "{after_fold}");

    let again = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": FIXTURE, "cursor": read["cursor"]}),
    )
    .await
    .unwrap();
    assert_eq!(again["events"], json!([]));
    let path = canvas_home(&home).join(format!("sessions/{FIXTURE}.jsonl"));
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{\"id\":\"x1\",\"at\":\"2026-11-27T10:06:00Z\",\"kind\":\"turn_began\",\"turn\":\"t3\"}\n{\"id\":\"x2\",");
    std::fs::write(&path, &text).unwrap();
    let read_on = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": FIXTURE, "cursor": second["cursor"]}),
    )
    .await
    .unwrap();
    assert_eq!(kinds(&read_on), ["turn_began"], "a read goes on from a fold");
    let appended = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": FIXTURE, "cursor": read["cursor"]}),
    )
    .await
    .unwrap();
    assert_eq!(appended["reset"], false);
    assert_eq!(kinds(&appended), ["turn_began"], "a half-written record waits");

    let held = call(&driver, method::HELD, json!({"external_id": FIXTURE}))
        .await
        .unwrap();
    assert!(held["nobody"]["proof"].as_str().unwrap().contains("absent"), "{held}");
    let idle = call(&driver, method::IDLE, json!({"external_id": FIXTURE}))
        .await
        .unwrap();
    assert_eq!(idle["sample"]["state"], "interrupted", "{idle}");

    let pid = canvas_home(&home).join(format!("sessions/{FIXTURE}.pid"));
    std::fs::write(&pid, std::process::id().to_string()).unwrap();
    let held = call(&driver, method::HELD, json!({"external_id": FIXTURE}))
        .await
        .unwrap();
    assert_eq!(held["person"]["stop"]["process"]["pid"], std::process::id());
    assert_eq!(held["person"]["stop"]["process"]["certain"], true);
    let idle = call(&driver, method::IDLE, json!({"external_id": FIXTURE}))
        .await
        .unwrap();
    assert_eq!(idle["sample"]["state"], "running", "{idle}");

    let gone = call(&driver, method::SESSIONS_READ, json!({"external_id": "nope"})).await;
    assert!(
        matches!(&gone, Err(CallError::Refused { word, .. }) if word == "not-found"),
        "{gone:?}"
    );
}

/// `models` and `setup/plan` say what `canvas models` says: its models, the plotter's first,
/// and the sign-in need with its line; the plan writes the door to `steery.json`. A new turn
/// runs `canvas turn` on the words through the turn mode; its lines name the session, give
/// the stroke line and the tokens as the events the transcript gives under the same ids,
/// and end it `finished`. A continue names the session, the receipt finds the delivery
/// marker only after the mark, and a model pick is refused: the canvas picks its own.
#[tokio::test]
async fn canvas_drives_a_turn_and_continues_it() {
    let home = home("drive");
    let driver = start(&home).await;
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    let models = call(&driver, method::MODELS, json!({})).await.unwrap();
    assert_eq!(models["models"][0]["model"], "claude-sonnet-5-5");
    assert_eq!(models["signed_in"], false);
    let plan = call(
        &driver,
        method::SETUP_PLAN,
        json!({"mcp_url": "http://127.0.0.1:7777/mcp", "skill": "", "hook": "steery hook canvas",
               "door": "ws://127.0.0.1:7777/door", "release": "7.0.0", "timeout": 120}),
    )
    .await
    .unwrap();
    assert_eq!(plan["found"]["release"], "0.3.1");
    assert_eq!(
        plan["entries"],
        json!([{"path": canvas_home(&home).join("steery.json"), "format": "json", "at": "/door",
                "value": "ws://127.0.0.1:7777/door", "carries": ["plugin"]}])
    );
    let need = &plan["needs"][0];
    assert_eq!(need["line"], "No Claude sign-in: Claude Code is not signed in.");
    let ways: Vec<&str> = need["ways"]
        .as_array()
        .unwrap()
        .iter()
        .map(|way| way["id"].as_str().unwrap())
        .collect();
    assert_eq!(ways, ["api-key", "gateway"], "no Claude Code here, so no plan sign-in");

    let new = json!({"act": "new", "cwd": cwd, "folder": home.join("turn"), "worktree": false});
    let mut picked = new.clone();
    picked["model"] = json!("claude-opus-5-5");
    let refused = call(&driver, method::START, picked).await;
    assert!(
        matches!(&refused, Err(CallError::Refused { word, .. }) if word == "selection-unsupported"),
        "{refused:?}"
    );
    let launch = call(&driver, method::START, new).await.unwrap();
    let argv: Vec<String> = serde_json::from_value(launch["argv"].clone()).unwrap();
    assert_eq!(argv[1..], ["turn".to_owned(), home.join("bin/canvas").display().to_string(),
        "turn".into(), "--cwd".into(), cwd.display().to_string()]);
    let lines = turn(&driver, &home, &launch, "Reply with PONG").await;
    let id = lines[0]["named"].as_str().unwrap().to_owned();
    assert_eq!(lines.last().unwrap()["ended"], json!({"state": "finished"}), "{lines:?}");

    let located = call(&driver, method::SESSIONS_LOCATE, json!({"external_id": id}))
        .await
        .unwrap();
    assert_eq!(located["session"]["title"], "Reply with PONG");
    let read = call(&driver, method::SESSIONS_READ, json!({"external_id": id}))
        .await
        .unwrap();
    let said: Vec<&Value> = of_type(&read, "words").into_iter().map(|w| &w["text"]).collect();
    assert_eq!(said, ["Reply with PONG", "reply \"PONG\""], "{read}");
    // The turn's events, but their stamps, are the transcript's under the same ids.
    let unstamped = |event: &Value| {
        let mut event = event.clone();
        event.as_object_mut().unwrap().remove("at");
        event
    };
    let given: Vec<Value> = lines
        .iter()
        .flat_map(|line| line["events"].as_array().cloned().unwrap_or_default())
        .map(|event| unstamped(&event))
        .collect();
    let stroke = unstamped(of_type(&read, "words")[1]);
    let spent = unstamped(of_type(&read, "tokens")[0]);
    assert_eq!(given, [stroke, spent], "{lines:?}");

    let mark = call(&driver, method::RECEIPT_MARK, json!({"external_id": id}))
        .await
        .unwrap();
    let find = json!({"external_id": id, "mark": mark["mark"], "delivery": "d42"});
    let before = call(&driver, method::RECEIPT_FIND, find.clone()).await.unwrap();
    assert_eq!(before["found"], false);
    let launch = call(
        &driver,
        method::START,
        json!({"act": "continue", "external_id": id, "cwd": cwd, "folder": home.join("turn"),
               "worktree": false}),
    )
    .await
    .unwrap();
    let argv: Vec<String> = serde_json::from_value(launch["argv"].clone()).unwrap();
    assert!(argv.windows(2).any(|pair| pair == ["--session", id.as_str()]), "{argv:?}");
    let lines = turn(&driver, &home, &launch, "Again.\n\nsteery-delivery:d42").await;
    assert_eq!(lines[0]["named"], id.as_str());
    let after = call(&driver, method::RECEIPT_FIND, find).await.unwrap();
    assert_eq!(after["found"], true);
    let held = call(&driver, method::HELD, json!({"external_id": id})).await.unwrap();
    assert!(held["nobody"].is_object(), "the turn removed its .pid: {held}");
}

/// Run a launch as the runtime does, its words on standard input, and read its lines.
async fn turn(driver: &Client, home: &Path, launch: &Value, words: &str) -> Vec<Value> {
    let argv: Vec<String> = serde_json::from_value(launch["argv"].clone()).unwrap();
    let mut turn = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(launch["cwd"].as_str().unwrap())
        .env_clear()
        .envs(environment(home))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    turn.stdin.take().unwrap().write_all(words.as_bytes()).unwrap();
    let output = turn.wait_with_output().unwrap();
    assert!(output.status.success());
    let mut lines = Vec::new();
    for line in String::from_utf8(output.stdout).unwrap().lines() {
        lines.push(
            call(
                driver,
                method::TURN_READ,
                json!({"turn": "t1", "line": line, "stream": "stdout"}),
            )
            .await
            .unwrap(),
        );
    }
    lines
}
