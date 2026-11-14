//! The conversations Aider keeps on this host, each one chat history file ([`crate::home`]),
//! and what the driver knows of them: the files it found or planned, by id, and what each
//! file says for its listing, kept until the file changes.
//!
//! A file whose every run gave Aider its words on the command line (`--message`,
//! `--message-file`) is excluded: a program ran it, and nobody was at it. Steery lists the
//! turns it started itself through `start`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use steery_sdk::wire::harness::{Events, List, Listed, Locate, Located, Read, Session};
use steery_sdk::{Error, refusal};

use crate::history::{self, Summary};
use crate::home;

/// What the driver knows of the files.
#[derive(Default)]
pub struct Store {
    known: Mutex<Known>,
}

#[derive(Default)]
struct Known {
    /// Every file found or planned, by its id.
    paths: BTreeMap<String, PathBuf>,
    /// What each file said, at the length and change time it had then.
    summaries: HashMap<PathBuf, (u64, u64, Summary)>,
}

impl Store {
    /// A file this driver will start a conversation in: listed and located from now on.
    pub fn planned(&self, path: &Path) {
        self.lock().paths.insert(home::id(path), path.to_owned());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Known> {
        self.known
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Every file now: the walk's, the transcripts the runtime saw, and the ones known.
    fn files(&self, home: &Path, transcripts: &[PathBuf]) -> BTreeSet<PathBuf> {
        let mut files = home::walk(home);
        files.extend(
            transcripts
                .iter()
                .filter(|path| {
                    path.is_absolute()
                        && path.is_file()
                        && path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(home::is_history)
                })
                .cloned(),
        );
        let mut known = self.lock();
        files.extend(known.paths.values().filter(|path| path.is_file()).cloned());
        for path in &files {
            known.paths.insert(home::id(path), path.clone());
        }
        files
    }

    /// The file of a conversation: one known, else one a new walk finds.
    pub fn path(&self, home: &Path, id: &str) -> Result<PathBuf, Error> {
        let known = self.lock().paths.get(id).cloned();
        let path = match known {
            Some(path) => path,
            None => {
                self.files(home, &[]);
                self.lock()
                    .paths
                    .get(id)
                    .cloned()
                    .ok_or_else(|| not_found(id))?
            }
        };
        if path.is_file() {
            Ok(path)
        } else {
            Err(not_found(id))
        }
    }

    /// What the file says, read again only where it changed.
    pub fn summary(&self, path: &Path) -> io::Result<(Summary, u64)> {
        let length = history::length(path)?;
        let modified = history::modified(path)?;
        if let Some((l, m, summary)) = self.lock().summaries.get(path)
            && (*l, *m) == (length, modified)
        {
            return Ok((summary.clone(), modified));
        }
        let summary = history::summary(path)?;
        self.lock()
            .summaries
            .insert(path.to_owned(), (length, modified, summary.clone()));
        Ok((summary, modified))
    }

    fn session(&self, path: &Path) -> io::Result<(Session, bool)> {
        let (summary, updated_at) = self.summary(path)?;
        Ok((
            Session {
                external_id: home::id(path),
                cwd: path.parent().map(Path::to_owned),
                title: summary.title,
                created_at: summary.created_at,
                updated_at,
                transcripts: vec![path.to_owned()],
                parent: None,
                app: false,
                sub_agents: Vec::new(),
            },
            summary.person,
        ))
    }

    /// `sessions/list`: every file, the ones a person was at as conversations, the others
    /// excluded.
    pub fn list(&self, home: &Path, asked: List) -> Listed {
        let mut sessions = Vec::new();
        let mut excluded = Vec::new();
        for path in self.files(home, &asked.transcripts) {
            let Ok((session, person)) = self.session(&path) else {
                continue;
            };
            if asked
                .seen_since
                .is_some_and(|since| session.updated_at < since)
            {
                continue;
            }
            if person {
                sessions.push(session);
            } else {
                excluded.push(session.external_id);
            }
        }
        sessions.sort_by_key(|session| session.updated_at);
        Listed {
            sessions,
            excluded,
            complete: true,
            cursor: None,
            after: None,
            gone: Vec::new(),
        }
    }

    /// `sessions/locate`.
    pub fn locate(&self, home: &Path, locate: Locate) -> Result<Located, Error> {
        let path = self.path(home, &locate.external_id)?;
        let (session, _) = self.session(&path).map_err(|e| unreadable(&path, &e))?;
        Ok(Located { session })
    }

    /// `sessions/read`. Aider runs no sub-agents.
    pub fn read(&self, home: &Path, read: Read) -> Result<Events, Error> {
        if let Some(sub) = read.sub_agent {
            return Err(Error::refused(
                refusal::NOT_FOUND,
                format!("Aider runs no sub-agents, so {sub} is not here"),
            ));
        }
        let path = self.path(home, &read.external_id)?;
        history::read(&path, read.cursor.as_deref(), read.tail, false)
            .map_err(|e| unreadable(&path, &e))
    }
}

fn not_found(id: &str) -> Error {
    Error::refused(
        refusal::NOT_FOUND,
        format!("no Aider chat history here has the id {id}"),
    )
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
