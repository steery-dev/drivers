//! Steery's hooks on Goose, in a turn this driver runs ([`crate::turn::hooks`]): the call of
//! a permission is the `toolRequest` block under the request's `toolCallId` in the
//! conversation's messages, where Goose wrote it already (`acp/server.rs:1520-1535`).

use serde_json::Value;
use steery_sdk::Error;
use steery_sdk::wire::harness::{HookMoment, HookRead};

use super::events::Message;
use super::place::Place;
use super::store::Store;
use super::{config, events};
use crate::turn::hooks::{self, ENDED, PERMISSION};

/// What an `allow_always` of Goose's keeps: a rule for the tool in its `permission.yaml`
/// (`agents/state_machine/tool_approval.rs:108-126`).
const ALWAYS: &str = "every later call of this tool: Goose keeps it as a rule";

/// The moment of the hook's `event`, or none where it is no moment Steery carries. A
/// request whose call Goose has not written yet is named by its ACP `kind`.
pub fn read(place: &Place, read: HookRead) -> Result<HookMoment, Error> {
    if ![PERMISSION, ENDED].contains(&read.event.as_str()) {
        return Ok(HookMoment { moment: None });
    }
    let document = hooks::parse(&read.document)?;
    let Some(external_id) = hooks::text(&document, "/sessionId") else {
        return Ok(HookMoment { moment: None });
    };
    let db = place.db();
    let store = Store::new(&db);
    let cwd = store.directory(external_id).ok().flatten();
    if read.event == ENDED {
        return Ok(hooks::ended(&document, cwd));
    }
    let (id, kind, input) = hooks::requested(&document);
    let messages = store.messages(external_id).unwrap_or_default();
    let servers = config::server_names(place);
    let call = match tool_request(&messages, id) {
        Some((message, block)) => {
            let requested = &block["toolCall"]["value"];
            events::call(
                id,
                requested["name"].as_str().unwrap_or(&kind),
                requested["arguments"].clone(),
                block
                    .pointer("/_meta/goose.acp.kind")
                    .and_then(Value::as_str),
                &message.id,
                &servers,
            )
        }
        None => events::call(id, &kind, input, Some(&kind), external_id, &servers),
    };
    Ok(hooks::asked(external_id, cwd, call, &document, ALWAYS))
}

/// The `toolRequest` block `id` in the messages, with the message that holds it.
fn tool_request<'a>(messages: &'a [Message], id: &str) -> Option<(&'a Message, &'a Value)> {
    messages.iter().rev().find_map(|message| {
        message
            .content
            .as_array()?
            .iter()
            .find(|block| block["type"] == "toolRequest" && block["id"] == id)
            .map(|block| (message, block))
    })
}
