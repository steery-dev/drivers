//! The driver, run as the runtime runs it, over standard input and output, against two chat
//! history files that Aider 0.86.2 wrote with an Ollama model (`tests/fixtures`): `mixed.md`,
//! a run with no API key that failed, then a person's run that restored it and was answered,
//! and `driven.md`, three runs that took their words from `--message-file`. A stand-in
//! `aider` on `PATH` answers `--version`, and a turn as Aider does: it appends a run that
//! answers `PONG` to the file `--chat-history-file` names.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::{CallError, Client};
use steery_sdk::{method, wire};

const STAND_IN: &str = r#"#!/bin/sh
[ "$1" = --version ] && { echo 'aider 0.86.2'; exit 0; }
file=""; prev=""
for arg in "$@"; do [ "$prev" = --chat-history-file ] && file=$arg; prev=$arg; done
words=$(cat)
{
  printf '\n# aider chat started at 2026-11-14 17:00:00\n\n'
  printf '> %s %s  \n> Aider v0.86.2  \n' "$0" "$*"
  printf '> Model: ollama_chat/qwen3:0.6b with whole edit format  \n\n'
  printf '%s\n' "$words" | sed 's/^/#### /; s/$/  /'
  printf '\nPONG\n\n> Tokens: 10 sent, 2 received.  \n'
} >> "$file"
"#;

/// A home with Aider installed (the stand-in), each fixture in a repository of its own below
/// `code`, and a copy of one where the walk never goes (`Library`).
fn home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("steery-aider-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("aider"), STAND_IN).unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(bin.join("aider"), std::fs::Permissions::from_mode(0o755)).unwrap();
    for (folder, fixture) in [
        ("code/mixed", "mixed.md"),
        ("code/driven", "driven.md"),
        ("Library/mixed", "mixed.md"),
    ] {
        let repository = home.join(folder);
        std::fs::create_dir_all(repository.join(".git")).unwrap();
        std::fs::copy(
            fixture_path(fixture),
            repository.join(".aider.chat.history.md"),
        )
        .unwrap();
    }
    home
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn history(home: &Path, folder: &str) -> PathBuf {
    home.join(folder).join(".aider.chat.history.md")
}

/// The driver, in an environment holding only `HOME` and `PATH`, initialized.
async fn start(home: &Path) -> Client {
    let client = Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            format!("PATH={}:/usr/bin:/bin", home.join("bin").display()),
            env!("CARGO_BIN_EXE_steery-driver-aider").into(),
        ],
        None,
    )
    .await
    .unwrap();
    let offered = wire::InitializeParams {
        protocol: vec![13, 14],
        runtime: wire::Runtime {
            release: "7.0.0".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        },
        home: home.to_owned(),
        config: home.join(".config/steery"),
        settings: Default::default(),
    };
    assert_eq!(client.initialize(&offered).await.unwrap().protocol, 14);
    client
}

async fn call(client: &Client, method: &str, params: Value) -> Result<Value, CallError> {
    client.call(method, params, Duration::from_secs(30)).await
}

fn of_type<'a>(read: &'a Value, kind: &str) -> Vec<&'a Value> {
    read["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == kind)
        .collect()
}

/// The person's conversation is listed, titled by its first words; the one only programs ran
/// is excluded; the copy in `Library` is neither. A read ends the failed run's turn `failed`
/// with litellm's words once the person's run begins, folds the thinking apart from the
/// answer and counts the tokens; a cursor read gives only what was appended, and a rewritten
/// file reads again from its start. A conversation no file has is `not-found`.
#[tokio::test]
async fn aider_lists_and_reads_its_chat_histories() {
    let home = home("sessions");
    let driver = start(&home).await;

    let listed = call(&driver, method::SESSIONS_LIST, json!({"transcripts": []}))
        .await
        .unwrap();
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{listed}");
    let mixed = &sessions[0];
    assert_eq!(mixed["title"], "Say hi.");
    assert_eq!(mixed["cwd"], json!(home.join("code/mixed")));
    assert_eq!(mixed["transcripts"], json!([history(&home, "code/mixed")]));
    assert_eq!(listed["excluded"].as_array().unwrap().len(), 1, "{listed}");
    let id = mixed["external_id"].as_str().unwrap();

    let read = call(&driver, method::SESSIONS_READ, json!({"external_id": id}))
        .await
        .unwrap();
    assert_eq!(read["whole"], true);
    let models: Vec<&Value> = of_type(&read, "model")
        .into_iter()
        .map(|event| &event["model"])
        .collect();
    assert_eq!(models, ["openai/gpt-4o", "ollama_chat/qwen3:0.6b"]);
    let ended = of_type(&read, "turn_ended");
    assert_eq!(ended.len(), 1, "the person's turn is still open: {read}");
    assert_eq!(ended[0]["state"], "failed");
    assert_eq!(ended[0]["failure"]["kind"], "sign_in");
    assert_eq!(ended[0]["failure"]["provider"], "openai");
    let words = of_type(&read, "words");
    let voices: Vec<(&str, &str)> = words
        .iter()
        .map(|w| (w["voice"].as_str().unwrap(), w["text"].as_str().unwrap()))
        .collect();
    assert_eq!(voices[0], ("person", "Say hi."));
    assert!(
        voices.contains(&("person", "What does hello.py print? One word, no edits.")),
        "{voices:?}"
    );
    let thought = voices
        .iter()
        .position(|(voice, text)| *voice == "thought" && text.starts_with("Okay, the user"))
        .expect("the model's thinking is a thought");
    assert_eq!(voices[thought + 1].0, "agent");
    assert!(!voices[thought + 1].1.contains("thinking-content"));
    let tokens = of_type(&read, "tokens");
    assert_eq!(
        (&tokens[0]["input"], &tokens[0]["output"]),
        (&json!(730), &json!(192))
    );

    let again = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": id, "cursor": read["cursor"]}),
    )
    .await
    .unwrap();
    assert_eq!(again["events"], json!([]));

    let path = history(&home, "code/mixed");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n#### And now?  \n\nStill hi.\n\n");
    std::fs::write(&path, &text).unwrap();
    let appended = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": id, "cursor": read["cursor"]}),
    )
    .await
    .unwrap();
    assert_eq!(appended["reset"], false);
    let kinds: Vec<&str> = appended["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["turn_ended", "turn_began", "words"], "{appended}");
    assert_eq!(appended["events"][0]["state"], "finished");
    // The answer waits: nothing says Aider finished writing it.

    // Aider only appends: a rewrite is another file in its place.
    std::fs::copy(fixture_path("driven.md"), &path).unwrap();
    let rewritten = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": id, "cursor": appended["cursor"]}),
    )
    .await
    .unwrap();
    assert_eq!(rewritten["reset"], true);
    assert_eq!(
        of_type(&rewritten, "words")[0]["text"],
        "What does hello.py print? Answer in one word, do not edit files."
    );

    let gone = call(
        &driver,
        method::SESSIONS_READ,
        json!({"external_id": "nope"}),
    )
    .await;
    assert!(
        matches!(&gone, Err(CallError::Refused { word, .. }) if word == "not-found"),
        "{gone:?}"
    );
}

/// A new turn runs Aider on the words from standard input, in the repository's root, with a
/// history file of its own beside the one already there; its lines name the conversation and
/// end it `finished`. A continue restores that file, the receipt finds the delivery marker
/// only after the mark, and the terminal command reopens the conversation.
#[tokio::test]
async fn aider_drives_a_turn_and_continues_it() {
    let home = home("drive");
    let driver = start(&home).await;
    let repository = home.join("code/mixed");
    let cwd = repository.join("src");
    std::fs::create_dir_all(&cwd).unwrap();

    let launch = call(
        &driver,
        method::START,
        json!({"act": "new", "cwd": cwd, "folder": home.join("turn"), "worktree": false,
               "model": "ollama_chat/qwen3:0.6b"}),
    )
    .await
    .unwrap();
    let argv: Vec<String> = serde_json::from_value(launch["argv"].clone()).unwrap();
    assert_eq!(argv[1], "turn");
    assert_eq!(argv[argv.len() - 2..], ["--message-file", "/dev/stdin"]);
    let file = &argv[argv
        .iter()
        .position(|arg| arg == "--chat-history-file")
        .unwrap()
        + 1];
    assert_eq!(Path::new(file).parent().unwrap(), repository);
    assert_ne!(Path::new(file), history(&home, "code/mixed"));

    let lines = turn(&driver, &home, &launch, "Reply with PONG").await;
    let id = lines[0]["named"].as_str().unwrap().to_owned();
    assert_eq!(
        lines.last().unwrap()["ended"]["state"],
        "finished",
        "{lines:?}"
    );
    let located = call(&driver, method::SESSIONS_LOCATE, json!({"external_id": id}))
        .await
        .unwrap();
    assert_eq!(located["session"]["transcripts"], json!([file]));
    let read = call(&driver, method::SESSIONS_READ, json!({"external_id": id}))
        .await
        .unwrap();
    let said: Vec<&Value> = of_type(&read, "words")
        .into_iter()
        .map(|w| &w["text"])
        .collect();
    assert_eq!(said, ["Reply with PONG", "PONG"], "{read}");

    let mark = call(&driver, method::RECEIPT_MARK, json!({"external_id": id}))
        .await
        .unwrap();
    let find = json!({"external_id": id, "mark": mark["mark"], "delivery": "d42"});
    let before = call(&driver, method::RECEIPT_FIND, find.clone())
        .await
        .unwrap();
    assert_eq!(before["found"], false);

    let continued = json!({"act": "continue", "external_id": id, "cwd": cwd,
                           "folder": home.join("turn"), "worktree": false});
    let elsewhere = call(&driver, method::START, continued.clone()).await;
    assert!(
        matches!(&elsewhere, Err(CallError::Refused { word, .. }) if word == "no-place"),
        "{elsewhere:?}"
    );
    let mut continued = continued;
    continued["cwd"] = json!(repository);
    let launch = call(&driver, method::START, continued).await.unwrap();
    let argv: Vec<String> = serde_json::from_value(launch["argv"].clone()).unwrap();
    assert!(
        argv.contains(&"--restore-chat-history".to_owned()),
        "{argv:?}"
    );
    let lines = turn(&driver, &home, &launch, "Again.\n\nsteery-delivery:d42").await;
    assert_eq!(lines[0]["named"], id.as_str());
    let after = call(&driver, method::RECEIPT_FIND, find).await.unwrap();
    assert_eq!(after["found"], true);

    let terminal = call(
        &driver,
        method::TERMINAL,
        json!({"external_id": id, "cwd": cwd}),
    )
    .await
    .unwrap();
    let command = terminal["command"].as_str().unwrap();
    assert!(command.contains("--restore-chat-history"), "{command}");
    assert!(command.contains(file.as_str()), "{command}");
}

/// Run a launch as the runtime does, its words on standard input, and read its lines.
async fn turn(driver: &Client, home: &Path, launch: &Value, words: &str) -> Vec<Value> {
    let argv: Vec<String> = serde_json::from_value(launch["argv"].clone()).unwrap();
    let mut turn = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(launch["cwd"].as_str().unwrap())
        .env_clear()
        .env("HOME", home)
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
        .write_all(words.as_bytes())
        .unwrap();
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
