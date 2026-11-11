//! A one-shot read through `copilot -s`. Copilot takes no instructions of a caller's own in
//! place of its own, so the read gives them first in the words it reads. The runtime writes
//! those words to a file in the reader's folder; this driver's reader mode ([`main`]) runs
//! Copilot on that file under a conversation id of its own, and then has Copilot delete
//! that conversation, as Copilot has no flag that keeps a run unsaved.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::PathBuf;

use steery_sdk::wire::harness::{
    ReadAs, Reader, ReaderFile, ReaderPlan, ReaderRead, ReaderTools, Stdin, Text,
};
use steery_sdk::{Error, refusal};

use super::{home, server, turn};

/// The first argument of this driver's reader mode.
pub const MODE: &str = "reader";

/// The file, in the reader's folder, that holds the words the read gives Copilot.
const WORDS: &str = ".steery-reader-words.md";

/// Copilot's built-in tools that only read (1.0.108).
const READ_TOOLS: [&str; 3] = ["view", "grep", "glob"];

/// `reader`: `copilot -s --no-ask-user --no-custom-instructions --disable-builtin-mcps`,
/// with no tool (`--excluded-tools` every source) or only [`READ_TOOLS`], on the model and
/// effort given. The answer is the whole output.
pub fn plan(reader: Reader) -> Result<ReaderPlan, Error> {
    let copilot = home::binary()
        .ok_or_else(|| Error::refused(refusal::NO_BINARY, "copilot is not installed here"))?;
    if !reader.cwd.is_absolute() {
        return Err(Error::invalid(
            "the reader's folder is not an absolute path",
        ));
    }
    turn::effort(reader.thinking.as_deref())?;
    let program = std::env::current_exe().map_err(|e| {
        Error::refused(
            refusal::START_FAILED,
            format!("cannot find this driver's own path: {e}"),
        )
    })?;
    let mut argv = vec![
        text(program)?,
        MODE.into(),
        text(reader.cwd.join(WORDS))?,
        "--".into(),
        text(copilot)?,
        "-s".into(),
        "--no-ask-user".into(),
        "--no-custom-instructions".into(),
        "--disable-builtin-mcps".into(),
        "--no-color".into(),
    ];
    if let Some(model) = reader.model {
        argv.extend(["--model".into(), model]);
    }
    if let Some(thinking) = reader.thinking {
        argv.extend(["--reasoning-effort".into(), thinking]);
    }
    match reader.tools {
        ReaderTools::None => {
            argv.extend(["--excluded-tools", "builtin:*", "mcp:*", "custom:*"].map(str::to_owned))
        }
        ReaderTools::Read => {
            argv.push("--available-tools".into());
            argv.extend(READ_TOOLS.map(str::to_owned));
        }
    }
    let words = match reader.system {
        Some(system) => format!("{system}\n\n{}", reader.asked),
        None => reader.asked,
    };
    Ok(ReaderPlan {
        argv,
        env: BTreeMap::new(),
        cwd: reader.cwd,
        stdin: Stdin::None,
        files: vec![ReaderFile {
            path: PathBuf::from(WORDS),
            content: words,
        }],
        read: ReadAs::Whole,
    })
}

fn text(path: PathBuf) -> Result<String, Error> {
    path.into_os_string()
        .into_string()
        .map_err(|path| Error::invalid(format!("{} is not text", path.display())))
}

/// `reader/read`: the whole output is the answer.
pub fn read(read: ReaderRead) -> Result<Text, Error> {
    Ok(Text {
        text: read.output.trim().to_owned(),
    })
}

/// The reader mode: `reader <words> -- <copilot...>`. It runs Copilot on a new conversation
/// id, the file `words` on its standard input and its output this process's own, then has
/// Copilot delete that conversation and exits as Copilot did.
pub async fn main(mut args: impl Iterator<Item = String>) -> std::io::Result<()> {
    let usage = || std::io::Error::other("usage: reader <words> -- <copilot...>");
    let words = std::fs::File::open(args.next().ok_or_else(usage)?)?;
    if args.next().as_deref() != Some("--") {
        return Err(usage());
    }
    let copilot = PathBuf::from(args.next().ok_or_else(usage)?);
    let id = uuid()?;
    let status = tokio::process::Command::new(&copilot)
        .args(args)
        .arg(format!("--session-id={id}"))
        .stdin(words)
        .status()
        .await?;
    if let Err(why) = server::delete(&copilot, &id).await {
        eprintln!("Copilot kept the read as conversation {id}: {why}");
    }
    std::process::exit(status.code().unwrap_or(1))
}

/// A random version 4 UUID, the form `--session-id` takes.
fn uuid() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}
