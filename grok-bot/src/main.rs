mod native;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use native::{Native, marker, now, person, refused, reply, running, string, timestamp};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use steery_sdk::wire::{self, harness as h};
use steery_sdk::{Error, PROTOCOL, Program, method, params, refusal, result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Driver {
    native: OnceLock<Native>,
    sends: tokio::sync::Mutex<()>,
    notifier: steery_sdk::Notifier,
}

fn default_cli() -> Result<PathBuf, Error> {
    let executable = std::env::current_exe().map_err(|error| refused(error.to_string()))?;
    Ok(executable.with_file_name("grokbot-gateway.mjs"))
}

fn cursor(entries: &[Value]) -> Result<String, Error> {
    let bytes = serde_json::to_vec(entries).map_err(|error| refused(error.to_string()))?;
    Ok(format!("{}:{:x}", entries.len(), Sha256::digest(bytes)))
}

fn after(entries: &[Value], mark: &str) -> Result<Option<usize>, Error> {
    let (count, _) = mark.split_once(':').ok_or_else(|| Error::invalid("invalid transcript cursor"))?;
    let count: usize = count.parse().map_err(|_| Error::invalid("invalid transcript cursor count"))?;
    if count <= entries.len() && cursor(&entries[..count])? == mark { Ok(Some(count)) } else { Ok(None) }
}

fn session(_native: &Native, agent: &Value) -> Result<h::Session, Error> {
    let id = string(agent, "id")?;
    Ok(h::Session {
        external_id: id.into(), cwd: None,
        title: agent.get("name").and_then(Value::as_str).map(str::to_owned),
        created_at: Some(timestamp(agent, "createdAt")?), updated_at: timestamp(agent, "updatedAt")?,
        transcripts: Vec::new(), parent: None, app: true, sub_agents: Vec::new(),
    })
}

impl Driver {
    fn native(&self) -> Result<&Native, Error> {
        self.native.get().ok_or_else(|| Error::invalid("initialize must come first"))
    }

    async fn read(&self, read: h::Read) -> Result<h::Events, Error> {
        if read.sub_agent.is_some() { return Err(refused("Grok Bot does not expose sub-agent transcripts")); }
        let entries = self.native()?.transcript(&read.external_id).await?;
        let mut reset = false;
        let mut start = 0;
        if let Some(mark) = &read.cursor {
            if let Some(index) = after(&entries, mark)? { start = index; } else { reset = true; }
        } else if let Some(bound) = read.tail {
            let mut bytes = 0u64;
            start = entries.len();
            for entry in entries.iter().rev() {
                let size = serde_json::to_vec(entry).map_err(|error| refused(error.to_string()))?.len() as u64 + 1;
                if bytes.saturating_add(size) > bound { break; }
                bytes += size;
                start -= 1;
            }
        }
        let mut events = Vec::new();
        for (index, entry) in entries.iter().enumerate().skip(start) {
            let words = person(entry).map(|text| (h::Voice::Person, text))
                .or_else(|| reply(entry).map(|text| (h::Voice::Agent, text)));
            if let Some((voice, text)) = words {
                let seq = entry.get("seq").and_then(Value::as_u64)
                    .ok_or_else(|| refused(format!("native message lacks integer seq: {entry}")))?;
                events.push(h::Event::Words { at: timestamp(entry, "timestampMs")?,
                    id: format!("{}:{seq}", read.external_id), voice, text: text.into(),
                    call: None, clips: Vec::new() });
            } else if entry.get("kind").and_then(Value::as_str) == Some("message")
                && entry.get("role").and_then(Value::as_str) == Some("user") {
                return Err(refused(format!("unsupported native user content at record {index}: {entry}")));
            } else if entry.get("kind").and_then(Value::as_str) == Some("send-message") {
                return Err(refused(format!("unsupported native assistant content at record {index}: {entry}")));
            }
        }
        Ok(h::Events { cursor: cursor(&entries)?, events, more: false, reset, whole: start == 0 })
    }
}

impl Program for Driver {
    fn initialize(&self, input: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !input.protocol.contains(&PROTOCOL) { return Err(Error::invalid("this driver speaks protocol 1 only")); }
        for key in input.settings.keys() {
            if !matches!(key.as_str(), "cli" | "GROK_ACCESS_TOKEN" | "GROK_CLIENT_VERSION") { return Err(Error::invalid(format!("unknown setting {key}"))); }
        }
        let cli = input.settings.get("cli").map(PathBuf::from).map(Ok).unwrap_or_else(default_cli)?;
        let mut env = BTreeMap::new();
        for key in ["GROK_ACCESS_TOKEN", "GROK_CLIENT_VERSION"] {
            if let Some(value) = input.settings.get(key) { env.insert(key.into(), value.clone()); }
        }
        self.native.set(Native { cli, cache: input.config.join("grok-bot/transcripts"), env })
            .map_err(|_| Error::invalid("driver already initialized"))?;
        let native = self.native()?.clone();
        let notifier = self.notifier.clone();
        tokio::spawn(async move {
            let mut previous = BTreeMap::<String, h::LiveState>::new();
            loop {
                match native.agents().await {
                    Ok(agents) => {
                        for agent in agents {
                            let observed = (|| -> Result<(String, h::LiveState), Error> {
                                let id = string(&agent, "id")?.to_owned();
                                let active = running(&agent)?;
                                let mut state = h::LiveState::default();
                                state.idle = Some(!active);
                                Ok((id, state))
                            })();
                            match observed {
                                Ok((id, state)) => {
                                    if previous.get(&id) != Some(&state) {
                                        match result(h::SessionChanged { external_id: id.clone(), state: state.clone() }) {
                                            Ok(value) => notifier.notify(method::SESSION_CHANGED, value),
                                            Err(error) => eprintln!("live notification: {error}"),
                                        }
                                        previous.insert(id, state);
                                    }
                                }
                                Err(error) => eprintln!("native live schema: {error}"),
                            }
                        }
                    }
                    Err(error) => eprintln!("native live polling: {error}"),
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo { id: "grok-bot".parse().map_err(Error::invalid)?, kind: wire::Kind::Harness,
                release: env!("CARGO_PKG_VERSION").into(), label: "Grok Bot".into(), short: Some("Grok Bot".into()) },
            harness: Some(Box::new(wire::HarnessInfo {
                levels: vec![wire::Level::Observe, wire::Level::Drive, wire::Level::Live],
                minimum: None, sign_in: Some("Sign in to the Grok Bot app".into()),
                knowing: "The shared connector does not identify this bot. Use your own native bot ID as session.external_id with session.harness=grok-bot when calling Steery. If your context does not expose your own ID, do not describe a session or guess from the roster.".into(),
                transcript: None,
                identity: wire::Identity { executables: vec!["steery-driver-grok-bot".into(), "gbot".into()],
                    package: None, session_variable: None },
                setup: wire::SetupFacts { reads_skills: false, cuts_tool_text: false, call_bound: None },
                reader: None,
                facts: wire::HarnessFacts { by_file: false, resumes_in_place: false, forks_beside: false,
                    keeps: wire::Keeps { model: false, thinking: false }, door: Some(h::LiveDoor::Keeps),
                    checks_idle: false, worktrees: false, acts: vec![h::Act::New, h::Act::Continue, h::Act::Hand] },
                hooks: Vec::new(),
            })),
            machine: None,
            settings: vec![wire::Setting { name: "cli".into(), secret: false, required: false,
                about: "Gateway wrapper path; defaults to colocated grokbot-gateway.mjs".into() },
                wire::Setting { name: "GROK_ACCESS_TOKEN".into(), secret: true, required: true,
                    about: "Signed-in Grok Bot account access token".into() },
                wire::Setting { name: "GROK_CLIENT_VERSION".into(), secret: false, required: true,
                    about: "Installed Grok Bot client version".into() }],
        })
    }

    async fn call(&self, name: &str, input: Value) -> Result<Value, Error> {
        let native = self.native()?;
        match name {
            method::SETUP_PLAN => {
                let setup: h::Setup = params(input)?;
                let status = native.api("getHostStatus", json!({})).await?;
                let state = native.api("getMcpState", json!({})).await?;
                let servers = state.get("servers").and_then(Value::as_array)
                    .ok_or_else(|| refused("getMcpState lacks servers"))?;
                let connected = servers.iter().any(|server|
                    server.get("url").and_then(Value::as_str) == Some(setup.mcp_url.as_str())
                    && server.get("isTeamServer").and_then(Value::as_bool) == Some(false)
                    && server.get("status").and_then(Value::as_str) == Some("connected"));
                let note = format!("Connect {} in your own Grok Bot account once; do not change a shared team connector", setup.mcp_url);
                result(h::Planned { found: Some(h::Found { binary: native.cli.clone(),
                    release: string(&status, "hostVersion")?.into(),
                    root: PathBuf::from(std::env::var("HOME").map_err(|_| refused("HOME is required to locate Grok Bot configuration"))?)
                        .join("Library/Application Support/Grok Bot") }),
                    refused: (!connected).then(|| note.clone()),
                    entries: Vec::new(), notes: vec![note], install: None })
            }
            method::SESSIONS_LIST => {
                let list: h::List = params(input)?;
                let mut sessions = Vec::new();
                for agent in native.agents().await? {
                    let found = session(native, &agent)?;
                    if list.seen_since.is_some_and(|since| found.updated_at < since) { continue; }
                    sessions.push(found);
                }
                result(h::Listed { sessions, excluded: Vec::new(), complete: list.seen_since.is_none() })
            }
            method::SESSIONS_LOCATE => {
                let locate: h::Locate = params(input)?;
                let agent = native.agent(&locate.external_id).await?;
                result(h::Located { session: session(native, &agent)? })
            }
            method::SESSIONS_READ => result(self.read(params(input)?).await?),
            method::HELD => {
                let held: h::Held = params(input)?;
                let agent = native.agent(&held.external_id).await?;
                let holder = if running(&agent)? {
                    h::Holder::Person { door: Some(h::Door::Keeps),
                        stop: h::HolderStop::Beyond("The Grok Bot gateway holds this remote turn; no local process or transcript owns it".into()) }
                } else {
                    h::Holder::Nobody { proof: format!("listAgents reports isRunningTurn=false and isComposingMessage=false for {}", held.external_id) }
                };
                result(h::HeldBy::Holder(holder))
            }
            method::IDLE => {
                let held: h::Held = params(input)?;
                let agent = native.agent(&held.external_id).await?;
                let active = running(&agent)?;
                let entries = native.transcript(&held.external_id).await?;
                let state = if active { Some(h::Execution::Running) }
                    else if entries.last().and_then(reply).is_some() { Some(h::Execution::Finished) }
                    else { None };
                result(h::Sampled { sample: state.map(|state| h::Sample { state, observed_at: now(),
                    started_at: None, ended_at: None, model: None, thinking: None, error: None,
                    vacant: false, if_unheld: false }) })
            }
            method::MODELS => {
                let _: wire::Empty = params(input)?;
                result(h::Models { models: Vec::new(), unlisted: None, watch: Vec::new() })
            }
            method::START => {
                let start: h::Start = params(input)?;
                if start.model.is_some() || start.thinking.is_some() {
                    return Err(Error::refused(refusal::SELECTION_UNSUPPORTED, "Grok Bot keeps its native model and thinking"));
                }
                if start.worktree || !start.reads.is_empty() { return Err(refused("Grok Bot does not expose native worktrees or additional read folders")); }
                match start.act {
                    h::Act::Fork => return Err(refused("Grok Bot does not expose a native history fork")),
                    h::Act::New => {
                        if start.external_id.is_some() { return Err(Error::invalid("new must not name an existing bot")); }
                    }
                    h::Act::Continue | h::Act::Hand => {
                        let id = start.external_id.as_deref().ok_or_else(|| Error::invalid("continue or hand requires external_id"))?;
                        if running(&native.agent(id).await?)? { return Err(Error::refused(refusal::HARNESS_BUSY, "Grok Bot is running a turn")); }
                    }
                }
                let executable = std::env::current_exe().map_err(|error| refused(error.to_string()))?;
                result(h::Launch { argv: vec![executable.to_string_lossy().into_owned(), "turn".into(),
                    native.cli.to_string_lossy().into_owned(), native.cache.to_string_lossy().into_owned(),
                    start.external_id.unwrap_or_default()], env: native.env.clone(), cwd: start.cwd,
                    stdin: h::Stdin::Words, inbox: false })
            }
            method::TURN_READ => {
                let read: h::TurnRead = params(input)?;
                if read.stream == h::Stream::Stderr { return Ok(json!({})); }
                let line: Value = serde_json::from_str(&read.line).map_err(|error| Error::invalid(format!("turn JSON: {error}")))?;
                let mut event: h::TurnLine = params(line.get("event").cloned().ok_or_else(|| Error::invalid("turn line lacks event"))?)?;
                if event.took == Some(true) {
                    let text = string(&line, "input")?;
                    if read.delivery.as_deref().is_some_and(|delivery| !marker(text, delivery)) { event.took = None; }
                }
                result(event)
            }
            method::STOP => {
                let stop: h::Stop = params(input)?;
                match stop.holder {
                    h::Holder::Nobody { .. } => result(h::Stopped { stopped: false, proof: None }),
                    _ => Err(refused("The remote gateway holder is beyond the runtime; stop it in Grok Bot")),
                }
            }
            method::RECEIPT_MARK => {
                let mark: h::ReceiptMark = params(input)?;
                result(h::Marked { mark: cursor(&native.transcript(&mark.external_id).await?)? })
            }
            method::RECEIPT_FIND => {
                let find: h::ReceiptFind = params(input)?;
                let entries = native.transcript(&find.external_id).await?;
                let index = after(&entries, &find.mark)?.ok_or_else(|| refused("native transcript changed after receipt mark"))?;
                let found = entries[index..].iter().any(|entry| person(entry).is_some_and(|text| marker(text, &find.delivery)));
                result(h::Receipt { found, owner: found.then_some(find.external_id) })
            }
            method::SAY => {
                let say: h::Say = params(input)?;
                if say.mode != h::SayMode::Immediate { return Err(refused("native gateway exposes immediate send only, not steer or queue")); }
                let _guard = self.sends.lock().await;
                let before = native.send(&say.external_id, &say.words).await?;
                native.prove_input(&say.external_id, &say.words, before).await?;
                result(h::Said { took: true })
            }
            method::PICK => {
                let pick: h::Pick = params(input)?;
                native.agent(&pick.external_id).await?;
                if pick.model.is_some() || pick.thinking.is_some() {
                    return Err(Error::refused(refusal::SELECTION_UNSUPPORTED, "Grok Bot keeps its native model and thinking"));
                }
                result(h::Picked::default())
            }
            method::RECAP => {
                let recap: h::Recap = params(input)?;
                let _guard = self.sends.lock().await;
                let before = native.send(&recap.external_id, &recap.asked).await?;
                let index = native.prove_input(&recap.external_id, &recap.asked, before).await?;
                result(h::Text { text: native.finish(&recap.external_id, index).await? })
            }
            _ => Err(Error::Unknown),
        }
    }
}

async fn emit(event: h::TurnLine, input: Option<&str>) -> Result<(), Error> {
    let mut value = json!({"event":event});
    if let Some(input) = input { value["input"] = json!(input); }
    let mut bytes = serde_json::to_vec(&value).map_err(|error| refused(error.to_string()))?;
    bytes.push(b'\n');
    let mut stdout = tokio::io::stdout();
    stdout.write_all(&bytes).await.map_err(|error| refused(error.to_string()))?;
    stdout.flush().await.map_err(|error| refused(error.to_string()))
}

async fn turn(args: &[String]) -> Result<(), Error> {
    if args.len() != 3 { return Err(Error::invalid("turn requires CLI path, cache path, and bot ID (empty for new)")); }
    let native = Native { cli: PathBuf::from(&args[0]), cache: PathBuf::from(&args[1]),
        env: ["GROK_ACCESS_TOKEN", "GROK_CLIENT_VERSION"].into_iter()
            .filter_map(|key| std::env::var(key).ok().map(|value| (key.into(), value))).collect() };
    let mut words = String::new();
    tokio::io::stdin().read_to_string(&mut words).await.map_err(|error| refused(error.to_string()))?;
    if words.is_empty() { return Err(Error::invalid("turn input is empty")); }
    let id = if args[2].is_empty() {
        native.create(&std::env::current_dir().map_err(|error| refused(error.to_string()))?).await?
    } else { args[2].clone() };
    emit(h::TurnLine { named: Some(id.clone()), ..Default::default() }, None).await?;
    let before = native.send(&id, &words).await?;
    let index = native.prove_input(&id, &words, before).await?;
    emit(h::TurnLine { took: Some(true), ..Default::default() }, Some(&words)).await?;
    native.finish(&id, index).await?;
    emit(h::TurnLine { ended: Some(h::Ended { state: h::TurnState::Finished, error: None }),
        ..Default::default() }, None).await
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "turn") {
        if let Err(error) = turn(&args[1..]).await {
            let message = match &error { Error::Refused { line: Some(line), .. } => line.clone(), _ => error.to_string() };
            let _ = emit(h::TurnLine { ended: Some(h::Ended { state: h::TurnState::Failed,
                error: Some(message.clone()) }), ..Default::default() }, None).await;
            eprintln!("{message}");
            std::process::exit(1);
        }
    } else if args.is_empty() {
        if let Err(error) = steery_sdk::serve(|notifier| Driver { native: OnceLock::new(), sends: tokio::sync::Mutex::new(()), notifier }).await {
            eprintln!("{error}");
            std::process::exit(1);
        }
    } else {
        eprintln!("usage: steery-driver-grok-bot [turn CLI CACHE BOT_ID]");
        std::process::exit(2);
    }
}
