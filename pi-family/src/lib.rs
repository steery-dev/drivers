//! Steery's drivers for the pi family, `omp` and `pi`, at observe, drive and
//! live. They read a harness's conversations from their transcripts, plan the entries that
//! set it up for Steery, read its moments, and say how to start, read and resume a turn. A
//! live conversation is reached through Steery's plugin door, which setup writes into the
//! harness and the runtime speaks directly: so a driver answers no live method.
//!
//! One library, two binaries: what tells the harnesses apart is [`Harness`], and every
//! module asks it.

mod calls;
mod drive;
mod events;
mod harness;
mod hooks;
mod models;
mod quiet;
mod setup;
mod transcripts;
mod tree;

use std::path::PathBuf;
use std::sync::OnceLock;

use serde_json::Value;
use steery_sdk::wire;
use steery_sdk::{Error, PROTOCOL, Program, method, refusal, result};

pub use harness::Harness;

/// Serve the driver for `harness` on standard input and output.
pub async fn serve(harness: Harness) -> std::io::Result<()> {
    steery_sdk::serve(move |_notifier| Driver::new(harness)).await
}

/// The driver: the harness it speaks for, where that keeps everything, from
/// `initialize`, and the models it listed last.
struct Driver {
    harness: Harness,
    place: OnceLock<Place>,
    catalog: models::Catalog,
}

/// The person's home, the runtime's config folder, and the harness's conversations under
/// its agent folder, or why there is none.
struct Place {
    home: PathBuf,
    config: PathBuf,
    store: Result<transcripts::Store, String>,
}

impl Driver {
    fn new(harness: Harness) -> Self {
        Self {
            harness,
            place: OnceLock::new(),
            catalog: models::Catalog::default(),
        }
    }

    fn place(&self) -> Result<&Place, Error> {
        self.place
            .get()
            .ok_or_else(|| Error::invalid("`initialize` came first"))
    }

    fn store(&self) -> Result<&transcripts::Store, Error> {
        self.place()?
            .store
            .as_ref()
            .map_err(|why| Error::refused(refusal::REFUSED, why.clone()))
    }
}

impl Program for Driver {
    fn initialize(&self, params: &wire::InitializeParams) -> Result<wire::Initialized, Error> {
        if !params.protocol.contains(&PROTOCOL) {
            return Err(Error::invalid(format!(
                "this driver speaks protocol {PROTOCOL} only"
            )));
        }
        let harness = self.harness;
        let _ = self.place.set(Place {
            home: params.home.clone(),
            config: params.config.clone(),
            store: harness.root(&params.home).map(|root| {
                let chosen = std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
                    .filter(|dir| !dir.is_empty())
                    .map(PathBuf::from);
                transcripts::Store::new(harness, &params.home, root, chosen)
            }),
        });
        Ok(wire::Initialized {
            protocol: PROTOCOL,
            program: wire::ProgramInfo {
                id: harness.id().parse().map_err(Error::invalid)?,
                kind: wire::Kind::Harness,
                release: env!("CARGO_PKG_VERSION").into(),
                label: harness.label().into(),
                short: harness.short().into(),
                tint: Some(harness.tint().into()),
                mark: None,
            },
            harness: Some(Box::new(harness.info())),
            machine: None,
            settings: Vec::new(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, Error> {
        let harness = self.harness;
        match method {
            method::SETUP_PLAN => {
                let place = self.place()?;
                result(
                    setup::plan(
                        harness,
                        &place.home,
                        &place.config,
                        steery_sdk::params(params)?,
                    )
                    .await?,
                )
            }
            method::SESSIONS_LIST => result(self.store()?.list(steery_sdk::params(params)?)),
            method::SESSIONS_LOCATE => result(self.store()?.locate(steery_sdk::params(params)?)?),
            method::SESSIONS_READ => result(self.store()?.read(steery_sdk::params(params)?)?),
            method::HOOK_READ => result(hooks::read(self.store()?, steery_sdk::params(params)?)?),
            method::HELD => result(drive::held(self.store()?, steery_sdk::params(params)?)),
            method::IDLE => result(drive::idle(self.store()?, steery_sdk::params(params)?)),
            method::MODELS => {
                let _: wire::Empty = steery_sdk::params(params)?;
                let place = self.place()?;
                result(
                    self.catalog
                        .list(harness, &place.home, self.store()?.root())
                        .await?,
                )
            }
            method::START => result(
                drive::start(self.store()?, &self.catalog, steery_sdk::params(params)?).await?,
            ),
            method::TURN_READ => result(drive::read(harness, steery_sdk::params(params)?)),
            method::RECEIPT_MARK => {
                result(drive::mark(self.store()?, steery_sdk::params(params)?)?)
            }
            method::RECEIPT_FIND => result(drive::find(steery_sdk::params(params)?)?),
            method::READER => result(drive::reader(harness, steery_sdk::params(params)?)?),
            method::TERMINAL => result(drive::terminal(
                &self.place()?.home,
                self.store()?,
                steery_sdk::params(params)?,
            )?),
            // The moments come over the plugin door, so no hook is a command to answer;
            // `stop`, `say`, `pick` and `recap` are the door's, which the runtime speaks
            // itself; a reader's output is its answer whole.
            _ => Err(Error::Unknown),
        }
    }
}
