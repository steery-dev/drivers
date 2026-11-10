//! The harnesses of the ACP family, and what names each on a person's tile.

/// A harness of the ACP family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    /// xAI's Grok Build, whose executable is `grok`.
    Grok,
    /// GitHub Copilot CLI, whose executable is `copilot`.
    Copilot,
}

impl Harness {
    /// The driver's id, which is the harness's executable.
    pub fn id(self) -> &'static str {
        match self {
            Self::Grok => "grok",
            Self::Copilot => "copilot",
        }
    }

    /// Its name as a person reads it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Grok => "Grok Build",
            Self::Copilot => "Copilot",
        }
    }

    /// Its name on a tile.
    pub fn short(self) -> &'static str {
        match self {
            Self::Grok => "Gk",
            Self::Copilot => "Co",
        }
    }

    /// Its colour on a tile.
    pub fn tint(self) -> &'static str {
        match self {
            Self::Grok => "#1d1d1f",
            Self::Copilot => "#8534f3",
        }
    }

    /// What it says of itself in `initialize`, beside its harness facts.
    pub(crate) fn program(self) -> Result<steery_sdk::wire::ProgramInfo, steery_sdk::Error> {
        Ok(steery_sdk::wire::ProgramInfo {
            id: self.id().parse().map_err(steery_sdk::Error::invalid)?,
            kind: steery_sdk::wire::Kind::Harness,
            release: env!("STEERY_PROGRAM_RELEASE").into(),
            label: self.label().into(),
            short: self.short().into(),
            tint: Some(self.tint().into()),
            mark: None,
        })
    }
}
