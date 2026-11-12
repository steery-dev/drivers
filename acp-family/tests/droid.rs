//! The Droid driver, run as the runtime runs it, over standard input and output, against a
//! home of its own that holds real session files of Droid 1.9.0 (`fixtures/droid/home`),
//! run on a local custom model: a turn that ran `echo droid-ok` and the turn after it when
//! the conversation resumed; a fork of that conversation; a turn whose model service was
//! unreachable; two turns killed while the model wrote; a resumed conversation whose second
//! turn's MCP call its person rejected; and a `droid exec` run that a person never typed
//! into. Then the documents Droid handed its hooks (`fixtures/droid/hooks`), the setup of a
//! home with a `droid` of its own, and the MCP servers a project loads.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use steery_sdk::client::{CallError, Client};
use steery_sdk::{method, wire};

const WAIT: Duration = Duration::from_secs(10);

/// `echo droid-ok`, then `Now say done in one word.` after a resume.
const RESUMED: &str = "8010a0e8-6d36-4d57-9a35-4c3aaccbb5d1";
/// A fork of [`RESUMED`] after both its turns.
const FORK: &str = "29852e7b-d427-4733-bcc7-19bb730e945b";
/// `Say hi.` with the model at `http://localhost:1`.
const FAILED: &str = "31fd3689-f0b2-441b-abb5-d12a08d574fe";
/// An essay, its `droid exec` killed while the model wrote.
const KILLED: &str = "0f804d81-5e71-403a-8310-5c7c08995d9e";
/// ToolSearch, then an MCP call (`echo___say`) whose permission was rejected.
const HOOKED: &str = "b60f5705-cdc1-4e36-8fec-d020525e90c9";
/// A conversation Droid opened that no prompt reached.
const UNTYPED: &str = "32bc4349-0a6c-46e8-8d68-5f1e708b0861";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/droid")
}

/// A copy of `from` under `to`, every file last changed an hour ago, as a store whose
/// turns stood still.
fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    let old = SystemTime::now() - Duration::from_secs(3600);
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&target)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
    }
}

/// A home of this test's own: the fixture store, a `droid` of its own that names `release`,
/// and MCP servers of the person and of a project.
fn home(name: &str, release: Option<&str>) -> PathBuf {
    let home =
        std::env::temp_dir().join(format!("steery-droid-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    copy(&fixtures().join("home"), &home);
    if let Some(release) = release {
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let droid = bin.join("droid");
        std::fs::write(&droid, format!("#!/bin/sh\necho {release}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&droid, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let own = json!({"mcpServers": {
        "echo": {"type": "stdio", "command": "/usr/bin/python3", "args": ["/tmp/echo.py"]},
        "notes": {"type": "http", "url": "https://notes.example/mcp"},
    }});
    std::fs::write(home.join(".factory/mcp.json"), own.to_string()).unwrap();
    let project = home.join("work/app/.factory");
    std::fs::create_dir_all(&project).unwrap();
    let off =
        json!({"mcpServers": {"notes": {"type": "http", "url": "https://n", "disabled": true}}});
    std::fs::write(project.join("mcp.json"), off.to_string()).unwrap();
    home
}

async fn start(home: &Path) -> Client {
    let client = Client::spawn(
        Path::new("/usr/bin/env"),
        &[
            "-i".into(),
            format!("HOME={}", home.display()),
            "PATH=/usr/bin:/bin".into(),
            env!("CARGO_BIN_EXE_steery-driver-droid").into(),
        ],
        None,
    )
    .await
    .unwrap();
    let offered = wire::InitializeParams {
        protocol: vec![13],
        runtime: wire::Runtime {
            release: "7.0.0".into(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        },
        home: home.to_owned(),
        config: home.join(".config/steery"),
        settings: Default::default(),
    };
    client.initialize(&offered).await.unwrap();
    client
}

async fn call(client: &Client, method: &str, params: Value) -> Value {
    client.call(method, params, WAIT).await.unwrap()
}

async fn events(client: &Client, id: &str) -> Vec<Value> {
    let read = call(client, method::SESSIONS_READ, json!({"external_id": id})).await;
    assert_eq!(read["whole"], true, "{read}");
    read["events"].as_array().unwrap().clone()
}

/// Each event as one short line.
fn lines(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event["type"].as_str().unwrap() {
            "words" => format!("words {}", event["voice"].as_str().unwrap()),
            "turn_ended" => format!("turn_ended {}", event["state"].as_str().unwrap()),
            "call_began" => format!("call_began {}", event["call"]["group"].as_str().unwrap()),
            kind => kind.to_owned(),
        })
        .collect()
}

/// What the person said in `events`.
fn person(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter(|event| event["voice"] == "person")
        .map(|event| event["text"].as_str().unwrap())
        .collect()
}

fn setup() -> Value {
    json!({
        "mcp_url": "http://127.0.0.1:7777/mcp",
        "skill": "# Steery",
        "hook": "/opt/steery/hook",
        "door": "http://127.0.0.1:7777/door",
        "release": "7.0.0",
        "timeout": 60,
        "trusts": [],
    })
}

#[tokio::test]
async fn reads_sessions_droid_wrote_and_the_hooks_droid_runs() {
    let home = home("store", Some("droid 1.9.0"));
    let client = start(&home).await;

    let listed = call(&client, method::SESSIONS_LIST, json!({"transcripts": []})).await;
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 7, "{listed}");
    assert_eq!(listed["excluded"], json!([UNTYPED]), "{listed}");
    assert_eq!(listed["complete"], true);
    let session = |id: &str| {
        sessions
            .iter()
            .find(|session| session["external_id"] == id)
            .unwrap()
            .clone()
    };
    let resumed = session(RESUMED);
    assert_eq!(resumed["cwd"], "/private/tmp/droid-obs");
    assert_eq!(
        resumed["title"],
        "Call the Execute tool with parameters {\"command\": \"echo droid-ok\", \"riskLevel\": \"low\"}. Then reply with what it printed."
    );
    assert_eq!(resumed["created_at"], 1_794_494_791_322_u64);
    assert!(resumed.get("parent").is_none());
    let transcript = home.join(format!(
        ".factory/sessions/-private-tmp-droid-obs/{RESUMED}.jsonl"
    ));
    assert_eq!(resumed["transcripts"], json!([transcript]));
    assert_eq!(session(FORK)["parent"], RESUMED);

    let located = call(
        &client,
        method::SESSIONS_LOCATE,
        json!({"external_id": FORK}),
    )
    .await;
    assert_eq!(located["session"], session(FORK));
    let missing = client
        .call(method::SESSIONS_READ, json!({"external_id": UNTYPED}), WAIT)
        .await;
    assert!(
        missing.is_err(),
        "a conversation no person typed into is not read"
    );

    // A tool-call turn, and the turn after the conversation resumed.
    let read = events(&client, RESUMED).await;
    assert_eq!(
        lines(&read),
        [
            "model",
            "turn_began",
            "words person",
            "words thought",
            "call_began shell",
            "call_ended",
            "words thought",
            "words agent",
            "turn_ended finished",
            "turn_began",
            "words person",
            "words thought",
            "call_began harness",
            "call_ended",
            "words thought",
            "words agent",
            "turn_ended finished",
            "tokens",
        ]
    );
    assert_eq!(read[0]["model"], "custom:Qwen3-local-0");
    assert_eq!(read[0]["provider"], "generic-chat-completion-api");
    let call_began = &read[4]["call"];
    assert_eq!(call_began["tool"], "Execute");
    assert_eq!(call_began["input"]["command"], "echo droid-ok");
    assert_eq!(read[5]["id"], call_began["id"]);
    assert_eq!(read[5]["ok"], true);
    assert!(read[5]["tail"].as_str().unwrap().starts_with("droid-ok\n"));
    assert_eq!(read[13]["ok"], false, "`Skill` named no skill");
    assert_eq!(read[17]["input"], 8188);
    assert_eq!(read[17]["output"], 1079);
    // Droid's system reminders and the folder it tells the model (`llm_only`) are no words
    // of the person.
    assert_eq!(
        person(&read),
        [
            "Call the Execute tool with parameters {\"command\": \"echo droid-ok\", \"riskLevel\": \"low\"}. Then reply with what it printed.",
            "Now say done in one word.",
        ]
    );

    // The fork reads as its own conversation: the history it copied, then its own turn.
    let fork = events(&client, FORK).await;
    let at = fork
        .iter()
        .position(|event| event["type"] == "fork")
        .unwrap();
    assert_eq!(fork[at]["parent"], RESUMED);
    assert_eq!(lines(&fork[..at]), lines(&read[..read.len() - 1]));
    assert_eq!(person(&fork[at..]), ["Say forked in one word."]);
    assert_eq!(fork.last().unwrap()["type"], "tokens");
    assert_eq!(
        events(&client, RESUMED).await,
        read,
        "the source is unchanged"
    );

    let failed = events(&client, FAILED).await;
    assert_eq!(
        lines(&failed),
        [
            "model",
            "turn_began",
            "words person",
            "turn_ended failed",
            "tokens"
        ]
    );
    assert_eq!(person(&failed), ["Say hi."]);
    let failure = &failed[3]["failure"];
    assert_eq!(failure["code"], "model_provider_unreachable");
    assert!(
        failure["line"]
            .as_str()
            .unwrap()
            .starts_with("Unable to reach http://localhost:1."),
        "{failure}"
    );

    // Killed as the model wrote: the file stood still with the turn open.
    let killed = call(
        &client,
        method::SESSIONS_READ,
        json!({"external_id": KILLED}),
    )
    .await;
    let killed_events = killed["events"].as_array().unwrap();
    assert_eq!(
        lines(killed_events),
        [
            "model",
            "turn_began",
            "words person",
            "turn_ended interrupted",
            "tokens"
        ]
    );
    let again = call(
        &client,
        method::SESSIONS_READ,
        json!({"external_id": KILLED, "cursor": killed["cursor"]}),
    )
    .await;
    assert_eq!(
        again["events"],
        json!([]),
        "the interrupted end is said once"
    );

    // A rejected permission ends the turn as its person stopped it.
    let hooked = events(&client, HOOKED).await;
    assert_eq!(person(&hooked).len(), 2);
    let mcp = hooked
        .iter()
        .find(|event| event["type"] == "call_began" && event["call"]["group"] == "mcp")
        .unwrap();
    assert_eq!(mcp["call"]["server"], "echo");
    assert_eq!(mcp["call"]["tool"], "echo___say");
    let ends: Vec<_> = lines(&hooked)
        .into_iter()
        .filter(|line| line.starts_with("turn_ended"))
        .collect();
    assert_eq!(ends, ["turn_ended finished", "turn_ended interrupted"]);

    // The hooks' documents, as Droid handed them in this conversation.
    let mut moments = Vec::new();
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "SessionEnd",
    ] {
        let document =
            std::fs::read_to_string(fixtures().join(format!("hooks/{event}.json"))).unwrap();
        let read = call(
            &client,
            method::HOOK_READ,
            json!({"event": event, "document": document, "env": {}}),
        )
        .await;
        let moment = read["moment"].clone();
        assert_eq!(moment["event"], event);
        assert_eq!(moment["external_id"], HOOKED);
        assert_eq!(moment["cwd"], "/private/tmp/droid-obs");
        moments.push(moment);
    }
    assert_eq!(
        moments[1]["prompt"],
        "Now call echo___say with words set to \"hello\"."
    );
    let call_ids: Vec<_> = hooked
        .iter()
        .filter(|event| event["type"] == "call_began")
        .map(|event| event["call"]["id"].clone())
        .collect();
    assert_eq!(moments[2]["call"]["tool"], "echo___say");
    assert_eq!(moments[2]["call"]["group"], "mcp");
    assert_eq!(
        moments[2]["call"]["id"], call_ids[1],
        "the store names the call"
    );
    assert_eq!(moments[3]["call"]["tool"], "ToolSearch");
    assert_eq!(moments[3]["call"]["id"], call_ids[0]);
    assert_eq!(moments[4]["continued"], false);
    assert_eq!(
        moments[5]["person_exited"], false,
        "`droid exec` ends `other`"
    );
    let unhooked = call(
        &client,
        method::HOOK_READ,
        json!({"event": "PreCompact", "document": "{\"session_id\":\"x\"}", "env": {}}),
    )
    .await;
    assert!(unhooked.get("moment").is_none(), "{unhooked}");

    // Steery's answers, printed as Droid reads them.
    let stop = std::fs::read_to_string(fixtures().join("hooks/Stop.json")).unwrap();
    let answered = call(
        &client,
        method::HOOK_ANSWER,
        json!({"event": "Stop", "document": stop, "env": {}, "answer": {"say": "go on"}}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(answered["stdout"].as_str().unwrap()).unwrap(),
        json!({"decision": "block", "reason": "go on"})
    );
    let pre = std::fs::read_to_string(fixtures().join("hooks/PreToolUse.json")).unwrap();
    let denied = call(
        &client,
        method::HOOK_ANSWER,
        json!({"event": "PreToolUse", "document": pre, "env": {},
               "answer": {"decision": {"deny": {"reason": "no"}}}}),
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(denied["stdout"].as_str().unwrap()).unwrap(),
        json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
               "permissionDecision": "deny", "permissionDecisionReason": "no"}})
    );
    assert_eq!(denied["exit_code"], 0);
    let start_doc = std::fs::read_to_string(fixtures().join("hooks/SessionStart.json")).unwrap();
    let led = call(
        &client,
        method::HOOK_ANSWER,
        json!({"event": "SessionStart", "document": start_doc, "env": {},
               "answer": {"lead": format!("droid:{HOOKED}")}}),
    )
    .await;
    let env_file: Value = serde_json::from_str(&start_doc).unwrap();
    assert_eq!(led["append"]["path"], env_file["CLAUDE_ENV_FILE"]);
    assert_eq!(
        led["append"]["text"],
        format!("export STEERY_LEAD='droid:{HOOKED}'\n")
    );

    // Setup with a `droid` 1.9.0 of this home's own.
    let plan = call(&client, method::SETUP_PLAN, setup()).await;
    assert_eq!(plan["found"]["release"], "1.9.0", "{plan}");
    assert!(plan.get("refused").is_none() && plan.get("install").is_none());
    let factory = home.join(".factory");
    let entries = plan["entries"].as_array().unwrap();
    let at = |at: &str| {
        entries
            .iter()
            .find(|entry| entry["at"] == at)
            .unwrap_or_else(|| panic!("no entry at {at}: {plan}"))
    };
    let mcp = at("/mcpServers/steery");
    assert_eq!(mcp["path"], json!(factory.join("mcp.json")));
    assert_eq!(mcp["value"]["type"], "http");
    assert_eq!(mcp["value"]["url"], "http://127.0.0.1:7777/mcp");
    let skill = entries
        .iter()
        .find(|entry| entry["path"] == json!(factory.join("skills/steery/SKILL.md")))
        .unwrap();
    assert_eq!(skill["value"], "# Steery");
    let hooks: Vec<_> = entries
        .iter()
        .filter(|entry| entry["path"] == json!(factory.join("hooks.json")))
        .collect();
    assert_eq!(hooks.len(), 7);
    let stop = at("/Stop/-");
    assert_eq!(
        stop["value"],
        json!({"hooks": [{"type": "command", "command": "/opt/steery/hook Stop || true",
                          "timeout": 10}]})
    );
    assert_eq!(at("/PreToolUse/-")["value"]["hooks"][0]["timeout"], 86_400);

    // The person's servers, with a project's `.factory/mcp.json` turning one off.
    let servers = call(
        &client,
        method::MCP_LIST,
        json!({"cwd": home.join("work/app/src")}),
    )
    .await;
    assert_eq!(servers["complete"], false);
    let names: Vec<_> = servers["servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|server| server["server"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["echo"], "{servers}");
    let watch = servers["watch"].as_array().unwrap();
    assert!(watch.contains(&json!(home.join("work/app/.factory/mcp.json"))));
    assert_eq!(watch.last().unwrap(), &json!(factory.join("mcp.json")));

    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn plans_an_install_or_an_update() {
    let missing = home("missing", None);
    let client = start(&missing).await;
    let plan = call(&client, method::SETUP_PLAN, setup()).await;
    assert!(plan.get("found").is_none(), "{plan}");
    assert_eq!(plan["entries"], json!([]));
    assert_eq!(
        plan["install"]["argv"],
        json!(["sh", "-c", "curl -fsSL https://app.factory.ai/cli | sh"])
    );
    let _ = std::fs::remove_dir_all(&missing);

    let old = home("old", Some("1.8.3"));
    let client = start(&old).await;
    let plan = call(&client, method::SETUP_PLAN, setup()).await;
    assert_eq!(plan["found"]["release"], "1.8.3");
    assert!(
        plan["refused"].as_str().unwrap().contains("droid update"),
        "{plan}"
    );
    assert_eq!(plan["entries"], json!([]));
    let _ = std::fs::remove_dir_all(&old);
}

/// Each line of a recorded turn of `fixtures/droid/turns/<name>`, read in order as one run
/// on `stream`.
async fn turn(client: &Client, name: &str, stream: &str) -> Vec<Value> {
    let text = std::fs::read_to_string(fixtures().join("turns").join(name)).unwrap();
    let mut read = Vec::new();
    for line in text.lines() {
        read.push(
            call(
                client,
                method::TURN_READ,
                json!({"turn": name, "stream": stream, "line": line}),
            )
            .await,
        );
    }
    read
}

/// Steery lists the models `droid exec --help` 1.9.0 printed, plans Droid's turns only
/// behind its own `PreToolUse` hook, and reads what `droid exec -o stream-json` wrote: a
/// turn whose call ran, one whose call Steery's hook denied, one Factory refused signed out,
/// and Droid's refusals of a model and a thinking level it does not run.
#[tokio::test]
async fn starts_and_reads_droid_turns() {
    let home = home("drive", None);
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let droid = bin.join("droid");
    let help = fixtures().join("turns/exec-help.txt");
    std::fs::write(
        &droid,
        format!(
            "#!/bin/sh\ncase \"$1\" in exec) cat '{}';; *) echo droid 1.9.0;; esac\n",
            help.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&droid, std::fs::Permissions::from_mode(0o755)).unwrap();
    let client = start(&home).await;
    let ask = |params: Value| {
        let client = &client;
        async move { client.call(method::START, params, WAIT).await }
    };
    let cwd = std::env::temp_dir();
    let new = json!({"act": "new", "cwd": cwd, "folder": cwd, "worktree": false});

    let models = call(&client, method::MODELS, json!({})).await;
    let listed = models["models"].as_array().unwrap();
    assert_eq!(listed[0]["model"], "gpt-6.1-sol", "{models}");
    assert_eq!(listed[0]["label"], "GPT-6.1 Sol");
    assert_eq!(
        listed[0]["thinking"],
        json!(["low", "medium", "high", "xhigh", "max"])
    );
    let custom = listed
        .iter()
        .find(|model| model["model"] == "custom:Qwen3-local-0")
        .unwrap();
    assert_eq!(custom["label"], "Qwen3 local");
    assert!(
        custom
            .get("thinking")
            .is_none_or(|levels| levels == &json!([]))
    );
    assert_eq!(
        models["watch"],
        json!([home.join(".factory/settings.json")])
    );

    // With no Steery hook before each call, Droid would run every call unasked.
    let ungated = ask(new.clone()).await.unwrap_err();
    assert!(
        matches!(&ungated, CallError::Refused { word, line: Some(line) }
            if word == "refused" && line.contains("PreToolUse")),
        "{ungated:?}"
    );
    let plan = call(&client, method::SETUP_PLAN, setup()).await;
    let gate = plan["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["at"] == "/PreToolUse/-")
        .unwrap();
    std::fs::write(
        home.join(".factory/hooks.json"),
        json!({"PreToolUse": [gate["value"]]}).to_string(),
    )
    .unwrap();

    let launched = ask(new.clone()).await.unwrap();
    assert_eq!(
        launched["argv"],
        json!([
            droid,
            "exec",
            "-o",
            "stream-json",
            "--skip-permissions-unsafe",
            "--cwd",
            cwd
        ])
    );
    assert_eq!(launched["stdin"], "words");
    let resumed = ask(
        json!({"act": "continue", "external_id": RESUMED, "cwd": cwd,
        "folder": cwd, "worktree": false, "model": "gpt-6.1-sol", "thinking": "high"}),
    )
    .await
    .unwrap();
    assert_eq!(
        resumed["argv"],
        json!([
            droid,
            "exec",
            "-o",
            "stream-json",
            "--skip-permissions-unsafe",
            "-s",
            RESUMED,
            "-m",
            "gpt-6.1-sol",
            "-r",
            "high",
            "--cwd",
            cwd
        ])
    );
    let forked = ask(json!({"act": "fork", "external_id": RESUMED, "cwd": cwd,
        "folder": cwd, "worktree": false}))
    .await
    .unwrap();
    assert_eq!(forked["argv"][5], "--fork");
    // `-s` of an id Droid does not keep opens a new conversation under it.
    for (params, word) in [
        (
            json!({"act": "continue", "external_id": "00000000-0000-4000-8000-00000000abcd",
                   "cwd": cwd, "folder": cwd, "worktree": false}),
            "not-found",
        ),
        (
            json!({"act": "continue", "external_id": RESUMED, "cwd": "/private/tmp/gone-droid",
                   "folder": cwd, "worktree": false}),
            "no-place",
        ),
        (
            json!({"act": "new", "cwd": cwd, "folder": cwd, "worktree": false,
                   "model": "nosuch-model"}),
            "model-missing",
        ),
        (
            json!({"act": "new", "cwd": cwd, "folder": cwd, "worktree": false,
                   "model": "custom:Qwen3-local-0", "thinking": "high"}),
            "selection-unsupported",
        ),
    ] {
        let error = ask(params).await.unwrap_err().to_string();
        assert!(error.contains(word), "{word}: {error}");
    }
    let terminal = call(
        &client,
        method::TERMINAL,
        json!({"external_id": RESUMED, "cwd": cwd}),
    )
    .await;
    assert_eq!(
        terminal["command"],
        format!("cd '/private/tmp/droid-obs' && droid --resume '{RESUMED}'")
    );

    let ran = turn(&client, "unsafe.stdout.jsonl", "stdout").await;
    assert_eq!(ran[0]["named"], "479fce20-98db-4a1c-bca1-c65c9ced789f");
    assert_eq!(ran[0]["took"], false);
    assert_eq!(ran[0]["events"][0]["model"], "custom:Qwen3-local-0");
    assert_eq!(ran[1]["took"], true);
    assert_eq!(
        ran[1]["events"][0]["turn"],
        "aebc626b-5a1d-4091-8c13-1b263d1f4f78"
    );
    let events: Vec<&Value> = ran
        .iter()
        .flat_map(|line| line["events"].as_array())
        .flatten()
        .collect();
    let began = events
        .iter()
        .find(|event| event["type"] == "call_began")
        .unwrap();
    assert_eq!(began["call"]["id"], "call_7868be9a5e4bf94b_0");
    assert_eq!(began["call"]["input"]["command"], "echo drive-check");
    let ended = events
        .iter()
        .find(|event| event["type"] == "call_ended")
        .unwrap();
    assert_eq!(ended["ok"], true);
    let ends: Vec<&Value> = ran.iter().filter_map(|line| line.get("ended")).collect();
    assert_eq!(ends, [&json!({"state": "finished"})]);

    let denied = turn(&client, "denied-unsafe.stdout.jsonl", "stdout").await;
    let ended = denied
        .iter()
        .flat_map(|line| line["events"].as_array())
        .flatten()
        .find(|event| event["type"] == "call_ended")
        .unwrap();
    assert_eq!(ended["ok"], false);
    assert_eq!(ended["tail"], "Error: Steery denied it");
    assert_eq!(
        denied.last().unwrap()["ended"],
        json!({"state": "finished"})
    );

    let signed_out = turn(&client, "signedout.stdout.jsonl", "stdout").await;
    assert_eq!(signed_out[1]["refused"], "refused");
    assert_eq!(signed_out[1]["ended"]["state"], "failed");
    assert!(
        signed_out[1]["ended"]["error"]
            .as_str()
            .unwrap()
            .contains("Authentication failed")
    );
    let model = turn(&client, "badmodel.stderr", "stderr").await;
    assert_eq!(model[0]["refused"], "model-missing");
    let effort = turn(&client, "badeffort.stderr", "stderr").await;
    assert_eq!(effort[0]["refused"], "selection-unsupported");
    let _ = std::fs::remove_dir_all(&home);
}
