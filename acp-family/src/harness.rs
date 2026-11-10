//! The harnesses of the ACP family, and what names each on a person's tile.

/// A harness of the ACP family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    /// xAI's Grok Build, whose executable is `grok`.
    Grok,
    /// OpenCode 2, whose executable is `opencode`.
    OpenCode,
    /// Nous Research's Hermes Agent, whose executable is `hermes`.
    Hermes,
    /// Kilo, whose executable is `kilo`.
    Kilo,
    /// Cursor's agent CLI, whose executable is `cursor-agent`.
    Cursor,
}

impl Harness {
    /// The driver's id.
    pub fn id(self) -> &'static str {
        match self {
            Self::Grok => "grok",
            Self::OpenCode => "opencode",
            Self::Hermes => "hermes",
            Self::Kilo => "kilo",
            Self::Cursor => "cursor",
        }
    }

    /// Its name as a person reads it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Grok => "Grok Build",
            Self::OpenCode => "OpenCode",
            Self::Hermes => "Hermes Agent",
            Self::Kilo => "Kilo",
            Self::Cursor => "Cursor",
        }
    }

    /// Its name on a tile.
    pub fn short(self) -> &'static str {
        match self {
            Self::Grok => "Gk",
            Self::OpenCode => "Oc",
            Self::Hermes => "He",
            Self::Kilo => "Kl",
            Self::Cursor => "Cu",
        }
    }

    /// Its colour on a tile.
    pub fn tint(self) -> &'static str {
        match self {
            Self::Grok => "#1d1d1f",
            Self::OpenCode => "#211e1e",
            Self::Hermes => "#4b2fd0",
            Self::Kilo => "#617a91",
            Self::Cursor => "#14120b",
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
