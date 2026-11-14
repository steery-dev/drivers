//! Steery's hooks on OpenHands, in a turn this driver runs ([`crate::turn::hooks`]).
//! OpenHands asks its ACP client once for the batch of actions its agent wrote, not per
//! call: the request's `toolCall` is `confirmation-<session>`, "Confirm Agent Actions"
//! (`openhands_cli/acp_impl/confirmation.py`), and the answer accepts or rejects them all.
//! The moment's call is the first of those actions, the oldest `ActionEvent` in the
//! conversation's events that no observation answered yet, which OpenHands wrote before it
//! asked.

use serde_json::Value;
use steery_sdk::Error;
use steery_sdk::wire::harness::{HookMoment, HookRead};

use super::events::{self, Record};
use super::mcp;
use super::place::Place;
use super::store::Store;
use crate::turn::hooks::{self, ENDED, PERMISSION};

/// What OpenHands' `always_proceed` keeps: it stops asking for the rest of the conversation
/// (`NeverConfirm`).
const ALWAYS: &str = "every later call in this conversation: OpenHands stops asking";

/// The moment of the hook's `event`, or none where it is no moment Steery carries.
pub fn read(place: &Place, read: HookRead) -> Result<HookMoment, Error> {
    if ![PERMISSION, ENDED].contains(&read.event.as_str()) {
        return Ok(HookMoment { moment: None });
    }
    let document = hooks::parse(&read.document)?;
    let Some(external_id) = hooks::text(&document, "/sessionId") else {
        return Ok(HookMoment { moment: None });
    };
    let store = Store::new(&place.conversations);
    let (cwd, records) = match store.conversation(external_id) {
        Ok((_, head)) => (
            head.cwd,
            store
                .records(external_id)
                .map(|(_, records)| records)
                .unwrap_or_default(),
        ),
        Err(_) => (None, Vec::new()),
    };
    if read.event == ENDED {
        return Ok(hooks::ended(&document, cwd));
    }
    let servers = mcp::names(place);
    let call = match pending(&records) {
        Some(action) => events::call(
            action["tool_call_id"].as_str().unwrap_or_default(),
            action["tool_name"].as_str().unwrap_or("other"),
            events::input(action),
            action["action"]["kind"].as_str(),
            action["llm_response_id"].as_str(),
            &servers,
        ),
        None => {
            let (id, kind, input) = hooks::requested(&document);
            events::call(id, &kind, input, None, None, &servers)
        }
    };
    Ok(hooks::asked(external_id, cwd, call, &document, ALWAYS))
}

/// The first action of the batch the agent wrote last, where nothing answered it yet: the
/// actions after the newest record that is neither an action nor a hook's run.
fn pending(records: &[Record]) -> Option<&Value> {
    records
        .iter()
        .rev()
        .map(|record| &record.value)
        .filter(|value| value["kind"] != "HookExecutionEvent")
        .take_while(|value| value["kind"] == "ActionEvent")
        .last()
}
