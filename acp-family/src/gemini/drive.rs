//! Gemini CLI at drive: Steery starts Gemini's turns itself. A turn is this driver's own
//! binary in its turn mode, speaking ACP to `gemini --acp` ([`crate::turn`]), which runs the
//! conversation in the turn's own process. Gemini keeps no mark of a running turn, no lock
//! and no pid, so who holds a conversation is for the runtime to look for, and what stands
//! of it is true only while nobody does.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufReader, Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use steery_driver_common::lines::{self, Line, RECORD};
use steery_driver_common::receipt::{self, DELIVERED, marked};
use steery_sdk::wire::harness::{
    Act, Beyond, Execution, Held, HeldBy, Holder, HookMoment, HookRead, Launch, Look, Looked,
    Marked, Model, Receipt, ReceiptFind, ReceiptMark, Sample, Sampled, Start, Terminal,
    TerminalCommand, TurnState,
};
use steery_sdk::{Error, refusal};

use super::store::Store;
use crate::turn::drive::{self as shared, now};
use crate::turn::hooks;

/// Gemini, to a turn this driver runs ([`crate::turn`]): words told mid-turn go in as one
/// prompt once Gemini has answered the turn's, as a second `session/prompt` aborts the
/// running one (`acpSession.ts`). Gemini runs Steery's own `AfterAgent` hook as the turn's
/// agent ends, and goes on with the words it gives.
pub struct Turns {
    /// Gemini's root, where the turn has a home to find it in.
    root: Option<PathBuf>,
}

impl Turns {
    /// Gemini's root under `$HOME`, as the runtime starts the turn.
    pub fn new() -> Self {
        Self {
            root: std::env::var_os("HOME").map(|home| super::root(Path::new(&home))),
        }
    }
}

impl crate::turn::Host for Turns {
    fn harness(&self) -> crate::Harness {
        crate::Harness::Gemini
    }

    fn told(&self) -> crate::turn::Told {
        crate::turn::Told::After
    }

    fn serves(&self) -> &'static [&'static str] {
        &["--acp"]
    }

    fn opens(&self) -> crate::turn::Opens {
        crate::turn::Opens::Load
    }

    /// Gemini sets a session's model with ACP's unstable `session/set_model`; it has no
    /// config option for it.
    fn pick(&self, session: &str, _option: &str, value: &str) -> (&'static str, Value) {
        (
            "session/set_model",
            json!({"sessionId": session, "modelId": value}),
        )
    }

    /// The conversation's mark, which lists it ([`super::store`]).
    fn opened(&self, session: &str) -> std::io::Result<()> {
        let Some(root) = &self.root else {
            return Err(std::io::Error::other("no HOME names Gemini's root"));
        };
        if session.is_empty() || session.starts_with('.') || session.contains('/') {
            return Err(std::io::Error::other(format!(
                "Gemini opened a session named `{session}`"
            )));
        }
        let started = super::store::started(root);
        std::fs::create_dir_all(&started)?;
        File::create(started.join(session)).map(drop)
    }

    fn goes_on(&self) -> bool {
        false
    }

    /// Gemini runs one prompt of a session at a time and nothing of it without one.
    fn runs(
        &self,
        _acp: Option<u32>,
        _session: &str,
        _queued: &[String],
        _: &mut Option<tokio::time::Instant>,
    ) -> bool {
        false
    }
}

/// `held`: the runtime looks for the processes that hold or name the conversation's chat
/// files. Gemini opens its file only to append a record, so a `gemini` that runs the
/// conversation is a stray between its writes. `gemini --acp` serves every conversation its
/// client opens, so stopping it would stop them all.
pub fn held(store: &Store, held: &Held) -> Result<HeldBy, Error> {
    let Ok((files, _)) = store.files(&held.external_id) else {
        return Ok(HeldBy::Holder(Holder::Nobody {
            proof: "Gemini CLI has no such conversation".into(),
        }));
    };
    Ok(HeldBy::Look(Looked {
        look: Look {
            transcripts: files,
            strays: true,
            beyond: vec![Beyond {
                arg: Some("--acp".into()),
                line: "Gemini CLI's ACP server, which serves every conversation its client opens"
                    .into(),
            }],
        },
    }))
}

/// `idle`: how the conversation's last turn ended, by its newest chat file, which stands
/// only while nobody holds it.
pub fn idle(store: &Store, held: &Held) -> Sampled {
    let sample = store
        .files(&held.external_id)
        .ok()
        .and_then(|(files, _)| super::events::stands(files.last()?).ok().flatten())
        .map(|stands| Sample {
            state: match stands.state {
                TurnState::Finished => Execution::Finished,
                TurnState::Failed => Execution::Failed,
                TurnState::Interrupted => Execution::Interrupted,
            },
            observed_at: now(),
            started_at: None,
            ended_at: Some(stands.at),
            model: stands.model,
            thinking: None,
            error: None,
            vacant: true,
            if_unheld: true,
        });
    Sampled { sample }
}

/// `start`: the launch plan of a turn ([`shared::launch`]). Gemini has no fork, no
/// hand-over and no thinking level an ACP client can pick; a model its last list (`listed`)
/// does not have is refused before anything runs.
pub fn start(root: &Path, store: &Store, listed: &[Model], start: Start) -> Result<Launch, Error> {
    if matches!(start.act, Act::Fork | Act::Hand) {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Gemini CLI continues a conversation or starts one; it has no fork or hand-over",
        ));
    }
    if start.thinking.is_some() {
        return Err(Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            "Gemini CLI takes no thinking level from its ACP client",
        ));
    }
    shared::offered("Gemini CLI", listed, start.model.as_deref(), None)?;
    shared::launch(
        "Gemini CLI",
        super::binary().as_deref(),
        start,
        |id| store.files(id).map(|(_, cwd)| cwd),
        shared::hook(&super::setup::hook_file(root)),
        BTreeMap::new(),
    )
}

/// A point in a conversation's newest chat file: its identity and the end of its last
/// whole record.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mark {
    dev: u64,
    ino: u64,
    bytes: u64,
}

/// `receipt/mark`: where the conversation's newest chat file ends now.
pub fn mark(store: &Store, mark: &ReceiptMark) -> Result<Marked, Error> {
    let (files, _) = store.files(&mark.external_id)?;
    let path = files.last().ok_or_else(|| Error::invalid("no chat file"))?;
    let end = receipt::end(path).map_err(|e| {
        Error::refused(
            refusal::SELECTION_UNSUPPORTED,
            format!("{}: {e}", path.display()),
        )
    })?;
    let mark = Mark {
        dev: end.dev,
        ino: end.ino,
        bytes: end.bytes,
    };
    Ok(Marked {
        mark: serde_json::to_string(&mark).map_err(|e| Error::invalid(e.to_string()))?,
    })
}

/// `receipt/find`: the person's prompts that carry the delivery's marker, among the whole
/// records of the newest chat file past the mark. Gemini writes a message again under its
/// id as it learns more of it, so a prompt is counted once by its id; two are refused. A
/// file that is not the marked one (Gemini moved an older release's `.json` into a
/// `.jsonl`) is read whole.
pub fn find(store: &Store, find: &ReceiptFind) -> Result<Receipt, Error> {
    let mark: Mark = serde_json::from_str(&find.mark)
        .map_err(|_| Error::invalid("this driver did not make the mark"))?;
    let (files, _) = store.files(&find.external_id)?;
    let path = files.last().ok_or_else(|| Error::invalid("no chat file"))?;
    let unread =
        |e: std::io::Error| Error::refused(refusal::REFUSED, format!("{}: {e}", path.display()));
    let mut file = File::open(path).map_err(unread)?;
    let meta = file.metadata().map_err(unread)?;
    let same = meta.dev() == mark.dev && meta.ino() == mark.ino && meta.len() >= mark.bytes;
    let start = if same { mark.bytes } else { 0 };
    file.seek(SeekFrom::Start(start)).map_err(unread)?;
    let mut reader = BufReader::new(file.take(meta.len() - start));
    let marker = format!("{DELIVERED}{}", find.delivery);
    let mut carrying = BTreeSet::new();
    let mut line = Vec::new();
    loop {
        match lines::capped(&mut reader, &mut line, RECORD).map_err(unread)? {
            Line::Partial => break,
            Line::Long => {}
            Line::Whole => {
                let Ok(record) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                if record["type"] == "user"
                    && let Some(id) = record["id"].as_str()
                    && super::events::person(&record)
                        .is_some_and(|(words, _)| marked(&words, &marker))
                {
                    carrying.insert(id.to_owned());
                }
            }
        }
    }
    if carrying.len() > 1 {
        return Err(Error::refused(
            refusal::REFUSED,
            "two inputs carry the delivery marker",
        ));
    }
    Ok(Receipt {
        found: carrying.len() == 1,
        owner: None,
    })
}

/// `terminal`: Gemini's own window on the conversation, in the folder it ran in.
pub fn terminal(store: &Store, terminal: &Terminal) -> Result<TerminalCommand, Error> {
    let (_, cwd) = store.files(&terminal.external_id)?;
    let cwd = cwd.unwrap_or_else(|| terminal.cwd.clone());
    Ok(TerminalCommand {
        command: format!(
            "cd {} && gemini --resume {}",
            quoted(&cwd.to_string_lossy()),
            quoted(&terminal.external_id)
        ),
        app: None,
        line: None,
    })
}

/// What an `allow_always` of Gemini's keeps, as each option names it (`acpUtils.ts`).
const ALWAYS: &str = "what the option names: Gemini CLI keeps it for this conversation, or \
                      in its policy for later conversations";

/// The moment of a permission Gemini asks a turn of this driver's for. Gemini's request
/// carries no input of the call: its title, content and locations stand for it. An MCP
/// call names its server and tool; a call of the model's is named by the tool that Gemini
/// puts before `__` in its id (`geminiChat.ts`), and another by its ACP `kind`.
pub fn permission(store: &Store, read: &HookRead) -> Result<HookMoment, Error> {
    let document = hooks::parse(&read.document)?;
    let Some(external_id) = hooks::text(&document, "/sessionId") else {
        return Ok(HookMoment { moment: None });
    };
    let cwd = store.files(external_id).ok().and_then(|(_, cwd)| cwd);
    let (id, kind, _) = hooks::requested(&document);
    let requested = &document["toolCall"];
    let mcp = requested.pointer("/_meta/gemini-cli/mcp");
    let server = mcp.and_then(|mcp| mcp["server_name"].as_str());
    let name = match (server, mcp.and_then(|mcp| mcp["tool_name"].as_str())) {
        (Some(server), Some(tool)) => format!("mcp_{server}_{tool}"),
        _ => id
            .split_once("__")
            .map(|(tool, _)| tool)
            .filter(|tool| !tool.is_empty())
            .map_or(kind, str::to_owned),
    };
    let input = json!({
        "title": requested["title"],
        "content": requested["content"],
        "locations": requested["locations"],
    });
    let call = super::events::call(id, &name, input, server);
    Ok(hooks::asked(external_id, cwd, call, &document, ALWAYS))
}

/// One literal POSIX shell word.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}
