//! `setup/plan`: the canvas reads no MCP server, skill or hooks. Its one entry is Steery's
//! door in `<CANVAS_HOME>/steery.json`, which `canvas turn` dials at its start. Where
//! `needs` (protocol 5) and `canvas models` says it is not signed in, the plan asks the
//! sign-in ([`crate::sign_in`]) with the canvas's own line; where the person asked to give
//! it again (protocol 15), it asks it as a change.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use steery_driver_common::program::{self, release_within, version};
use steery_driver_common::signed::{Known, sure};
use serde_json::json;
use steery_sdk::wire::harness::{Entry, Format, Found, Part, Planned, Setup};

use crate::{home, models, sign_in};

/// The last answer of the sign-in probe in this process. A probe with no answer uses it.
pub(crate) static SIGNED: Known = Known::new();

/// The plan for this host, under the protocol picked.
pub async fn plan(home: &Path, root: &Path, setup: Setup, protocol: u32) -> Planned {
    let Some(binary) = home::binary() else {
        return Planned {
            found: None,
            refused: Some(
                "Steery has no installer for the canvas: put its `canvas` launcher on PATH".into(),
            ),
            entries: Vec::new(),
            notes: vec!["Not installed here. The driver found no `canvas` on PATH.".into()],
            install: None,
            needs: Vec::new(),
        };
    };
    let release = version(&binary)
        .await
        .as_deref()
        .and_then(release_within)
        .map(|(major, minor, patch)| format!("{major}.{minor}.{patch}"));
    let mut needs = Vec::new();
    if protocol >= program::NEEDS {
        let why = Mutex::new(None);
        let now = sure(|| async {
            let said = models::ask(&binary).await;
            match said {
                Ok(said) => {
                    *why.lock().unwrap_or_else(PoisonError::into_inner) = said.line;
                    Some(said.signed_in)
                }
                Err(e) => {
                    eprintln!("{e}");
                    None
                }
            }
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
    let mut notes = vec![
        "The canvas reads no MCP server, skill or hooks. Steery writes only its door to \
         steery.json. The canvas runs on Claude: a Claude plan through Claude Code, an \
         Anthropic API key, or a gateway."
            .to_owned(),
    ];
    if release.is_none() {
        notes.push(
            "`--version` named no release, so the driver says nothing about this build.".into(),
        );
    }
    Planned {
        found: Some(Found {
            binary,
            release: release.unwrap_or_default(),
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
        notes,
        install: None,
        needs,
    }
}
