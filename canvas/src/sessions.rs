//! The sessions the canvas keeps on this host, one transcript each in its sessions' folder
//! ([`crate::home`]): listed, located, read and folded from the door ([`crate::transcript`]),
//! and the receipt of words in them.

use std::io;
use std::path::Path;

use steery_driver_common::receipt;
use steery_sdk::wire::harness::{
    Events, Fold, Folded, List, Listed, Locate, Located, Marked, Read, Receipt, ReceiptFind,
    ReceiptMark, Session,
};
use steery_sdk::{Error, refusal};

use crate::{home, transcript};

/// `sessions/fold`: the records the canvas sent over the door, folded as `sessions/read`
/// folds the same lines. The canvas runs no sub-agents.
pub fn fold(root: &Path, fold: Fold) -> Result<Folded, Error> {
    if let Some(sub) = fold.sub_agent {
        return Err(Error::refused(
            refusal::NOT_FOUND,
            format!("the canvas runs no sub-agents, so {sub} is not here"),
        ));
    }
    let path = home::transcript_to_be(root, &fold.external_id)?;
    transcript::fold(&path, fold.cursor.as_deref(), fold.records)
        .map_err(|e| unreadable(&path, &e))
}

/// The session of the transcript at `path`, named `id`.
fn session(path: &Path, id: String) -> io::Result<Session> {
    let summary = transcript::summary(path)?;
    Ok(Session {
        external_id: id,
        cwd: summary.cwd,
        title: summary.title,
        created_at: summary.created_at,
        updated_at: transcript::modified(path)?,
        transcripts: vec![path.to_owned()],
        parent: None,
        app: false,
        sub_agents: Vec::new(),
    })
}

/// `sessions/list`: every transcript in the sessions' folder. The canvas keeps them nowhere
/// else, so the transcripts the runtime saw add none.
pub fn list(root: &Path, asked: List) -> Listed {
    let mut sessions: Vec<Session> = std::fs::read_dir(home::sessions(root))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let id = home::id_of(&path)?;
            session(&path, id).ok()
        })
        .filter(|session| asked.seen_since.is_none_or(|since| session.updated_at >= since))
        .collect();
    sessions.sort_by_key(|session| session.updated_at);
    Listed {
        sessions,
        excluded: Vec::new(),
        complete: true,
        cursor: None,
        after: None,
        gone: Vec::new(),
    }
}

/// `sessions/locate`.
pub fn locate(root: &Path, locate: Locate) -> Result<Located, Error> {
    let path = home::transcript(root, &locate.external_id)?;
    let session = session(&path, locate.external_id).map_err(|e| unreadable(&path, &e))?;
    Ok(Located { session })
}

/// `sessions/read`. The canvas runs no sub-agents.
pub fn read(root: &Path, read: Read) -> Result<Events, Error> {
    if let Some(sub) = read.sub_agent {
        return Err(Error::refused(
            refusal::NOT_FOUND,
            format!("the canvas runs no sub-agents, so {sub} is not here"),
        ));
    }
    let path = home::transcript(root, &read.external_id)?;
    transcript::read(&path, read.cursor.as_deref(), read.tail).map_err(|e| unreadable(&path, &e))
}

/// `receipt/mark`: where the transcript's last whole record ends.
pub fn mark(root: &Path, mark: ReceiptMark) -> Result<Marked, Error> {
    let path = home::transcript(root, &mark.external_id)?;
    let end = receipt::end(&path).map_err(|e| unreadable(&path, &e))?;
    Ok(Marked {
        mark: format!("{}:{}:{}", end.dev, end.ino, end.bytes),
    })
}

/// `receipt/find`: whether one `words` record after the mark carries the delivery marker.
pub fn find(root: &Path, find: ReceiptFind) -> Result<Receipt, Error> {
    let end = parse_mark(&find.mark)
        .ok_or_else(|| Error::invalid(format!("{} is not a mark of the canvas's", find.mark)))?;
    let path = home::transcript(root, &find.external_id)?;
    let found = receipt::found(&path, &end, &find.delivery, transcript::carries)
        .map_err(|why| Error::refused(refusal::REFUSED, why))?;
    Ok(Receipt {
        found,
        owner: None,
    })
}

fn parse_mark(mark: &str) -> Option<receipt::End> {
    let mut parts = mark.split(':').map(str::parse::<u64>);
    let end = receipt::End {
        dev: parts.next()?.ok()?,
        ino: parts.next()?.ok()?,
        bytes: parts.next()?.ok()?,
    };
    parts.next().is_none().then_some(end)
}

pub fn unreadable(path: &Path, e: &io::Error) -> Error {
    if e.kind() == io::ErrorKind::NotFound {
        return Error::refused(refusal::NOT_FOUND, format!("{} is gone", path.display()));
    }
    Error::refused(
        refusal::REFUSED,
        format!("cannot read {}: {e}", path.display()),
    )
}
