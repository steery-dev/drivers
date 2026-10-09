//! Who owns a Codex thread natively, and what its turns are.
//!
//! Two owners take words for a thread and stop its turn themselves: the window of the
//! Codex app that owns it, through the app's router ([`crate::router`]); and an app server
//! already running for the thread's home that has it loaded with nothing queued, reached
//! through `codex app-server proxy` (its control socket, which Codex finds itself). A
//! socket that answers is no proof of ownership: the home it says it serves,
//! `thread/loaded/list` and `thread/queue/list` must all agree first.
//!
//! What a thread's turns are is asked of an app server of the driver's own, in the
//! thread's home (`thread/turns/list`), which loads nothing and writes nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::appserver::{AppServer, Failed};

/// How long one look at a thread has.
pub const WITHIN: Duration = Duration::from_secs(8);
const LOADED_PAGES_MAX: usize = 100;
const TURN_PAGES_MAX: usize = 200;

/// The owner of a thread that runs its turns, as `held` gives it to the runtime and
/// `stop` and `receipt/find` give it back: the window of the app, or a running app
/// server's queue in a home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Owner {
    App {
        thread: String,
        turn: String,
    },
    Queue {
        home: PathBuf,
        thread: String,
        turn: String,
    },
}

impl Owner {
    pub fn token(&self) -> String {
        serde_json::to_string(self).expect("an owner is JSON")
    }

    pub fn parse(token: &str) -> Option<Self> {
        serde_json::from_str(token).ok()
    }
}

/// One turn as Codex lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub id: String,
    pub status: String,
}

impl Turn {
    pub fn ended(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "interrupted" | "failed")
    }

    pub fn running(&self) -> bool {
        self.status == "inProgress"
    }
}

fn read<T>(result: Result<T, Failed>) -> Result<T, String> {
    result.map_err(|e| e.to_string())
}

/// An app server of the driver's own in `home`, its home checked.
pub async fn reader(binary: &Path, home: &Path) -> Result<AppServer, String> {
    let mut command = crate::home::helper(binary);
    command.env("CODEX_HOME", home);
    let server = read(AppServer::open(command, false).await)?;
    same_home(&server, home)?;
    Ok(server)
}

/// The app server already running for `home`, where one owns `thread`: loaded there, its
/// queue readable.
pub async fn queue(binary: &Path, home: &Path, thread: &str) -> Result<AppServer, String> {
    let mut command = crate::home::helper(binary);
    command.env("CODEX_HOME", home);
    let mut server = read(AppServer::running(command).await)?;
    same_home(&server, home)?;
    if !loaded(&mut server).await?.contains(thread) {
        return Err("the running Codex app server has not loaded this thread".into());
    }
    queue_empty(&mut server, thread).await?;
    Ok(server)
}

fn same_home(server: &AppServer, home: &Path) -> Result<(), String> {
    let said = server.home().ok_or("Codex did not say its home")?;
    let canonical = |path: &Path| path.canonicalize().ok();
    if said.is_absolute() && canonical(said).is_some() && canonical(said) == canonical(home) {
        Ok(())
    } else {
        Err(format!("this Codex serves {}", said.display()))
    }
}

async fn loaded(server: &mut AppServer) -> Result<HashSet<String>, String> {
    let mut cursor: Option<String> = None;
    let mut cursors = HashSet::new();
    let mut threads = HashSet::new();
    for _ in 0..LOADED_PAGES_MAX {
        let page = read(
            server
                .call(
                    "thread/loaded/list",
                    json!({"cursor": cursor, "limit": 100}),
                )
                .await,
        )?;
        for thread in page
            .get("data")
            .and_then(Value::as_array)
            .ok_or("Codex's loaded page has no data")?
        {
            let thread = thread
                .as_str()
                .ok_or("Codex's loaded thread is no string")?;
            threads.insert(thread.to_owned());
        }
        match page.get("nextCursor") {
            Some(Value::Null) | None => return Ok(threads),
            Some(Value::String(next)) if !next.is_empty() && cursors.insert(next.clone()) => {
                cursor = Some(next.clone());
            }
            _ => return Err("Codex's loaded pages did not advance".into()),
        }
    }
    Err("Codex's loaded pages exceeded their bound".into())
}

/// Whether nothing waits in the thread's queue.
pub async fn queue_empty(server: &mut AppServer, thread: &str) -> Result<bool, String> {
    let page = read(
        server
            .call("thread/queue/list", json!({"threadId": thread, "limit": 1}))
            .await,
    )?;
    let data = page
        .get("data")
        .and_then(Value::as_array)
        .ok_or("Codex's queue page has no data")?;
    Ok(data.is_empty() && page.get("nextCursor").is_none_or(Value::is_null))
}

/// The newest turn of a thread.
pub async fn latest(server: &mut AppServer, thread: &str) -> Result<Option<Turn>, String> {
    let page = read(
        server
            .call(
                "thread/turns/list",
                json!({"threadId": thread, "sortDirection": "desc", "limit": 1, "itemsView": "notLoaded"}),
            )
            .await,
    )?;
    let turns = page
        .get("data")
        .and_then(Value::as_array)
        .ok_or("Codex's turn page has no data")?;
    turns.first().map(turn).transpose()
}

fn turn(turn: &Value) -> Result<Turn, String> {
    Ok(Turn {
        id: turn
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or("a Codex turn has no id")?
            .to_owned(),
        status: turn
            .get("status")
            .and_then(Value::as_str)
            .ok_or("a Codex turn has no status")?
            .to_owned(),
    })
}

/// The turns of a thread, newest first, page by page, until `stop` says enough.
async fn pages(
    server: &mut AppServer,
    thread: &str,
    items: &str,
    mut each: impl FnMut(&Value) -> Result<bool, String>,
) -> Result<bool, String> {
    let mut cursor: Option<String> = None;
    let mut cursors = HashSet::new();
    let mut seen = HashSet::new();
    for _ in 0..TURN_PAGES_MAX {
        let page = read(
            server
                .call(
                    "thread/turns/list",
                    json!({"threadId": thread, "sortDirection": "desc", "limit": 50,
                           "itemsView": items, "cursor": cursor}),
                )
                .await,
        )?;
        for value in page
            .get("data")
            .and_then(Value::as_array)
            .ok_or("Codex's turn page has no data")?
        {
            let found = turn(value)?;
            if !seen.insert(found.id) {
                return Err("Codex listed a turn twice".into());
            }
            if each(value)? {
                return Ok(true);
            }
        }
        match page.get("nextCursor") {
            Some(Value::String(next)) if !next.is_empty() && cursors.insert(next.clone()) => {
                cursor = Some(next.clone());
            }
            Some(Value::Null) | None => return Ok(false),
            _ => return Err("Codex's turn pages did not advance".into()),
        }
    }
    Err("Codex's turn pages exceeded their bound".into())
}

/// One turn of a thread by its id.
pub async fn find(
    server: &mut AppServer,
    thread: &str,
    expected: &str,
) -> Result<Option<Turn>, String> {
    let mut found = None;
    pages(server, thread, "notLoaded", |value| {
        let listed = turn(value)?;
        if listed.id == expected {
            found = Some(listed);
            return Ok(true);
        }
        Ok(false)
    })
    .await?;
    Ok(found)
}

/// The one turn after `before` (the newest when it is none) whose person input carries the
/// delivery marker whole. Two carrying it is an error: the words went in twice.
pub async fn carrying(
    server: &mut AppServer,
    thread: &str,
    before: Option<&str>,
    marker: &str,
) -> Result<Option<String>, String> {
    let mut matches = Vec::new();
    let reached = pages(server, thread, "full", |value| {
        let id = turn(value)?.id;
        if Some(id.as_str()) == before {
            return Ok(true);
        }
        if carries(value, marker)? {
            matches.push(id);
        }
        Ok(false)
    })
    .await?;
    if before.is_some() && !reached {
        return Err("the turn marked before is gone from Codex's history".into());
    }
    match matches.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(one.clone())),
        _ => Err("more than one Codex turn carries the delivery marker".into()),
    }
}

fn carries(turn: &Value, marker: &str) -> Result<bool, String> {
    if turn.get("itemsView").and_then(Value::as_str) != Some("full") {
        return Err("Codex did not load the turn's items".into());
    }
    let items = turn
        .get("items")
        .and_then(Value::as_array)
        .ok_or("a Codex turn has no items")?;
    Ok(items.iter().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("userMessage")
            && item
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .any(|text| marked(text, marker))
    }))
}

/// Whether `text` holds the delivery marker whole: the character after it is no letter,
/// digit, `.`, `-` or `_` (PROTOCOL.md §3.3).
pub fn marked(text: &str, marker: &str) -> bool {
    text.match_indices(marker).any(|(at, _)| {
        text[at + marker.len()..]
            .chars()
            .next()
            .is_none_or(|next| !(next.is_alphanumeric() || matches!(next, '.' | '-' | '_')))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delivery_marker_matches_only_whole() {
        assert!(marked("words steery-delivery:abc", "steery-delivery:abc"));
        assert!(marked("steery-delivery:abc) more", "steery-delivery:abc"));
        assert!(!marked("steery-delivery:abcd", "steery-delivery:abc"));
        assert!(!marked("steery-delivery:abc.1", "steery-delivery:abc"));
        assert!(marked(
            "steery-delivery:abc-x steery-delivery:abc",
            "steery-delivery:abc"
        ));
    }

    #[test]
    fn an_owner_token_says_itself_back() {
        let owner = Owner::Queue {
            home: "/h/.codex".into(),
            thread: "t".into(),
            turn: "u".into(),
        };
        assert_eq!(Owner::parse(&owner.token()), Some(owner));
        assert_eq!(Owner::parse("{\"app\":{\"thread\":\"t\"}}"), None);
    }

    #[test]
    fn a_turn_s_items_say_whether_it_carries_the_words() {
        let full = json!({"id": "t1", "status": "completed", "itemsView": "full", "items": [
            {"type": "userMessage", "content": [{"type": "text", "text": "do it steery-delivery:d1"}]}]});
        assert_eq!(carries(&full, "steery-delivery:d1"), Ok(true));
        assert_eq!(carries(&full, "steery-delivery:d2"), Ok(false));
        assert!(carries(&json!({"id": "t1", "itemsView": "notLoaded"}), "x").is_err());
    }
}
