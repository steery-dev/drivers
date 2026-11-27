//! The driver, run as the runtime runs it, over standard input and output: against a
//! transcript the canvas's contract describes (`fixtures/sessions`), a finished turn with
//! both voices' lines and tokens, then one that failed on a usage limit; and its own turn
//! mode, the canvas's harness, against a mock of the Messages API on loopback.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use steery_sdk::client::{CallError, Client};
use steery_sdk::{method, wire};

const FIXTURE: &str = "019ac4a0-1000-7000-8000-000000000001";

/// A home whose canvas folder holds the fixture.
fn home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("steery-canvas-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
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

/// `HOME`, a `PATH` with no Claude Code, `CANVAS_HOME`, and the variables of `claude`.
fn environment(home: &Path, claude: &[(String, String)]) -> Vec<(String, String)> {
    let mut environment = vec![
        ("HOME".into(), home.display().to_string()),
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("CANVAS_HOME".into(), canvas_home(home).display().to_string()),
    ];
    environment.extend(claude.iter().cloned());
    environment
}

/// The driver, in [`environment`], initialized.
async fn start(home: &Path, claude: &[(String, String)]) -> Client {
    let mut argv = vec!["-i".to_owned()];
    argv.extend(environment(home, claude).into_iter().map(|(k, v)| format!("{k}={v}")));
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
    let driver = start(&home, &[]).await;

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

/// `models` and `setup/plan` against a mock of the Messages API, as a gateway
/// (`ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`): signed in, the plotter's model first, the
/// driver itself found, and the door written to `steery.json`. A new turn runs this driver's
/// own turn mode on the words: it names the session, sends the sketcher the words alone and
/// the plotter the sketch's lines, prints each line of both voices in order (comments and
/// blank lines left out) and each call's tokens, and ends `finished`; its transcript holds
/// the same records, and `turn/read` gives the events `sessions/read` gives. A continue names
/// the session, and the receipt finds the delivery marker only after the mark. A key the
/// gateway refuses ends a turn `failed`, kind `sign_in`, and plans the sign-in need with the
/// gateway's words.
#[tokio::test]
async fn canvas_draws_a_turn_on_claude_and_continues_it() {
    let home = home("drive");
    let mock = Mock::start();
    let signed = mock.environment("good");
    let driver = start(&home, &signed).await;
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();

    let models = call(&driver, method::MODELS, json!({})).await.unwrap();
    assert_eq!(models["models"][0]["model"], "claude-sonnet-5-5");
    assert_eq!(models["signed_in"], true, "{models}");
    let setup = json!({"mcp_url": "http://127.0.0.1:7777/mcp", "skill": "",
        "hook": "steery hook canvas", "door": "ws://127.0.0.1:7777/door", "release": "7.0.0",
        "timeout": 120});
    let plan = call(&driver, method::SETUP_PLAN, setup.clone()).await.unwrap();
    assert_eq!(plan["found"]["binary"], env!("CARGO_BIN_EXE_steery-driver-canvas"));
    assert!(plan["install"].is_null(), "{plan}");
    assert!(plan["needs"].is_null(), "signed in, so no need: {plan}");
    assert_eq!(
        plan["entries"],
        json!([{"path": canvas_home(&home).join("steery.json"), "format": "json", "at": "/door",
                "value": "ws://127.0.0.1:7777/door", "carries": ["plugin"]}])
    );

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
    assert_eq!(
        argv,
        [env!("CARGO_BIN_EXE_steery-driver-canvas").to_owned(), "turn".into(), "--cwd".into(),
            cwd.display().to_string()]
    );

    let (said, ok) = run(&home, &launch, &signed, "Sketch the login flow");
    assert!(ok, "{said:?}");
    let id = said[0]["named"].as_str().unwrap().to_owned();
    let voices: Vec<(&str, &str)> = said
        .iter()
        .filter_map(|line| Some((line["line"]["voice"].as_str()?, line["line"]["text"].as_str()?)))
        .collect();
    assert_eq!(
        voices,
        [
            ("sketcher", "box login \"Login\""),
            ("sketcher", "box home \"Home\""),
            ("plotter", "box login \"Login\" in=flow"),
            ("plotter", "login -> home \"signs in\""),
            ("plotter", "reply \"Drew the login flow.\""),
        ],
        "{said:?}"
    );
    let shapes: Vec<&str> = said
        .iter()
        .map(|line| line.as_object().unwrap().keys().next().unwrap().as_str())
        .collect();
    assert_eq!(
        shapes,
        ["named", "line", "line", "tokens", "line", "line", "line", "tokens", "ended"]
    );
    let tokens: Vec<&Value> = said.iter().filter_map(|line| line.get("tokens")).collect();
    assert_eq!(
        (&tokens[0]["voice"], &tokens[0]["model"], &tokens[0]["in"], &tokens[0]["cached"],
            &tokens[0]["out"]),
        (&json!("sketcher"), &json!("claude-haiku-5-5"), &json!(100), &json!(40), &json!(12))
    );
    assert_eq!(tokens[1]["model"], "claude-sonnet-5-5");
    let ended = &said.last().unwrap()["ended"];
    assert_eq!(ended["state"], "finished", "{said:?}");
    assert!(ended.get("failure").is_none(), "{ended}");

    let asked = mock.asked();
    assert_eq!(asked.len(), 2, "{asked:?}");
    assert_eq!(asked[0]["model"], "claude-haiku-5-5");
    assert_eq!(
        asked[0]["messages"][0]["content"],
        "Canvas outline:\n(empty)\n\nNew words:\ntype \"Sketch the login flow\""
    );
    assert_eq!(asked[0]["thinking"]["type"], "disabled");
    assert_eq!(asked[0]["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(asked[1]["model"], "claude-sonnet-5-5");
    assert_eq!(
        asked[1]["messages"][0]["content"],
        "Canvas outline:\n(empty)\n\nSketch to refine:\nbox login \"Login\"\nbox home \"Home\"\
         \n\nWords (the person stopped):\ntype \"Sketch the login flow\""
    );

    let path = canvas_home(&home).join(format!("sessions/{id}.jsonl"));
    let records: Vec<Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let record_kinds: Vec<&str> = records.iter().map(|r| r["kind"].as_str().unwrap()).collect();
    assert_eq!(
        record_kinds,
        ["session", "turn_began", "words", "line", "line", "tokens", "line", "line", "line",
            "tokens", "turn_ended"]
    );
    assert_eq!(records[0]["title"], "Sketch the login flow");
    assert_eq!(records[0]["cwd"], json!(cwd));
    assert_eq!(records[2]["text"], "Sketch the login flow");
    let turn_id = records[1]["turn"].as_str().unwrap();
    assert_eq!(records[3]["message"], format!("{turn_id}/sketcher"));
    assert_eq!(records[6]["message"], format!("{turn_id}/plotter"));
    // Each JSON line is its record: the same id, time and fields.
    for (line, record) in said[1..].iter().zip(&records[3..]) {
        let body = line.as_object().unwrap().values().next().unwrap();
        for (key, value) in body.as_object().unwrap() {
            assert_eq!(&record[key], value, "{key} of {line} and {record}");
        }
    }
    assert!(!canvas_home(&home).join(format!("sessions/{id}.pid")).exists());

    let located = call(&driver, method::SESSIONS_LOCATE, json!({"external_id": id}))
        .await
        .unwrap();
    assert_eq!(located["session"]["title"], "Sketch the login flow");
    let read = call(&driver, method::SESSIONS_READ, json!({"external_id": id}))
        .await
        .unwrap();
    let given = read_lines(&driver, &said).await;
    let words = of_type(&read, "words");
    let spent = of_type(&read, "tokens");
    let expected: Vec<&Value> = vec![words[1], words[2], spent[0], words[3], words[4], words[5],
        spent[1]];
    assert_eq!(given.iter().collect::<Vec<_>>(), expected, "{read}");
    assert_eq!(of_type(&read, "model")[0]["model"], "claude-sonnet-5-5");

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
    let (again, ok) = run(&home, &launch, &signed, "Again.\n\nsteery-delivery:d42");
    assert!(ok, "{again:?}");
    assert_eq!(again[0]["named"], id.as_str());
    let after = call(&driver, method::RECEIPT_FIND, find).await.unwrap();
    assert_eq!(after["found"], true);
    let held = call(&driver, method::HELD, json!({"external_id": id})).await.unwrap();
    assert!(held["nobody"].is_object(), "the turn removed its .pid: {held}");

    let refused = mock.environment("bad");
    let (failed, ok) = run(&home, &launch, &refused, "Once more");
    assert!(!ok, "a failed turn exits 1: {failed:?}");
    assert_eq!(failed.len(), 2, "{failed:?}");
    let ended = &failed[1]["ended"];
    assert_eq!(ended["state"], "failed");
    assert_eq!(
        ended["failure"],
        json!({"kind": "sign_in", "service": mock.base,
               "line": format!("Your Claude gateway at {} answered HTTP 401: invalid x-api-key.",
                   mock.base)})
    );
    let read = call(&driver, method::SESSIONS_READ, json!({"external_id": id}))
        .await
        .unwrap();
    let last = of_type(&read, "turn_ended").pop().unwrap().clone();
    assert_eq!(last["failure"]["kind"], "sign_in", "{read}");

    let signed_out = start(&home, &refused).await;
    let models = call(&signed_out, method::MODELS, json!({})).await.unwrap();
    assert_eq!(models["signed_in"], false);
    let plan = call(&signed_out, method::SETUP_PLAN, setup).await.unwrap();
    let need = &plan["needs"][0];
    assert_eq!(
        need["line"],
        format!("Your Claude gateway at {} refused the key (HTTP 401): invalid x-api-key.", mock.base)
    );
    let ways: Vec<&str> = need["ways"]
        .as_array()
        .unwrap()
        .iter()
        .map(|way| way["id"].as_str().unwrap())
        .collect();
    assert_eq!(ways, ["api-key", "gateway"], "no Claude Code here, so no plan sign-in");
}

/// Run a launch as the runtime does, in an environment of `HOME`, `PATH`, `CANVAS_HOME` and
/// `claude`, its words on standard input: its JSON lines, and whether it exited 0.
fn run(home: &Path, launch: &Value, claude: &[(String, String)], words: &str) -> (Vec<Value>, bool) {
    let argv: Vec<String> = serde_json::from_value(launch["argv"].clone()).unwrap();
    let mut turn = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(launch["cwd"].as_str().unwrap())
        .env_clear()
        .envs(environment(home, claude))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    turn.stdin.take().unwrap().write_all(words.as_bytes()).unwrap();
    let output = turn.wait_with_output().unwrap();
    let lines = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (lines, output.status.success())
}

/// The events `turn/read` gives for each of a turn's lines.
async fn read_lines(driver: &Client, lines: &[Value]) -> Vec<Value> {
    let mut events = Vec::new();
    for line in lines {
        let read = call(
            driver,
            method::TURN_READ,
            json!({"turn": "t1", "line": line.to_string(), "stream": "stdout"}),
        )
        .await
        .unwrap();
        events.extend(read["events"].as_array().cloned().unwrap_or_default());
    }
    events
}

/// A gateway of the Messages API on loopback: `GET /v1/models`, and a streamed `POST
/// /v1/messages` whose text is the sketch for the sketcher's model and the refine for the
/// plotter's. A key other than `good` is refused with 401, as Anthropic's API says it.
struct Mock {
    base: String,
    asked: Arc<Mutex<Vec<Value>>>,
}

impl Mock {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let kept = Arc::clone(&asked);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let kept = Arc::clone(&kept);
                std::thread::spawn(move || answer(stream, &kept));
            }
        });
        Self { base, asked }
    }

    fn environment(&self, token: &str) -> Vec<(String, String)> {
        vec![
            ("ANTHROPIC_BASE_URL".into(), self.base.clone()),
            ("ANTHROPIC_AUTH_TOKEN".into(), token.into()),
        ]
    }

    fn asked(&self) -> Vec<Value> {
        self.asked.lock().unwrap().clone()
    }
}

fn answer(mut stream: TcpStream, asked: &Mutex<Vec<Value>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut head = String::new();
    reader.read_line(&mut head).unwrap();
    let (mut length, mut token) = (0, String::new());
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().unwrap(),
            "authorization" => token = value.trim().trim_start_matches("Bearer ").to_owned(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    let (status, kind, text) = if token != "good" {
        (
            "401 Unauthorized",
            "application/json",
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#
                .to_owned(),
        )
    } else if head.starts_with("GET /v1/models ") {
        ("200 OK", "application/json", r#"{"data":[]}"#.to_owned())
    } else {
        let request: Value = serde_json::from_slice(&body).unwrap();
        let sketch = request["model"] == "claude-haiku-5-5";
        asked.lock().unwrap().push(request);
        let events = match sketch {
            true => sse(
                &["box login \"Log", "in\"\n# the login page\n\n", "box home \"Home\""],
                (100, 40, 12),
            ),
            false => sse(
                &["box login \"Login\" in=flow\nlogin -> home \"signs in\"\n",
                    "reply \"Drew the login flow.\""],
                (900, 600, 30),
            ),
        };
        ("200 OK", "text/event-stream", events)
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
        text.len()
    );
}

/// A streamed Messages answer of one text block in these deltas, with these input, cached
/// and output tokens.
fn sse(deltas: &[&str], (input, cached, output): (u64, u64, u64)) -> String {
    let mut events = vec![
        json!({"type": "message_start", "message": {"usage": {"input_tokens": input - cached,
            "cache_read_input_tokens": cached, "cache_creation_input_tokens": 0,
            "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    ];
    events.extend(deltas.iter().map(|text| {
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}})
    }));
    events.extend([
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": output}}),
        json!({"type": "message_stop"}),
    ]);
    events
        .iter()
        .map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap()))
        .collect()
}
