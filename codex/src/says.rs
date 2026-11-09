//! Steery's says a thread took (PROTOCOL.md §4.3, version 7): the turn that took each
//! say's words, and those turns once they ended. `session/changed` names them
//! ([`crate::router::Followers`]), and a `stop` naming a say stops only its turn.
//!
//! A say is tied to its turn where Codex named that turn as it took the words; else it
//! waits until a turn whose person input carries its delivery marker, or its words where
//! they carry none, is in the thread's history ([`native::taking`]).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use steery_sdk::wire::harness::LiveSettled;

use crate::appserver::AppServer;
use crate::native::{self, Words};

/// How many ended turns of says a thread keeps, oldest dropped first (PROTOCOL.md §4.3).
const SETTLED_MAX: usize = 16;
/// How many turns that took says a thread keeps.
const TIED_MAX: usize = 64;
/// How many says a thread keeps waiting for their turn.
const WAITING_MAX: usize = 64;
/// How many threads are kept; the one said to longest ago goes first.
const THREADS_MAX: usize = 256;

/// A say whose turn is not known yet.
#[derive(Debug, Clone)]
pub struct Waiting {
    pub say: String,
    pub words: Words,
    /// The thread's point before the words went, where it was read: words of no delivery
    /// are matched only past it ([`native::taking`]).
    pub before: Option<native::Point>,
    /// When the words went.
    pub at: u64,
}

/// What is known of the turn a say's words went into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taken {
    Turn(String),
    Waiting,
    Unknown,
}

/// A turn that took says.
struct Tied {
    turn: String,
    says: Vec<String>,
    began_at: u64,
    ended: bool,
}

#[derive(Default)]
struct Thread {
    waiting: Vec<Waiting>,
    tied: Vec<Tied>,
    /// The ended ones, oldest first, each under its turn.
    settled: Vec<(String, LiveSettled)>,
    /// A look for the waiting ones' turns runs.
    looking: bool,
    used: u64,
}

/// Every thread's says.
#[derive(Default)]
pub struct Says {
    threads: Mutex<HashMap<String, Thread>>,
}

impl Says {
    fn threads(&self) -> MutexGuard<'_, HashMap<String, Thread>> {
        self.threads.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn thread<'a>(threads: &'a mut HashMap<String, Thread>, thread: &str) -> &'a mut Thread {
        if !threads.contains_key(thread)
            && threads.len() >= THREADS_MAX
            && let Some(oldest) = threads
                .iter()
                .min_by_key(|(_, held)| held.used)
                .map(|(name, _)| name.clone())
        {
            threads.remove(&oldest);
        }
        let held = threads.entry(thread.to_owned()).or_default();
        held.used = crate::home::now();
        held
    }

    /// A say whose words went where Codex did not name the turn that took them.
    pub fn wait(&self, thread: &str, waiting: Waiting) {
        let mut threads = self.threads();
        let held = Self::thread(&mut threads, thread);
        held.waiting.retain(|other| other.say != waiting.say);
        if held.waiting.len() >= WAITING_MAX {
            held.waiting.remove(0);
        }
        held.waiting.push(waiting);
    }

    /// The say `say` went into `turn`, its words said at `at`; a say tied already stays
    /// with its turn. Whether anything changed.
    pub fn tie(&self, thread: &str, say: &str, turn: &str, at: u64) -> bool {
        let mut threads = self.threads();
        let held = Self::thread(&mut threads, thread);
        held.waiting.retain(|waiting| waiting.say != say);
        if held
            .tied
            .iter()
            .any(|tied| tied.says.iter().any(|named| named == say))
        {
            return false;
        }
        match held.tied.iter_mut().find(|tied| tied.turn == turn) {
            Some(tied) => {
                tied.says.push(say.to_owned());
                if let Some((_, settled)) = held.settled.iter_mut().find(|(of, _)| of == turn) {
                    settled.says.push(say.to_owned());
                }
            }
            None => {
                if held.tied.len() >= TIED_MAX {
                    held.tied.remove(0);
                }
                held.tied.push(Tied {
                    turn: turn.to_owned(),
                    says: vec![say.to_owned()],
                    began_at: at,
                    ended: false,
                });
            }
        }
        true
    }

    /// The turn the say `say` went into, as known now.
    pub fn taken(&self, thread: &str, say: &str) -> Taken {
        let threads = self.threads();
        let Some(held) = threads.get(thread) else {
            return Taken::Unknown;
        };
        if let Some(tied) = held
            .tied
            .iter()
            .find(|tied| tied.says.iter().any(|named| named == say))
        {
            Taken::Turn(tied.turn.clone())
        } else if held.waiting.iter().any(|waiting| waiting.say == say) {
            Taken::Waiting
        } else {
            Taken::Unknown
        }
    }

    /// The says waiting for their turn, oldest first.
    pub fn waiting(&self, thread: &str) -> Vec<Waiting> {
        self.threads()
            .get(thread)
            .map(|held| held.waiting.clone())
            .unwrap_or_default()
    }

    /// The says of the turns that took them and have not been seen to end: the turn
    /// running's, as Codex runs one turn of a thread at a time.
    pub fn running(&self, thread: &str) -> Vec<String> {
        self.threads()
            .get(thread)
            .map(|held| {
                held.tied
                    .iter()
                    .filter(|tied| !tied.ended)
                    .flat_map(|tied| tied.says.iter().cloned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `turn` ended at `at` (every turn not yet seen to end where none is named), begun
    /// at `began_at` where that is known better than when its words went. Whether
    /// anything settled.
    pub fn ended(
        &self,
        thread: &str,
        turn: Option<&str>,
        began_at: Option<u64>,
        at: u64,
        failed: bool,
        error: Option<String>,
    ) -> bool {
        let mut threads = self.threads();
        let Some(held) = threads.get_mut(thread) else {
            return false;
        };
        let mut settled = false;
        for tied in held
            .tied
            .iter_mut()
            .filter(|tied| !tied.ended && turn.is_none_or(|turn| turn == tied.turn))
        {
            tied.ended = true;
            settled = true;
            if held.settled.len() >= SETTLED_MAX {
                held.settled.remove(0);
            }
            held.settled.push((
                tied.turn.clone(),
                LiveSettled {
                    began_at: began_at.unwrap_or(tied.began_at),
                    at,
                    says: tied.says.clone(),
                    failed,
                    error: error.clone(),
                },
            ));
        }
        settled
    }

    /// The ended turns of says, oldest first.
    pub fn settled(&self, thread: &str) -> Vec<LiveSettled> {
        self.threads()
            .get(thread)
            .map(|held| {
                held.settled
                    .iter()
                    .map(|(_, settled)| settled.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether a look for the waiting says' turns starts now: some wait, and none runs.
    pub fn look(&self, thread: &str) -> bool {
        let mut threads = self.threads();
        match threads.get_mut(thread) {
            Some(held) if !held.waiting.is_empty() && !held.looking => {
                held.looking = true;
                true
            }
            _ => false,
        }
    }

    /// The look started by [`Says::look`] is over.
    pub fn looked(&self, thread: &str) {
        if let Some(held) = self.threads().get_mut(thread) {
            held.looking = false;
        }
    }
}

/// Tie each say waiting in `thread` to the turn `server` lists as carrying its words, and
/// settle the ones found ended. Whether anything changed.
pub async fn tie_waiting(
    says: &Says,
    server: &mut AppServer,
    thread: &str,
) -> Result<bool, String> {
    let waiting = says.waiting(thread);
    if waiting.is_empty() {
        return Ok(false);
    }
    let asked: Vec<(Option<&native::Point>, &Words)> = waiting
        .iter()
        .map(|waiting| (waiting.before.as_ref(), &waiting.words))
        .collect();
    let found = native::taking(server, thread, &asked).await?;
    let now = crate::home::now();
    let mut changed = false;
    for (waiting, found) in waiting.iter().zip(found) {
        let Some(found) = found else { continue };
        changed |= says.tie(thread, &waiting.say, &found.turn.id, waiting.at);
        if found.turn.ended() {
            changed |= says.ended(
                thread,
                Some(&found.turn.id),
                None,
                now,
                found.turn.status == "failed",
                found.error,
            );
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting(say: &str) -> Waiting {
        Waiting {
            say: say.into(),
            words: Words::Text("go".into()),
            before: None,
            at: 1,
        }
    }

    /// A say named with its turn is that turn's while it runs, and the turn's settled
    /// once it ends, with every say tied to it after; a waiting one is neither until
    /// tied. At most 16 settled are kept, the oldest dropped first.
    #[test]
    fn a_say_s_turn_names_it_while_it_runs_and_once_it_settled() {
        let says = Says::default();
        assert_eq!(says.taken("t", "s1"), Taken::Unknown);
        says.wait("t", waiting("s2"));
        assert_eq!(says.taken("t", "s2"), Taken::Waiting);
        assert!(says.tie("t", "s1", "u1", 5));
        assert!(!says.tie("t", "s1", "u2", 6), "a say stays with its turn");
        assert_eq!(says.running("t"), vec!["s1".to_owned()]);
        assert!(says.ended("t", None, Some(4), 9, true, Some("limit".into())));
        assert!(!says.ended("t", Some("u1"), None, 10, false, None));
        assert!(says.running("t").is_empty());
        assert!(says.tie("t", "s2", "u1", 7));
        assert_eq!(says.taken("t", "s2"), Taken::Turn("u1".into()));
        assert_eq!(
            says.settled("t"),
            vec![LiveSettled {
                began_at: 4,
                at: 9,
                says: vec!["s1".into(), "s2".into()],
                failed: true,
                error: Some("limit".into()),
            }]
        );
        for turn in 0..20 {
            let (say, turn) = (format!("say{turn}"), format!("turn{turn}"));
            says.tie("t", &say, &turn, 1);
            says.ended("t", Some(&turn), None, 2, false, None);
        }
        let settled = says.settled("t");
        assert_eq!(settled.len(), SETTLED_MAX);
        assert_eq!(settled[0].says, vec!["say4".to_owned()]);
        assert_eq!(settled[15].says, vec!["say19".to_owned()]);
    }
}
