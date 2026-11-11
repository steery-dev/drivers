//! Steery's hooks on Cline, in a turn this driver runs ([`crate::turn::hooks`]): the call of
//! a permission is the `tool_use` block under the request's `toolCallId` in the
//! conversation's messages, where Cline wrote it already.

use serde_json::Value;
use steery_sdk::Error;
use steery_sdk::wire::harness::{HookMoment, HookRead};

use super::place::Place;
use super::store::Store;
use super::{config, events};
use crate::turn::hooks::{self, ENDED, PERMISSION};

/// What an `allow_always` of Cline's keeps: Cline reads it as an `allow_once`.
const ALWAYS: &str = "this call only: Cline keeps no rule for it";

/// The moment of the hook's `event`, or none where it is no moment Steery carries. A
/// request whose call Cline has not written yet is named by its ACP `kind`.
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
    let row = store.row(external_id).ok().flatten();
    let cwd = row.as_ref().map(|row| row.cwd.clone());
    if read.event != PERMISSION {
        return Ok(hooks::ended(&document, cwd));
    }
    let (id, kind, input) = hooks::requested(&document);
    let used = row
        .as_ref()
        .and_then(|row| Store::messages(row).ok())
        .and_then(|messages| tool_use(&messages, id));
    let (tool, input) = used.unwrap_or((kind, input));
    let servers = config::server_names(place);
    let call = events::call(id, &tool, input, external_id, &servers);
    Ok(hooks::asked(external_id, cwd, call, &document, ALWAYS))
}

/// The name and input of the `tool_use` block `id` in the messages.
fn tool_use(messages: &[Value], id: &str) -> Option<(String, Value)> {
    messages
        .iter()
        .rev()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "tool_use" && block["id"] == id)
        .map(|block| {
            (
                block["name"].as_str().unwrap_or_default().to_owned(),
                block["input"].clone(),
            )
        })
}
