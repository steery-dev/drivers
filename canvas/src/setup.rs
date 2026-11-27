//! `setup/plan`: the canvas is this driver's own turn mode, so it is here wherever the driver
//! is, and installs nothing. It reads no MCP server, skill or hooks. Its one entry is
//! Steery's door in `<CANVAS_HOME>/steery.json`, which each turn dials at its start. Where
//! `needs` (protocol 5) and the check of [`crate::claude`] says Claude does not answer, the
//! plan asks the sign-in ([`crate::sign_in`]) with the check's line; where the person asked
//! to give it again (protocol 15), it asks it as a change.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use serde_json::json;
use steery_driver_common::program;
use steery_driver_common::signed::{Known, sure};
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

use crate::claude::Source;
use crate::{home, sign_in};

/// The last answer of the sign-in probe in this process. A probe with no answer uses it.
pub(crate) static SIGNED: Known = Known::new();

/// The plan for this host, under the protocol picked.
pub async fn plan(home: &Path, root: &Path, setup: Setup, protocol: u32) -> Planned {
    let binary = match std::env::current_exe() {
        Ok(binary) => binary,
        Err(e) => {
            return Planned {
                found: None,
                refused: Some(format!("The canvas cannot find its own driver: {e}")),
                entries: Vec::new(),
                notes: Vec::new(),
                install: None,
                needs: Vec::new(),
            };
        }
    };
    let mut needs = Vec::new();
    if protocol >= program::NEEDS {
        let why = Mutex::new(None);
        let now = sure(|| async {
            let notice = Source::read().check().await;
            let signed_in = notice.is_none();
            *why.lock().unwrap_or_else(PoisonError::into_inner) = notice;
            Some(signed_in)
        })
        .await;
        let claude = home::claude(home).is_some();
        let asked = program::asks(protocol, &setup.ask, sign_in::NEED);
        match (SIGNED.steady(now) == Some(false), asked) {
            (true, _) => needs.push(sign_in::need(
                why.into_inner().unwrap_or_else(PoisonError::into_inner),
                claude,
            )),
            (false, true) => needs.push(sign_in::change(claude)),
            (false, false) => {}
        }
    }
    Planned {
        found: Some(Found {
            binary,
            release: env!("STEERY_PROGRAM_RELEASE").into(),
            root: root.to_owned(),
        }),
        refused: None,
        entries: vec![Entry {
            path: root.join("steery.json"),
            format: Format::Json,
            at: Some("/door".into()),
            value: json!(setup.door),
            carries: vec![Part::Plugin],
        }],
        notes: vec![
            "The canvas is this driver's own headless harness. It reads no MCP server, skill \
             or hooks. Steery writes only its door to steery.json. The canvas runs on Claude: \
             a Claude plan through Claude Code, an Anthropic API key, or a gateway."
                .to_owned(),
        ],
        install: None,
        needs,
    }
}
