//! Steery's hooks on Kilo, in a turn this driver runs ([`crate::turn::hooks`]): the call of
//! a permission is the one `kilo.db` keeps.

use steery_sdk::Error;
use steery_sdk::wire::harness::{HookMoment, HookRead};

use super::store::Store;
use super::{calls, config};
use crate::turn::hooks::{self, ENDED, PERMISSION};

/// What an `allow_always` of Kilo's keeps.
const ALWAYS: &str = "calls like it, everywhere: Kilo saves the rule in your global configuration";

/// The moment of the hook's `event`, or none where it is no moment Steery carries. The call
/// of a permission is the one Kilo's database keeps under the request's `toolCallId`, which
/// is the call's `callID`. A request with no tool behind it (Kilo then sends the
/// permission's own id) is named by its ACP `kind`.
pub fn read(store: &Store, read: HookRead) -> Result<HookMoment, Error> {
    if ![PERMISSION, ENDED].contains(&read.event.as_str()) {
        return Ok(HookMoment { moment: None });
    }
    let document = hooks::parse(&read.document)?;
    let Some(external_id) = hooks::text(&document, "/sessionId") else {
        return Ok(HookMoment { moment: None });
    };
    let cwd = store.directory(external_id).ok().flatten();
    match read.event.as_str() {
        PERMISSION => {
            let (id, kind, input) = hooks::requested(&document);
            let (tool, input) = store.tool(external_id, id)?.unwrap_or((kind, input));
            let servers = config::servers(store.config(), cwd.as_deref());
            let call = calls::call(id, &tool, input, &servers);
            Ok(hooks::asked(external_id, cwd, call, &document, ALWAYS))
        }
        _ => Ok(hooks::ended(&document, cwd)),
    }
}
