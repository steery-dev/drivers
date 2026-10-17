//! Codex at drive and live: who holds a thread, what can be proved of it now, the launch
//! plan of a turn and the lines it writes, stopping a native owner's turn, receipts, the
//! terminal command, models, the one-shot reader, and words and picks for a thread a
//! native owner holds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use steery_driver_common::receipt::{DELIVERED, marked};
use steery_sdk::wire::harness::{
    Act, Beyond, Door, Ended, Event, Execution, Held, HeldBy, Holder, HolderStop, Launch, Look,
    Looked, Marked, Model, Models, Pick, Picked, ReadAs, Reader, ReaderPlan, ReaderRead,
    ReaderTools, Receipt, ReceiptFind, ReceiptMark, Said, Sample, Sampled, Say, SayMode, Start,
    Stdin, Stop, Stopped, Stream, Terminal, TerminalCommand, Text, TurnLine, TurnRead, TurnState,
};
use steery_sdk::{Error, refusal};

use crate::appserver::AppServer;
use crate::native::{self, Owner};
use crate::rollout::Store;
use crate::router::{self, Followers, Took};

/// What a thread an app server has loaded with no window owning it says: the app is never
/// quit for one thread, since it serves every other it has (decided 2026-10-06).
const APP_LOADED: &str = "app has it loaded with no window: open it in a Codex window, or Fork";
/// How much of a rollout's end is read for where its conversation stands.
const LATEST: u64 = 256 * 1024;
/// The stop's graceful step must be done within the runtime's bound for `stop`.
const STOP_WITHIN: std::time::Duration = std::time::Duration::from_millis(2500);

/// What drive needs: Codex's home and conversations, its binary, and the followers.
pub struct Drive {
    pub home: PathBuf,
    pub store: Store,
    pub followers: Arc<Followers>,
}

fn binary() -> Result<PathBuf, Error> {
    crate::home::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "codex is not installed here"))
}

fn uuid(id: &str) -> Result<(), Error> {
    uuid::Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| Error::refused(refusal::NOT_FOUND, format!("{id} is no Codex thread")))
}

/// The native owner of a thread now, and the turn it runs.
enum Owned {
    /// The window of the Codex app that owns it.
    Window(Option<String>),
    /// A running app server of the thread's home, whose queue holds it.
    Queue(Option<String>),
}

impl Drive {
    /// The home a thread's rollout is filed under: `<home>/sessions/<yyyy>/<mm>/<dd>/`.
    fn thread_home(&self, thread: &str) -> Result<PathBuf, Error> {
        let file = self.store.current(thread)?;
        let sessions = file
            .ancestors()
            .nth(4)
            .filter(|dir| dir.file_name().is_some_and(|n| n == "sessions"));
        Ok(sessions
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.store.root().to_owned()))
    }

    /// Who owns `thread` natively now, asked of the app's router and of the running app
    /// server of its home; with the turn each runs, where it says.
    async fn owned(&self, thread: &str) -> Option<Owned> {
        let home = self.thread_home(thread).ok()?;
        let binary = crate::home::binary()?;
        let queued = tokio::time::timeout(native::WITHIN, async {
            let mut server = native::queue(&binary, &home, thread).await.ok()?;
            let latest = native::latest(&mut server, thread).await.ok();
            server.close().await;
            Some(
                latest
                    .flatten()
                    .filter(native::Turn::running)
                    .map(|turn| turn.id),
            )
        })
        .await
        .ok()
        .flatten();
        if let Some(turn) = queued {
            return Some(Owned::Queue(turn));
        }
        let router = self.followers.router()?.to_owned();
        let window = match self.followers.execution(thread) {
            Some(execution) => Some(execution),
            None if router::owns(&router, thread).await => None,
            None => return None,
        };
        if window.is_some_and(|execution| execution != Execution::Running) {
            return Some(Owned::Window(None));
        }
        let turn = tokio::time::timeout(native::WITHIN, async {
            let mut server = native::reader(&binary, &home).await.ok()?;
            let latest = native::latest(&mut server, thread).await.ok().flatten();
            server.close().await;
            latest
        })
        .await
        .ok()
        .flatten();
        Some(Owned::Window(
            turn.filter(native::Turn::running).map(|turn| turn.id),
        ))
    }

    /// `held`: a native owner, through which the thread takes words and applies a pick
    /// and which stops its turn itself; else where the runtime looks, an app server that
    /// has it loaded being beyond reach.
    pub async fn held(&self, held: Held) -> Result<HeldBy, Error> {
        let thread = &held.external_id;
        let transcripts = self.store.transcripts(thread).unwrap_or_default();
        if !transcripts.is_empty() {
            let home = self.thread_home(thread)?;
            match self.owned(thread).await {
                Some(Owned::Window(turn)) => {
                    return Ok(HeldBy::Holder(Holder::Person {
                        door: Some(Door::Picks),
                        stop: HolderStop::Owner(turn.map(|turn| {
                            Owner::App {
                                thread: thread.clone(),
                                turn,
                            }
                            .token()
                        })),
                    }));
                }
                Some(Owned::Queue(turn)) => {
                    return Ok(HeldBy::Holder(Holder::Person {
                        door: Some(Door::Picks),
                        stop: HolderStop::Owner(turn.map(|turn| {
                            Owner::Queue {
                                home,
                                thread: thread.clone(),
                                turn,
                            }
                            .token()
                        })),
                    }));
                }
                None => {}
            }
        }
        Ok(HeldBy::Look(Looked {
            look: Look {
                transcripts,
                strays: false,
                beyond: vec![Beyond {
                    arg: Some("app-server".into()),
                    line: APP_LOADED.into(),
                }],
            },
        }))
    }

    /// `idle`: the owning window's or app server's own state of the thread; else where
    /// its rollout says it stands, which holds only while nothing holds the rollout.
    pub async fn idle(&self, held: Held) -> Result<Sampled, Error> {
        let thread = &held.external_id;
        let now = crate::home::now();
        if let Some(execution) = self.followers.execution(thread) {
            return Ok(Sampled {
                sample: Some(sample(execution, now, false, false)),
            });
        }
        if let (Ok(home), Some(binary)) = (self.thread_home(thread), crate::home::binary()) {
            let queued = tokio::time::timeout(native::WITHIN, async {
                let mut server = native::queue(&binary, &home, thread).await.ok()?;
                let empty = native::queue_empty(&mut server, thread).await.ok();
                let read = server
                    .call(
                        "thread/read",
                        json!({"threadId": thread, "includeTurns": false}),
                    )
                    .await
                    .ok();
                server.close().await;
                let running = match read?.pointer("/thread/status/type").and_then(Value::as_str) {
                    Some("active") => true,
                    Some("idle") => empty != Some(true),
                    _ => return None,
                };
                Some(running)
            })
            .await
            .ok()
            .flatten();
            if let Some(running) = queued {
                let execution = if running {
                    Execution::Running
                } else {
                    Execution::Finished
                };
                return Ok(Sampled {
                    sample: Some(sample(execution, now, false, false)),
                });
            }
        }
        // What the rollout says is kept while it stands; when it was looked at is now.
        let Ok(said) = self
            .store
            .latest(thread, LATEST, |events| stands(events, now))
        else {
            return Ok(Sampled { sample: None });
        };
        Ok(Sampled {
            sample: said.map(|sample| Sample {
                observed_at: now,
                ..sample
            }),
        })
    }

    /// `start`: the launch plan of a turn, which runs this binary in its turn mode
    /// speaking to `codex app-server`.
    pub fn start(&self, start: Start) -> Result<Launch, Error> {
        let codex = binary()?;
        let act = match start.act {
            Act::Continue => "continue",
            Act::Fork => "fork",
            Act::New => "new",
            Act::Hand => {
                return Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    "Codex has no hand-over",
                ));
            }
        };
        if start.act != Act::New {
            let id = start.external_id.as_deref().ok_or_else(|| {
                Error::invalid(format!("a {act} names the conversation it acts on"))
            })?;
            uuid(id)?;
            self.store.current(id)?;
        }
        if !start.cwd.is_absolute() || !start.folder.is_absolute() {
            return Err(Error::invalid("the folders are absolute paths"));
        }
        let program = std::env::current_exe().map_err(|e| {
            Error::refused(
                refusal::START_FAILED,
                format!("this driver's own path: {e}"),
            )
        })?;
        let asked = crate::turn::Asked {
            codex,
            act: act.to_owned(),
            id: (start.act != Act::New)
                .then(|| start.external_id.clone())
                .flatten(),
            worktree: start.worktree && start.act != Act::Continue,
            model: start.model,
            thinking: start.thinking,
            inbox: Some(start.folder.join("inbox")),
        };
        let mut argv = vec![program.to_string_lossy().into_owned(), "turn".to_owned()];
        argv.extend(asked.args());
        Ok(Launch {
            argv,
            env: BTreeMap::new(),
            cwd: start.cwd,
            stdin: Stdin::Words,
            inbox: true,
        })
    }

    /// `stop`: the owner's own way to end the turn it runs, seen to end. A terminal
    /// Codex has no such way: the runtime's signals are its stop.
    pub async fn stop(&self, stop: Stop) -> Result<Stopped, Error> {
        let not_stopped = |line: String| Error::refused(refusal::NOT_STOPPED, line);
        let token = match &stop.holder {
            Holder::Person {
                stop: HolderStop::Owner(Some(token)),
                ..
            } => token,
            _ => {
                return Ok(Stopped {
                    stopped: false,
                    proof: None,
                });
            }
        };
        let owner = Owner::parse(token)
            .ok_or_else(|| Error::invalid("an owner token this driver did not give"))?;
        let codex = binary()?;
        let stopping = async {
            match &owner {
                Owner::App { thread, turn } => {
                    let home = self
                        .thread_home(thread)
                        .map_err(|_| "the thread is gone".to_owned())?;
                    let mut server = native::reader(&codex, &home).await?;
                    let found = native::find(&mut server, thread, turn)
                        .await?
                        .ok_or("the turn is gone from Codex's history")?;
                    if !found.ended() {
                        let router = self
                            .followers
                            .router()
                            .ok_or("the Codex app's router is gone")?
                            .to_owned();
                        let mut app = router::App::open(&router)
                            .await
                            .map_err(|e| e.to_string())?;
                        app.interrupt(thread, turn).await?;
                    }
                    let after = native::find(&mut server, thread, turn)
                        .await?
                        .ok_or("the turn is gone")?;
                    server.close().await;
                    Ok::<_, String>((thread.clone(), turn.clone(), after))
                }
                Owner::Queue { home, thread, turn } => {
                    let mut server = native::queue(&codex, home, thread).await?;
                    let found = native::find(&mut server, thread, turn)
                        .await?
                        .ok_or("the turn is gone from Codex's history")?;
                    if !found.ended() {
                        server
                            .call(
                                "turn/interrupt",
                                json!({"threadId": thread, "turnId": turn}),
                            )
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                    let after = native::find(&mut server, thread, turn)
                        .await?
                        .ok_or("the turn is gone")?;
                    server.close().await;
                    Ok((thread.clone(), turn.clone(), after))
                }
            }
        };
        let (thread, turn, after) = tokio::time::timeout(STOP_WITHIN, stopping)
            .await
            .map_err(|_| not_stopped("Codex did not interrupt its turn in time".into()))?
            .map_err(not_stopped)?;
        if !after.ended() {
            return Err(not_stopped(format!(
                "Codex turn {turn} is still {}",
                after.status
            )));
        }
        Ok(Stopped {
            stopped: true,
            proof: Some(format!(
                "Codex thread {thread} turn {turn} is {}",
                after.status
            )),
        })
    }

    /// `receipt/mark`: the newest turn of the thread before words go, and the owner whose
    /// history it is.
    pub async fn mark(&self, mark: ReceiptMark) -> Result<Marked, Error> {
        let thread = &mark.external_id;
        uuid(thread)?;
        let home = self.thread_home(thread)?;
        let codex = binary()?;
        let marked = tokio::time::timeout(native::WITHIN, async {
            let (mut server, queue) = match native::queue(&codex, &home, thread).await {
                Ok(server) => (server, true),
                Err(_) => (native::reader(&codex, &home).await?, false),
            };
            let latest = native::latest(&mut server, thread).await;
            server.close().await;
            Ok::<_, String>((latest?.map(|turn| turn.id), queue))
        })
        .await
        .map_err(|_| {
            Error::refused(
                refusal::NO_ANSWER,
                "Codex did not list the thread's turns in time",
            )
        })?
        .map_err(|line| Error::refused(refusal::REFUSED, line))?;
        Ok(Marked {
            mark: json!({"home": home, "before": marked.0, "queue": marked.1}).to_string(),
        })
    }

    /// `receipt/find`: the one turn after the mark whose person input carries the delivery
    /// marker, and the owner that ran it.
    pub async fn find(&self, find: ReceiptFind) -> Result<Receipt, Error> {
        let thread = &find.external_id;
        let mark: Value = serde_json::from_str(&find.mark)
            .map_err(|_| Error::invalid("a mark this driver did not give"))?;
        let home = mark
            .get("home")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| Error::invalid("a mark this driver did not give"))?;
        let before = mark
            .get("before")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let queue = mark.get("queue").and_then(Value::as_bool) == Some(true);
        let marker = format!("{DELIVERED}{}", find.delivery);
        let codex = binary()?;
        let found = tokio::time::timeout(std::time::Duration::from_secs(18), async {
            // The owner may have gone after the words went: a read of the same home proves
            // the history as well.
            let (mut server, queued) = match queue {
                true => match native::queue(&codex, &home, thread).await {
                    Ok(server) => (server, true),
                    Err(_) => (native::reader(&codex, &home).await?, false),
                },
                false => (native::reader(&codex, &home).await?, false),
            };
            let found = native::carrying(&mut server, thread, before.as_deref(), &marker).await;
            server.close().await;
            Ok::<_, String>((found?, queued))
        })
        .await
        .map_err(|_| {
            Error::refused(
                refusal::NO_ANSWER,
                "Codex did not list the thread's turns in time",
            )
        })?
        .map_err(|line| Error::refused(refusal::REFUSED, line))?;
        let (turn, queued) = found;
        Ok(Receipt {
            found: turn.is_some(),
            owner: turn.map(|turn| match queued {
                true => Owner::Queue {
                    home,
                    thread: thread.clone(),
                    turn,
                }
                .token(),
                false => Owner::App {
                    thread: thread.clone(),
                    turn,
                }
                .token(),
            }),
        })
    }

    /// `say`: words through the window of the app that owns the thread, which takes them
    /// as typed into its composer; else into the queue of the running app server that owns
    /// it, which acknowledges and does not confirm.
    pub async fn say(&self, say: Say) -> Result<Said, Error> {
        let thread = &say.external_id;
        uuid(thread)?;
        if say.mode == SayMode::Immediate
            && let Some(Owned::Window(Some(_)) | Owned::Queue(Some(_))) = self.owned(thread).await
        {
            return Err(Error::refused(
                refusal::HARNESS_BUSY,
                "Codex is running a turn in it",
            ));
        }
        if let Some(router) = self.followers.router().map(Path::to_path_buf) {
            let took = async {
                let mut app = router::App::open(&router).await.ok()?;
                Some(app.say(thread, &say.words, None, None).await)
            }
            .await;
            match took {
                Some(Took::Yes) => return Ok(Said { took: true }),
                Some(Took::No(word, line)) => return Err(Error::refused(word, line)),
                Some(Took::Nobody) | None => {}
            }
        }
        let home = self.thread_home(thread)?;
        let codex = binary()?;
        let queued = tokio::time::timeout(native::WITHIN, async {
            let mut server = native::queue(&codex, &home, thread).await.ok()?;
            let added = server
                .call(
                    "thread/queue/add",
                    json!({"threadId": thread,
                           "input": [{"type": "text", "text": say.words, "text_elements": []}],
                           "clientUserMessageId": uuid::Uuid::new_v4().to_string()}),
                )
                .await;
            server.close().await;
            Some(added)
        })
        .await
        .ok()
        .flatten();
        match queued {
            // A queue's acknowledgement is not delivery: the runtime looks for the words.
            Some(Ok(_)) => Ok(Said { took: false }),
            Some(Err(failed)) => Err(Error::refused(refusal::REFUSED, failed.to_string())),
            None => Err(Error::refused(
                refusal::NO_DOOR,
                "no Codex window or app server takes words for it now",
            )),
        }
    }

    /// `pick`: a model and effort for the thread's next turns, set on the running app
    /// server that owns it (`thread/settings/update`) once its own list has them. Codex
    /// takes any model there and fails only at the turn, so the pick is checked first.
    pub async fn pick(&self, pick: Pick) -> Result<Picked, Error> {
        let thread = &pick.external_id;
        uuid(thread)?;
        let home = self.thread_home(thread)?;
        let codex = binary()?;
        let unsupported = || {
            Error::refused(
                refusal::SELECTION_UNSUPPORTED,
                "only a Codex app server that owns the thread applies a pick on its own",
            )
        };
        let mut server = tokio::time::timeout(native::WITHIN, native::queue(&codex, &home, thread))
            .await
            .map_err(|_| unsupported())?
            .map_err(|_| unsupported())?;
        let result = async {
            let listed = list(&mut server)
                .await
                .map_err(|line| Error::refused(refusal::REFUSED, line))?;
            let read = server
                .call(
                    "thread/read",
                    json!({"threadId": thread, "includeTurns": false}),
                )
                .await
                .map_err(|e| Error::refused(refusal::REFUSED, e.to_string()))?;
            let model = pick
                .model
                .clone()
                .or_else(|| {
                    read.pointer("/thread/model")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            let offer = listed.iter().find(|offer| offer.model == model);
            if pick.model.is_some() && offer.is_none() {
                return Err(Error::refused(
                    refusal::MODEL_MISSING,
                    format!("Codex lists no {model} here"),
                ));
            }
            if let (Some(thinking), Some(offer)) = (&pick.thinking, offer)
                && !offer.thinking.is_empty()
                && !offer.thinking.contains(thinking)
            {
                return Err(Error::refused(
                    refusal::SELECTION_UNSUPPORTED,
                    format!("{model} takes no effort {thinking}"),
                ));
            }
            let mut settings = json!({"threadId": thread});
            if let Some(model) = &pick.model {
                settings["model"] = json!(model);
            }
            if let Some(effort) = &pick.thinking {
                settings["effort"] = json!(effort);
            }
            server
                .call("thread/settings/update", settings)
                .await
                .map_err(|e| Error::refused(refusal::REFUSED, e.to_string()))?;
            let read = server
                .call(
                    "thread/read",
                    json!({"threadId": thread, "includeTurns": false}),
                )
                .await
                .map_err(|e| Error::refused(refusal::REFUSED, e.to_string()))?;
            Ok(Picked {
                model: read
                    .pointer("/thread/model")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                thinking: read
                    .pointer("/thread/reasoningEffort")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or(pick.thinking.clone()),
            })
        }
        .await;
        server.close().await;
        result
    }

    /// `terminal`: `codex resume <thread>` in its folder, under the home its rollout is
    /// filed in; the app opens it at its own address.
    pub fn terminal(&self, terminal: Terminal) -> Result<TerminalCommand, Error> {
        let id = &terminal.external_id;
        uuid(id)?;
        let home = self.thread_home(id)?;
        let cwd = terminal
            .cwd
            .to_str()
            .filter(|_| terminal.cwd.is_absolute())
            .ok_or_else(|| Error::invalid("the folder is no absolute UTF-8 path"))?;
        let mut command = format!("cd {} && ", quoted(cwd));
        if home != crate::home::default_root(&self.home) {
            let home = home
                .to_str()
                .ok_or_else(|| Error::invalid("Codex's home is not UTF-8"))?;
            command.push_str(&format!("CODEX_HOME={} ", quoted(home)));
        }
        command.push_str(&format!("codex resume {id}"));
        let line = match &terminal.holder {
            Some(Holder::Person { door: Some(_), .. }) => Some(
                "The Codex app has it open; a terminal beside it is refused while the app writes it."
                    .to_owned(),
            ),
            Some(Holder::Person { .. }) => {
                Some("Codex has it open already; a second one is refused while it writes.".to_owned())
            }
            _ => None,
        };
        Ok(TerminalCommand {
            command,
            app: Some(format!("codex://threads/{id}")),
            line,
        })
    }

    /// `models`: what Codex's own app server lists (`model/list`), the default first.
    pub async fn models(&self) -> Result<Models, Error> {
        let codex = binary()?;
        let root = self.store.root();
        let listed = tokio::time::timeout(std::time::Duration::from_secs(25), async {
            let mut command = steery_driver_common::program::helper(&codex);
            command.env("CODEX_HOME", root);
            let mut server = AppServer::open(command, false)
                .await
                .map_err(|e| e.to_string())?;
            let listed = list(&mut server).await;
            server.close().await;
            listed
        })
        .await
        .map_err(|_| Error::refused(refusal::NO_ANSWER, "Codex did not list its models in time"))?
        .map_err(|line| Error::refused(refusal::REFUSED, line))?;
        let mut watch: Vec<PathBuf> = ["config.toml", "auth.json", "models_cache.json"]
            .iter()
            .map(|name| root.join(name))
            .collect();
        watch.extend(
            ["/etc/codex/config.toml", "/etc/codex/managed_config.toml"].map(PathBuf::from),
        );
        Ok(Models {
            models: listed,
            unlisted: None,
            watch,
        })
    }
}

/// Every model an app server lists, page by page, the default first.
async fn list(server: &mut AppServer) -> Result<Vec<Model>, String> {
    let mut models = Vec::new();
    let mut cursors = std::collections::HashSet::new();
    let mut params = json!({});
    loop {
        let page = server
            .call("model/list", params)
            .await
            .map_err(|e| e.to_string())?;
        let (listed, next) = page_of(&page)?;
        models.extend(listed);
        let Some(cursor) = next else { break };
        if !cursors.insert(cursor.clone()) {
            return Err("Codex repeated a model cursor".into());
        }
        params = json!({"cursor": cursor});
    }
    models.sort_by_key(|(default, _)| !*default);
    Ok(models.into_iter().map(|(_, model)| model).collect())
}

/// A `model/list` page: the models it shows, whether each is the default, and its next
/// cursor.
/// One page of models, each with whether it is the default, and the next page's cursor.
type Page = (Vec<(bool, Model)>, Option<String>);

fn page_of(result: &Value) -> Result<Page, String> {
    let data = result
        .get("data")
        .and_then(Value::as_array)
        .ok_or("Codex's model/list has no data")?;
    let next = match result.get("nextCursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(cursor)) if !cursor.is_empty() => Some(cursor.clone()),
        _ => return Err("Codex's model/list has an invalid cursor".into()),
    };
    let mut models = Vec::new();
    for model in data {
        let model = model
            .as_object()
            .ok_or("Codex's model/list has an invalid model")?;
        if model.get("hidden").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let name = model
            .get("model")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or("Codex's model/list has an unnamed model")?
            .to_owned();
        let label = model
            .get("displayName")
            .and_then(Value::as_str)
            .filter(|label| *label != name)
            .map(str::to_owned);
        let thinking = match model.get("supportedReasoningEfforts") {
            None => Vec::new(),
            Some(Value::Array(efforts)) => efforts
                .iter()
                .map(|effort| {
                    effort
                        .get("reasoningEffort")
                        .and_then(Value::as_str)
                        .filter(|level| !level.is_empty())
                        .map(str::to_owned)
                        .ok_or("Codex's model/list has an invalid reasoning effort")
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err("Codex's model/list has invalid reasoning efforts".into()),
        };
        let default = model.get("isDefault").and_then(Value::as_bool) == Some(true);
        models.push((
            default,
            Model {
                provider: None,
                model: name,
                label,
                thinking,
                cost: None,
            },
        ));
    }
    Ok((models, next))
}

fn sample(state: Execution, now: u64, vacant: bool, if_unheld: bool) -> Sample {
    Sample {
        state,
        observed_at: now,
        started_at: None,
        ended_at: None,
        model: None,
        thinking: None,
        error: None,
        vacant,
        if_unheld,
    }
}

/// Where a rollout's newest records say its conversation stands, standing only while no
/// process holds the rollout: a turn begun and not ended runs; one ended is how it ended.
fn stands(events: &[Event], now: u64) -> Option<Sample> {
    let mut model = None;
    let mut thinking = None;
    let mut last = None;
    for event in events {
        match event {
            Event::Model {
                model: m,
                thinking: t,
                ..
            } => {
                model = Some(m.clone());
                thinking = t.clone();
            }
            Event::TurnBegan { at, .. } => last = Some((Execution::Running, Some(*at), None, None)),
            Event::TurnEnded {
                at, state, failure, ..
            } => {
                let started = last.and_then(
                    |(_, started, _, _): (Execution, Option<u64>, Option<u64>, Option<String>)| {
                        started
                    },
                );
                let execution = match state {
                    TurnState::Finished => Execution::Finished,
                    TurnState::Failed => Execution::Failed,
                    TurnState::Interrupted => Execution::Interrupted,
                };
                last = Some((
                    execution,
                    started,
                    Some(*at),
                    failure.as_ref().map(|f| f.line.clone()),
                ));
            }
            _ => {}
        }
    }
    let (state, started_at, ended_at, error) = last?;
    Some(Sample {
        state,
        observed_at: now,
        started_at,
        ended_at,
        model,
        thinking,
        error,
        vacant: state != Execution::Running,
        if_unheld: true,
    })
}

/// `turn/read`: one line of a turn's output, as the turn mode writes it — every message
/// of the app server on standard output, and its refusal on standard error.
pub fn turn_line(read: TurnRead) -> TurnLine {
    match read.stream {
        Stream::Stderr => TurnLine {
            refused: (!read.line.trim().is_empty()).then(|| refused(&read.line).to_owned()),
            ..TurnLine::default()
        },
        Stream::Stdout => {
            let Ok(line) = serde_json::from_str::<Value>(&read.line) else {
                return TurnLine::default();
            };
            TurnLine {
                named: line
                    .pointer("/result/thread/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                took: read
                    .delivery
                    .as_deref()
                    .filter(|delivery| took(&line, delivery))
                    .map(|_| true),
                ended: ended(&line),
                refused: None,
            }
        }
    }
}

/// Whether a line is the person input carrying the delivery marker, as the app server
/// says it took it (`item/started` of a `userMessage`).
fn took(line: &Value, delivery: &str) -> bool {
    let marker = format!("{DELIVERED}{delivery}");
    matches!(
        line.get("method").and_then(Value::as_str),
        Some("item/started" | "item/completed")
    ) && line.pointer("/params/item/type").and_then(Value::as_str) == Some("userMessage")
        && line
            .pointer("/params/item/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .any(|text| marked(text, &marker))
}

/// `turn/completed` is how each turn ends: `completed`, `failed` with the error the app
/// server gave, or `interrupted`.
fn ended(line: &Value) -> Option<Ended> {
    if line.get("method").and_then(Value::as_str) != Some("turn/completed") {
        return None;
    }
    let turn = line.pointer("/params/turn")?;
    Some(match turn.get("status").and_then(Value::as_str)? {
        "completed" => Ended {
            state: TurnState::Finished,
            error: None,
        },
        "failed" => Ended {
            state: TurnState::Failed,
            error: turn
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(crate::events::one_line),
        },
        "interrupted" => Ended {
            state: TurnState::Interrupted,
            error: None,
        },
        _ => return None,
    })
}

/// The refusal word a turn's line of standard error means: the app server refusing a
/// resume beside a live writer, a thread it cannot find, or anything else.
fn refused(said: &str) -> &'static str {
    if said.contains("active writer") {
        refusal::HARNESS_BUSY
    } else if said.contains("not found") || said.contains("no rollout") {
        refusal::NOT_FOUND
    } else {
        refusal::REFUSED
    }
}

/// `reader`: `codex exec --json --ephemeral` with every way out taken away: `--disable`
/// for each tool by name, every MCP server turned off, `--ignore-rules`, and a `read-only`
/// sandbox; asked on standard input. With `tools: read` the shell stays, under the
/// `read-only` sandbox, which is how Codex reads files. `config.toml` is kept: it holds
/// the person's provider and model, and dropping it sends Codex to an API key a person
/// signed in through ChatGPT never set. The instructions go in as
/// `developer_instructions`. Codex refuses a feature it does not know, so only those its
/// own `codex features list` names are taken away.
pub async fn reader(reader: Reader) -> Result<ReaderPlan, Error> {
    let codex = binary()?;
    let known = features(&codex).await?;
    let servers = servers(&codex, &reader.cwd).await?;
    planned(&codex, &known, &servers, reader)
}

/// [`reader`]'s plan for the Codex at `codex`, which knows the features `known` and
/// reaches the MCP servers `servers` from the reader's folder.
fn planned(
    codex: &Path,
    known: &[String],
    servers: &[String],
    reader: Reader,
) -> Result<ReaderPlan, Error> {
    let mut argv = vec![
        codex.to_string_lossy().into_owned(),
        "exec".into(),
        "--json".into(),
        "--ephemeral".into(),
        "--ignore-rules".into(),
        "--skip-git-repo-check".into(),
        "--color".into(),
        "never".into(),
        "--sandbox".into(),
        "read-only".into(),
    ];
    for server in servers {
        argv.push("-c".into());
        argv.push(format!("mcp_servers.{server}.enabled=false"));
    }
    let taken = match reader.tools {
        ReaderTools::None => TAKEN,
        ReaderTools::Read => LOOK_TAKEN,
    };
    for feature in taken
        .iter()
        .filter(|feature| known.iter().any(|known| known == *feature))
    {
        argv.push("--disable".into());
        argv.push((*feature).into());
    }
    if let Some(system) = &reader.system {
        argv.push("-c".into());
        argv.push(format!(
            "developer_instructions={}",
            serde_json::to_string(system).map_err(|e| Error::invalid(e.to_string()))?
        ));
    }
    if let Some(model) = reader.model.as_deref().filter(|model| !model.is_empty()) {
        argv.push("--model".into());
        argv.push(model.into());
    }
    argv.push("-".into());
    Ok(ReaderPlan {
        argv,
        env: BTreeMap::new(),
        cwd: reader.cwd,
        stdin: Stdin::Words,
        files: Vec::new(),
        read: ReadAs::Events,
    })
}

/// The MCP servers Codex would reach from `cwd`, as `codex mcp list --json` names them:
/// each is turned off for the read, since `-c mcp_servers={}` merges into them rather than
/// replacing them (Codex 0.162). A name a dotted override cannot spell is refused.
async fn servers(codex: &Path, cwd: &Path) -> Result<Vec<String>, Error> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        steery_driver_common::program::helper(codex)
            .args(["mcp", "list", "--json"])
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| {
        Error::refused(
            refusal::NO_ANSWER,
            "`codex mcp list` did not answer in time",
        )
    })?
    .map_err(|e| Error::refused(refusal::NO_BINARY, format!("codex would not start: {e}")))?;
    let listed: Value = serde_json::from_slice(&output.stdout)
        .ok()
        .filter(|_| output.status.success())
        .ok_or_else(|| {
            Error::refused(
                refusal::REFUSED,
                "`codex mcp list` did not list its servers",
            )
        })?;
    let mut names = Vec::new();
    for server in listed.as_array().into_iter().flatten() {
        let name = server
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(Error::refused(
                refusal::REFUSED,
                format!("Codex's MCP server {name:?} cannot be turned off for one read"),
            ));
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

/// The features this Codex knows, as `codex features list` names them, one per line.
async fn features(codex: &Path) -> Result<Vec<String>, Error> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        steery_driver_common::program::helper(codex)
            .args(["features", "list"])
            .stdout(std::process::Stdio::piped())
            .output(),
    )
    .await
    .map_err(|_| {
        Error::refused(
            refusal::NO_ANSWER,
            "`codex features list` did not answer in time",
        )
    })?
    .map_err(|e| Error::refused(refusal::NO_BINARY, format!("codex would not start: {e}")))?;
    if !output.status.success() {
        return Err(Error::refused(
            refusal::REFUSED,
            "`codex features list` failed",
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect())
}

/// `reader/read`: the last `agent_message` item of `codex exec --json`'s events. Lines
/// that are not JSON and events of other kinds are stepped over.
pub fn reader_read(read: ReaderRead) -> Result<Text, Error> {
    let mut said = None;
    let mut failed = None;
    for line in read.output.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) == Some("turn.failed") {
            failed = event
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(crate::events::one_line);
        }
        let item = event.get("item").unwrap_or(&Value::Null);
        if item.get("type").and_then(Value::as_str) != Some("agent_message") {
            continue;
        }
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            said = Some(text.to_owned());
        }
    }
    said.map(|text| Text { text }).ok_or_else(|| {
        Error::refused(
            refusal::NO_ANSWER,
            failed.unwrap_or_else(|| "the reader said nothing that reads as an answer".into()),
        )
    })
}

/// Every Codex feature that would let captured text reach this machine or the network.
const TAKEN: &[&str] = &[
    "shell_tool",
    "unified_exec",
    "apply_patch_freeform",
    "view_image",
    "image_generation",
    "browser_use",
    "browser_use_external",
    "computer_use",
    "in_app_browser",
    "plugins",
    "remote_plugin",
    "apps",
    "web_search_request",
    "standalone_web_search",
    "hooks",
    "multi_agent",
    "goals",
    "sleep_tool",
];

/// Every feature a look takes away: all the reader's but the shell, which is how Codex
/// reads a file.
const LOOK_TAKEN: &[&str] = &[
    "hooks",
    "multi_agent",
    "goals",
    "image_generation",
    "browser_use",
    "browser_use_external",
    "computer_use",
    "in_app_browser",
    "plugins",
    "remote_plugin",
    "apps",
    "web_search_request",
    "standalone_web_search",
    "sleep_tool",
];

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(stream: Stream, line: &str, delivery: Option<&str>) -> TurnLine {
        turn_line(TurnRead {
            turn: "t".into(),
            stream,
            line: line.into(),
            delivery: delivery.map(str::to_owned),
        })
    }

    /// A turn's lines: the response that opened the thread names it, the person input
    /// carrying the marker is the words taken, `turn/completed` ends it, and a refusal on
    /// standard error says which.
    #[test]
    fn a_turn_s_lines_say_what_happened() {
        let opened = line(
            Stream::Stdout,
            r#"{"id":1,"result":{"thread":{"id":"01a1"}}}"#,
            None,
        );
        assert_eq!(opened.named.as_deref(), Some("01a1"));
        let item = r#"{"method":"item/started","params":{"item":{"type":"userMessage","content":[{"type":"text","text":"go steery-delivery:d1"}]}}}"#;
        assert_eq!(line(Stream::Stdout, item, Some("d1")).took, Some(true));
        assert_eq!(line(Stream::Stdout, item, Some("d")).took, None);
        let failed = r#"{"method":"turn/completed","params":{"threadId":"01a1","turn":{"id":"u","status":"failed","error":{"message":"unexpected status 401 Unauthorized"}}}}"#;
        assert_eq!(
            line(Stream::Stdout, failed, None).ended,
            Some(Ended {
                state: TurnState::Failed,
                error: Some("unexpected status 401 Unauthorized".into())
            })
        );
        let done =
            r#"{"method":"turn/completed","params":{"turn":{"id":"u","status":"completed"}}}"#;
        assert_eq!(
            line(Stream::Stdout, done, None).ended.map(|e| e.state),
            Some(TurnState::Finished)
        );
        let status = |status: &str| {
            let completed = json!({"method": "turn/completed", "params": {"threadId": "01a1", "turn": {"id": "u", "status": status}}});
            line(Stream::Stdout, &completed.to_string(), None)
                .ended
                .map(|e| e.state)
        };
        assert_eq!(status("interrupted"), Some(TurnState::Interrupted));
        assert_eq!(status("inProgress"), None);
        let started = line(
            Stream::Stdout,
            r#"{"id":4,"result":{"turn":{"id":"u"}}}"#,
            None,
        );
        assert_eq!(started.named, None, "a turn's response names no thread");
        let retrying = line(
            Stream::Stdout,
            r#"{"method":"error","params":{"willRetry":false}}"#,
            None,
        );
        assert_eq!(retrying.ended, None, "only turn/completed ends a turn");
        assert_eq!(
            line(
                Stream::Stderr,
                "thread/resume: thread already has an active writer",
                None
            )
            .refused
            .as_deref(),
            Some(refusal::HARNESS_BUSY)
        );
        assert_eq!(
            line(Stream::Stderr, "thread/resume: no rollout found", None)
                .refused
                .as_deref(),
            Some(refusal::NOT_FOUND)
        );
        assert_eq!(line(Stream::Stdout, "not json", None), TurnLine::default());
    }

    fn asked(tools: ReaderTools, model: Option<&str>) -> Reader {
        Reader {
            system: Some("you read".into()),
            model: model.map(str::to_owned),
            tools,
            cwd: PathBuf::from("/tmp/steery-read"),
            asked: "q".into(),
        }
    }

    fn known() -> Vec<String> {
        TAKEN
            .iter()
            .chain(LOOK_TAKEN)
            .map(|f| (*f).to_owned())
            .collect()
    }

    /// Codex keeps its own `config.toml` — that is where the provider a person signed in
    /// to lives — loses every tool and MCP server, reads only, keeps no session, and is
    /// asked on standard input; no model named asks for its own.
    #[test]
    fn codex_keeps_its_sign_in_and_loses_its_tools() {
        let servers = vec!["steery".to_owned()];
        let plan = planned(
            Path::new("/bin/codex"),
            &known(),
            &servers,
            asked(ReaderTools::None, None),
        )
        .unwrap();
        let argv = &plan.argv;
        assert_eq!(argv[1], "exec");
        for expected in ["--json", "--ignore-rules", "--ephemeral", "--sandbox"] {
            assert!(argv.iter().any(|arg| arg == expected), "missing {expected}");
        }
        assert!(
            !argv.iter().any(|arg| arg == "--ignore-user-config"),
            "keeps config.toml"
        );
        let sandbox = argv.iter().position(|arg| arg == "--sandbox").unwrap();
        assert_eq!(argv[sandbox + 1], "read-only");
        assert!(
            argv.iter()
                .any(|arg| arg == "mcp_servers.steery.enabled=false")
        );
        assert!(!argv.iter().any(|arg| arg == "--model"), "its own model");
        assert_eq!((plan.stdin, plan.read), (Stdin::Words, ReadAs::Events));
        assert!(plan.files.is_empty());
        let named = planned(
            Path::new("/bin/codex"),
            &known(),
            &[],
            asked(ReaderTools::None, Some("m")),
        )
        .unwrap();
        let model = named.argv.iter().position(|arg| arg == "--model").unwrap();
        assert_eq!(named.argv[model + 1], "m");
    }

    /// Every feature taken away is passed with its own `--disable`, in pairs; a look keeps
    /// the shell, how Codex reads a file; a feature this Codex does not know is not named.
    #[test]
    fn every_taken_tool_is_named_with_its_own_switch() {
        let disabled = |tools, known: &[String]| {
            let plan = planned(Path::new("/bin/codex"), known, &[], asked(tools, None)).unwrap();
            plan.argv
                .windows(2)
                .filter(|pair| pair[0] == "--disable")
                .map(|pair| pair[1].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(disabled(ReaderTools::None, &known()), TAKEN);
        let look = disabled(ReaderTools::Read, &known());
        assert_eq!(look, LOOK_TAKEN);
        assert!(!look.iter().any(|f| f == "shell_tool"));
        assert_eq!(
            disabled(ReaderTools::None, &["hooks".to_owned()]),
            ["hooks"]
        );
    }

    /// The reader's answer is the last agent message, whatever else `codex exec` prints.
    #[test]
    fn a_reader_s_answer_is_its_last_agent_message() {
        let output = "Reading prompt from stdin...\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"first\"}}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Fix login\"}}\n{\"type\":\"turn.completed\",\"usage\":{}}\ntokens used: 12\n";
        assert_eq!(
            reader_read(ReaderRead {
                output: output.into()
            })
            .unwrap()
            .text,
            "Fix login"
        );
        assert!(
            reader_read(ReaderRead {
                output: "tokens used: 12".into()
            })
            .is_err()
        );
    }

    /// As Codex lists them: the default first, a hidden one left out, each with the
    /// efforts it takes and a name for a person only where it differs.
    #[test]
    fn codex_lists_its_shown_models_default_first() {
        let (mut listed, next) = page_of(&json!({"data": [
            {"model": "gpt-6-sol", "displayName": "gpt-6-sol", "hidden": false, "isDefault": false,
             "supportedReasoningEfforts": [{"reasoningEffort": "low"}, {"reasoningEffort": "ultra"}]},
            {"model": "gpt-6-astra", "displayName": "GPT-6 Astra", "isDefault": true,
             "supportedReasoningEfforts": [{"reasoningEffort": "low"}]},
            {"model": "internal", "hidden": true, "supportedReasoningEfforts": []}
        ]})).unwrap();
        assert!(next.is_none());
        listed.sort_by_key(|(default, _)| !*default);
        let listed: Vec<Model> = listed.into_iter().map(|(_, model)| model).collect();
        assert_eq!(
            listed.iter().map(|m| m.model.as_str()).collect::<Vec<_>>(),
            ["gpt-6-astra", "gpt-6-sol"]
        );
        assert_eq!(listed[0].label.as_deref(), Some("GPT-6 Astra"));
        assert_eq!(listed[1].label, None);
        assert_eq!(listed[1].thinking, ["low", "ultra"]);
    }

    #[test]
    fn codex_rejects_invalid_model_pages() {
        for page in [
            json!({}),
            json!({"data": [], "nextCursor": 5}),
            json!({"data": [{"displayName": "unnamed"}]}),
            json!({"data": [{"model": "m", "supportedReasoningEfforts": [null]}]}),
        ] {
            assert!(page_of(&page).is_err(), "{page}");
        }
    }

    /// Where a rollout says its conversation stands: a turn begun and not ended runs; an
    /// ended one is vacant, both only while nothing holds the rollout.
    #[test]
    fn a_rollout_s_newest_turn_says_where_it_stands() {
        let running = stands(&[Event::TurnBegan { at: 5, turn: None }], 9).unwrap();
        assert_eq!(
            (running.state, running.vacant, running.if_unheld),
            (Execution::Running, false, true)
        );
        let ended = stands(
            &[
                Event::TurnBegan { at: 5, turn: None },
                Event::TurnEnded {
                    at: 7,
                    turn: None,
                    state: TurnState::Interrupted,
                    failure: None,
                },
            ],
            9,
        )
        .unwrap();
        assert_eq!(
            (ended.state, ended.started_at, ended.ended_at, ended.vacant),
            (Execution::Interrupted, Some(5), Some(7), true)
        );
        assert!(stands(&[], 9).is_none());
    }
}
