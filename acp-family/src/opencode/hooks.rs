//! Steery's hooks on OpenCode, in a turn this driver runs ([`crate::turn::hooks`]): the call
//! of a permission is the one OpenCode's database keeps.

use steery_sdk::Error;
use steery_sdk::wire::harness::{HookMoment, HookRead};

use super::place::Place;
use super::store::Store;
use super::{config, events};
use crate::turn::hooks::{self, ENDED, PERMISSION};

/// What an `allow_always` of OpenCode's keeps.
const ALWAYS: &str = "calls like it in this project: OpenCode saves the rule for the project";

/// The moment of the hook's `event`, or none where it is no moment Steery carries. The call
/// of a permission is the one OpenCode keeps under the request's `toolCallId`; a sub-agent's
/// call is named `<sub-agent's session>:<call>`. A request with no call kept is named by its
/// ACP `kind`.
pub fn read(place: &Place, read: HookRead) -> Result<HookMoment, Error> {
    if ![PERMISSION, ENDED].contains(&read.event.as_str()) {
        return Ok(HookMoment { moment: None });
    }
    let document = hooks::parse(&read.document)?;
    let Some(external_id) = hooks::text(&document, "/sessionId") else {
        return Ok(HookMoment { moment: None });
    };
    let store = Store::new(&place.db);
    let cwd = store.directory(external_id).ok().flatten();
    if read.event != PERMISSION {
        return Ok(hooks::ended(&document, cwd));
    }
    let (id, kind, input) = hooks::requested(&document);
    let (session, call) = match id.split_once(':') {
        Some((session, call)) if session.starts_with("ses_") => (session, call),
        _ => (external_id, id),
    };
    let servers = cwd
        .as_deref()
        .map(|cwd| config::server_names(place, Some(cwd)))
        .unwrap_or_default();
    let call = match store.tool(session, call)? {
        Some((tool, input, message)) => events::call(id, &tool, input, &message, &servers),
        None => events::call(id, &kind, input, external_id, &servers),
    };
    Ok(hooks::asked(external_id, cwd, call, &document, ALWAYS))
}
