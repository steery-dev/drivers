//! Steery's hooks on Hermes, in a turn this driver runs ([`crate::turn::hooks`]). Hermes asks
//! before an edit, under a call id of its own (`edit-approval-<n>`), and names the tool and
//! its arguments in the request's `rawInput` (`{tool, arguments}`), before the call is a row
//! of its store.

use std::path::Path;

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{HookMoment, HookRead};

use super::store::Home;
use super::{calls, mcp};
use crate::turn::hooks::{self, ENDED, PERMISSION};

/// Hermes offers no `allow_always` at an edit, so this says nothing a person reads.
const ALWAYS: &str = "every later call of this tool";

/// The moment of the hook's `event`, or none where it is no moment Steery carries.
pub fn read(root: &Path, read: HookRead) -> Result<HookMoment, Error> {
    if ![PERMISSION, ENDED].contains(&read.event.as_str()) {
        return Ok(HookMoment { moment: None });
    }
    let mut document = hooks::parse(&read.document)?;
    let Some(said) = hooks::text(&document, "/sessionId") else {
        return Ok(HookMoment { moment: None });
    };
    // A turn goes on in the chain's newest session; Steery knows the conversation by its
    // first.
    let Some(standing) = Home::new(root).standing(said)? else {
        return Ok(HookMoment { moment: None });
    };
    document["sessionId"] = Value::String(standing.external_id.clone());
    if read.event == ENDED {
        return Ok(hooks::ended(&document, standing.cwd));
    }
    let (id, kind, input) = hooks::requested(&document);
    let (tool, arguments) = match input.get("tool").and_then(Value::as_str) {
        Some(tool) => (tool.to_owned(), input["arguments"].clone()),
        None => (kind, input),
    };
    let entry = json!({"id": id, "function": {"name": tool, "arguments": arguments}});
    let call = calls::call(&entry, None, &mcp::names(root))
        .ok_or_else(|| Error::invalid("the permission names no call"))?;
    Ok(hooks::asked(
        &standing.external_id,
        standing.cwd,
        call,
        &document,
        ALWAYS,
    ))
}
