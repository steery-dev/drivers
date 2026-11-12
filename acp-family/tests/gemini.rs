//! The Gemini CLI driver's held turn, run as the runtime runs it: `setup/plan` and `start`
//! over the program protocol, then the launch plan's command, which runs the real
//! `gemini --acp` against a model API on the loopback and dials this test as Steery's
//! plugin door. Gemini CLI must be on `PATH`; without it the test says so and passes.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use steery_sdk::client::Client;
use steery_sdk::wire::door::{Ask, DoorMode, DoorSayMode, PluginFrame, RuntimeFrame};
use steery_sdk::{method, wire};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

const WAIT: Duration = Duration::from_secs(30);
const MODEL: &str = "gemini-2.5-flash";

/// A Gemini API on the loopback. It answers each prompt with `heard: <its words>`, and holds
/// a prompt whose words say `slow` until the test releases it.
struct Model {
    url: String,
    /// The last user words of each `streamGenerateContent` request, in order.
    heard: Arc<Mutex<Vec<String>>>,
    release: Arc<Semaphore>,
}

impl Model {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let heard = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(Semaphore::new(0));
        let (kept, gate) = (heard.clone(), release.clone());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, kept.clone(), gate.clone()));
            }
        });
        Self {
            url,
            heard,
            release,
        }
    }

    fn heard(&self) -> Vec<String> {
        self.heard.lock().unwrap().clone()
    }

    /// Wait until the model has heard `words` as a prompt.
    async fn hears(&self, words: &str) {
        tokio::time::timeout(WAIT, async {
            while !self.heard().iter().any(|heard| heard == words) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the model never heard {words:?}: {:?}", self.heard()));
    }
}

/// One request on one connection, which the answer closes.
async fn serve(mut stream: TcpStream, heard: Arc<Mutex<Vec<String>>>, release: Arc<Semaphore>) {
    let mut read = Vec::new();
    let mut buffer = [0u8; 16384];
    let (head, length) = loop {
        let Ok(n) = stream.read(&mut buffer).await else {
            return;
        };
        if n == 0 {
            return;
        }
        read.extend_from_slice(&buffer[..n]);
        if let Some(end) = read.windows(4).position(|four| four == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&read[..end]).into_owned();
            let length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            read.drain(..end + 4);
            break (head, length);
        }
    };
    while read.len() < length {
        let Ok(n) = stream.read(&mut buffer).await else {
            return;
        };
        if n == 0 {
            return;
        }
        read.extend_from_slice(&buffer[..n]);
    }
    let path = head.split(' ').nth(1).unwrap_or_default().to_owned();
    let body: Value = serde_json::from_slice(&read).unwrap_or(Value::Null);
    // The prompt's own words are its last part; the first turn's parts begin with context.
    let words = body["contents"]
        .as_array()
        .and_then(|contents| contents.last())
        .and_then(|content| content["parts"].as_array())
        .and_then(|parts| parts.iter().rev().find_map(|part| part["text"].as_str()))
        .unwrap_or_default()
        .to_owned();
    let answer = json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": format!("heard: {words}")}]},
            "finishReason": "STOP",
            "index": 0,
        }],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 3, "totalTokenCount": 13},
    });
    let (kind, text) = if path.contains("streamGenerateContent") {
        heard.lock().unwrap().push(words.clone());
        if words.contains("slow") {
            release.acquire().await.unwrap().forget();
        }
        ("text/event-stream", format!("data: {answer}\r\n\r\n"))
    } else if path.contains("countTokens") {
        ("application/json", json!({"totalTokens": 10}).to_string())
    } else {
        ("application/json", answer.to_string())
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
        text.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// The runtime's side of one door connection.
struct Door {
    socket: WebSocketStream<TcpStream>,
    /// The last state the copy sent.
    state: Value,
}

impl Door {
    async fn send(&mut self, frame: RuntimeFrame) {
        let text = serde_json::to_string(&frame).unwrap();
        self.socket.send(Message::text(text)).await.unwrap();
    }

    async fn ask(&mut self, ask: Ask) -> PluginFrame {
        let id = match &ask {
            Ask::Say { id, .. }
            | Ask::Pick { id, .. }
            | Ask::Stop { id, .. }
            | Ask::Force { id, .. }
            | Ask::Compact { id, .. } => id.clone(),
        };
        self.send(RuntimeFrame::Ask(ask)).await;
        self.answer(&id).await
    }

    /// The next frame; a state also goes to `self.state`. None once the copy closes.
    async fn frame(&mut self) -> Option<PluginFrame> {
        loop {
            let message = tokio::time::timeout(WAIT, self.socket.next())
                .await
                .unwrap_or_else(|_| panic!("the copy sent nothing in time; state {}", self.state))?
                .ok()?;
            match message {
                Message::Text(text) => {
                    let frame: PluginFrame = serde_json::from_str(text.as_str()).unwrap();
                    if let PluginFrame::State { state } = &frame {
                        self.state = json!(state);
                    }
                    return Some(frame);
                }
                Message::Close(_) => return None,
                _ => {}
            }
        }
    }

    /// The next frame that is not a state.
    async fn next(&mut self) -> Option<PluginFrame> {
        loop {
            match self.frame().await? {
                PluginFrame::State { .. } => {}
                frame => return Some(frame),
            }
        }
    }

    async fn answer(&mut self, id: &str) -> PluginFrame {
        match self.next().await {
            Some(frame @ PluginFrame::Answer { .. }) if matches!(&frame, PluginFrame::Answer { id: answered, .. } if answered == id) => {
                frame
            }
            other => panic!("expected the answer to {id}, got {other:?}"),
        }
    }

    /// Read states until one fits `fits`.
    async fn until(&mut self, fits: impl Fn(&Value) -> bool) {
        while !fits(&self.state) {
            match self.frame().await {
                Some(PluginFrame::State { .. }) => {}
                other => panic!("expected a state, got {other:?}"),
            }
        }
    }
}

fn say(id: &str, words: &str, mode: DoorSayMode, idle: bool) -> Ask {
    Ask::Say {
        id: id.into(),
        words: words.into(),
        mode,
        idle,
        throw: None,
    }
}

fn ok(frame: &PluginFrame) -> &Value {
    match frame {
        PluginFrame::Answer { ok: Some(ok), .. } => ok,
        other => panic!("expected an ok answer, got {other:?}"),
    }
}

fn refused(frame: &PluginFrame) -> &str {
    match frame {
        PluginFrame::Answer {
            refused: Some(word),
            ..
        } => word,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// `PATH` for the driver and Gemini: Gemini's folder, `node`'s, and the system's.
fn path(gemini: &Path) -> Option<String> {
    let node =
        std::env::split_paths(&std::env::var_os("PATH")?).find(|dir| dir.join("node").is_file())?;
    let dirs = [
        gemini.parent()?.to_owned(),
        node,
        "/usr/bin".into(),
        "/bin".into(),
    ];
    Some(
        std::env::join_paths(dirs)
            .ok()?
            .to_string_lossy()
            .into_owned(),
    )
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|file| file.is_file())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_held_turn_takes_every_word_through_the_door() {
    let Some(gemini) = on_path("gemini") else {
        eprintln!("skipped: no gemini on PATH");
        return;
    };
    let path = path(&gemini).expect("node on PATH beside gemini");
    let home = std::env::temp_dir().join(format!("steery-gemini-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(home.join(".gemini")).unwrap();
    std::fs::write(
        home.join(".gemini/settings.json"),
        json!({"model": {"name": MODEL}, "security": {"folderTrust": {"enabled": false}}})
            .to_string(),
    )
    .unwrap();
    let model = Model::start().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let door_url = format!("ws://{}/door", listener.local_addr().unwrap());
    let env = [
        format!("HOME={}", home.display()),
        format!("PATH={path}"),
        format!("GOOGLE_GEMINI_BASE_URL={}", model.url),
    ];

    let mut args = vec!["-i".to_owned()];
    args.extend(env.iter().cloned());
    args.push(env!("CARGO_BIN_EXE_steery-driver-gemini").into());
    let driver = Client::spawn(Path::new("/usr/bin/env"), &args, None)
        .await
        .unwrap();
    let offered = wire::InitializeParams {
        protocol: vec![13],
        runtime: wire::Runtime {
            release: "7.0.0".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        },
        home: home.clone(),
        config: home.join(".config/steery"),
        settings: Default::default(),
    };
    driver.initialize(&offered).await.unwrap();

    // The runtime writes the plan's files; the door's address is one of them.
    let planned = driver
        .call(
            method::SETUP_PLAN,
            json!({"mcp_url": "http://127.0.0.1:7/mcp", "skill": "# Steery",
                   "hook": "steery hook gemini", "door": door_url,
                   "release": "7.0.0", "timeout": 600}),
            WAIT,
        )
        .await
        .unwrap();
    for entry in planned["entries"].as_array().unwrap() {
        if entry["format"] == "text" {
            let file = Path::new(entry["path"].as_str().unwrap());
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, entry["value"].as_str().unwrap()).unwrap();
        }
    }
    let launch: wire::harness::Launch = serde_json::from_value(
        driver
            .call(
                method::START,
                json!({"act": "new", "cwd": work, "folder": home.join("turn"), "worktree": false}),
                WAIT,
            )
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(launch.stdin, wire::harness::Stdin::Held, "{launch:?}");

    // The turn as the runtime runs it: its standard input open and silent.
    let mut turn = tokio::process::Command::new(&launch.argv[0])
        .args(&launch.argv[1..])
        .env_clear()
        .envs(env.iter().map(|pair| pair.split_once('=').unwrap()))
        .envs(&launch.env)
        .current_dir(&launch.cwd)
        .stdin(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let (stream, _) = tokio::time::timeout(Duration::from_secs(90), listener.accept())
        .await
        .expect("the turn never dialed the door")
        .unwrap();
    let mut door = Door {
        socket: tokio_tungstenite::accept_async(stream).await.unwrap(),
        state: Value::Null,
    };
    let session = match door.next().await {
        Some(PluginFrame::Hello {
            harness,
            pid,
            session_id,
            mode,
            cwd,
            ..
        }) => {
            assert_eq!(harness, "gemini");
            assert_eq!(pid, turn.id().unwrap());
            assert_eq!(mode, DoorMode::Rpc);
            assert_eq!(
                cwd.unwrap().canonicalize().unwrap(),
                work.canonicalize().unwrap()
            );
            session_id
        }
        other => panic!("expected a hello, got {other:?}"),
    };
    door.send(RuntimeFrame::Welcome {
        door: steery_sdk::wire::door::DOOR,
        entries: None,
        live: false,
    })
    .await;

    // Words to an idle conversation go in at once.
    let first = door
        .ask(say("s1", "slow first", DoorSayMode::Steer, false))
        .await;
    assert!(ok(&first)["taken_at"].is_u64(), "{first:?}");
    model.hears("slow first").await;

    // While it runs, words wait for its end, as a second prompt would abort it.
    let held = door
        .ask(say("s2", "second", DoorSayMode::Queue, false))
        .await;
    assert!(ok(&held)["queued_at"].is_u64(), "{held:?}");
    assert!(ok(&held).get("taken_at").is_none(), "{held:?}");
    let busy = door
        .ask(say("s3", "only when idle", DoorSayMode::Steer, true))
        .await;
    assert_eq!(refused(&busy), "harness-busy");
    let compact = door
        .ask(Ask::Compact {
            id: "c1".into(),
            before: 0,
            summary: String::new(),
            least: 0,
        })
        .await;
    assert_eq!(refused(&compact), "harness-busy");
    door.until(|state| state["pending"] == true).await;

    // The same say again is answered once its words go in: after the first turn ends.
    door.send(RuntimeFrame::Ask(say(
        "s2",
        "second",
        DoorSayMode::Queue,
        false,
    )))
    .await;
    model.release.add_permits(1);
    let taken = door.answer("s2").await;
    assert!(ok(&taken)["taken_at"].is_u64(), "{taken:?}");
    model.hears("second").await;
    door.until(|state| {
        state["turn"].is_null()
            && state["settled"]
                .as_array()
                .is_some_and(|settled| settled.len() == 2)
    })
    .await;
    assert_eq!(
        door.state["settled"][0]["says"],
        json!(["s1"]),
        "{}",
        door.state
    );
    assert_eq!(
        door.state["settled"][1]["says"],
        json!(["s2"]),
        "{}",
        door.state
    );
    assert_eq!(model.heard(), ["slow first", "second"]);

    // A stop cancels the running turn; with none running, it stops nothing.
    let third = door
        .ask(say("s4", "slow stop me", DoorSayMode::Steer, false))
        .await;
    assert!(ok(&third)["taken_at"].is_u64(), "{third:?}");
    model.hears("slow stop me").await;
    let stopped = door
        .ask(Ask::Stop {
            id: "x1".into(),
            say: None,
        })
        .await;
    assert_eq!(ok(&stopped)["stopped"], true, "{stopped:?}");
    door.until(|state| state["turn"].is_null()).await;
    let idle = door
        .ask(Ask::Stop {
            id: "x2".into(),
            say: None,
        })
        .await;
    assert_eq!(ok(&idle)["stopped"], false, "{idle:?}");
    let picked = door
        .ask(Ask::Pick {
            id: "p1".into(),
            model: None,
            thinking: Some("high".into()),
        })
        .await;
    assert_eq!(refused(&picked), "selection-unsupported");

    // The runtime's signal ends the turn: the copy closes the door and exits.
    std::process::Command::new("kill")
        .args(["-TERM", &turn.id().unwrap().to_string()])
        .status()
        .unwrap();
    assert!(
        door.next().await.is_none(),
        "the copy did not close the door"
    );
    let status = tokio::time::timeout(WAIT, turn.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "{status}");

    // The conversation the door ran is the one the driver lists, from Gemini's own file.
    let listed = driver
        .call(method::SESSIONS_LIST, json!({}), WAIT)
        .await
        .unwrap();
    let ids: Vec<&str> = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|listed| listed["external_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [session.as_str()], "{listed}");
    let read = driver
        .call(method::SESSIONS_READ, json!({"external_id": session}), WAIT)
        .await
        .unwrap();
    let text = read.to_string();
    for words in ["slow first", "second", "heard: second", "slow stop me"] {
        assert!(text.contains(words), "{words:?} not in {read}");
    }
    driver.shutdown().await.unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
