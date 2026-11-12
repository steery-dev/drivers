//! Steery's hooks on OpenClaw, in a turn this driver runs ([`crate::turn::hooks`]).
//! OpenClaw's gateway asks before a command runs, and `openclaw acp` relays that approval
//! as a permission whose `rawInput` names the tool (`name`, `exec`) and the command
//! (`buildAcpPermissionRequest`), under the call's own id.
use std::path::Path;

use serde_json::{Value, json};
use steery_sdk::Error;
use steery_sdk::wire::harness::{HookMoment, HookRead};

use super::events;
use super::store::Store;
use crate::turn::hooks::{self, ENDED, PERMISSION};

/// What OpenClaw's `allow-always` keeps: the command, in the gateway's exec allowlist.
const ALWAYS: &str = "this command, in OpenClaw's exec allowlist";

/// The moment of the hook's `event`, or none where it is no moment Steery carries.
pub fn read(root: &Path, read: HookRead) -> Result<HookMoment, Error> {
    if ![PERMISSION, ENDED].contains(&read.event.as_str()) {
        return Ok(HookMoment { moment: None });
    }
    let mut document = hooks::parse(&read.document)?;
    let store = Store::new(root);
    let Some(conversation) =
        hooks::text(&document, "/sessionId").and_then(|acp| store.conversation(acp))
    else {
        return Ok(HookMoment { moment: None });
    };
    let cwd = store
        .standing(&conversation)?
        .and_then(|standing| standing.cwd);
    document["sessionId"] = Value::String(conversation.clone());
    if read.event == ENDED {
        return Ok(hooks::ended(&document, cwd));
    }
    let (id, kind, mut input) = hooks::requested(&document);
    let tool = input
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or(kind);
    if let Some(object) = input.as_object_mut() {
        object.remove("name");
        object.remove("approvalId");
    }
    let part = json!({"id": id, "name": tool, "arguments": input});
    let call = events::call(&part, None, &store.servers())
        .ok_or_else(|| Error::invalid("the permission names no call"))?;
    Ok(hooks::asked(&conversation, cwd, call, &document, ALWAYS))
}
