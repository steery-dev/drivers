//! The harnesses of the ACP family, and what names each on a person's tile.

/// A harness of the ACP family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Harness {
    /// xAI's Grok Build, whose executable is `grok`.
    Grok,
    /// GitHub Copilot CLI, whose executable is `copilot`.
    Copilot,
    /// OpenCode 2, whose executable is `opencode`.
    OpenCode,
    /// Nous Research's Hermes Agent, whose executable is `hermes`.
    Hermes,
    /// Kilo, whose executable is `kilo`.
    Kilo,
    /// Cursor's agent CLI, whose executable is `cursor-agent`.
    Cursor,
    /// Cline's CLI, whose executable is `cline`.
    Cline,
    /// Goose, whose executable is `goose`.
    Goose,
    /// Google's Gemini CLI, whose executable is `gemini`.
    Gemini,
    /// Google's Antigravity CLI, whose executable is `agy`.
    Antigravity,
    /// Alibaba's Qwen Code, whose executable is `qwen`.
    Qwen,
    /// OpenClaw, whose executable is `openclaw`.
    OpenClaw,
    /// Factory's Droid, whose executable is `droid`.
    Droid,
    /// Amazon's Kiro CLI, whose executable is `kiro-cli`.
    Kiro,
}

impl Harness {
    /// The driver's id.
    pub fn id(self) -> &'static str {
        match self {
            Self::Grok => "grok",
            Self::Copilot => "copilot",
            Self::OpenCode => "opencode",
            Self::Hermes => "hermes",
            Self::Kilo => "kilo",
            Self::Cursor => "cursor",
            Self::Cline => "cline",
            Self::Goose => "goose",
            Self::Gemini => "gemini",
            Self::Antigravity => "antigravity",
            Self::Qwen => "qwen",
            Self::OpenClaw => "openclaw",
            Self::Droid => "droid",
            Self::Kiro => "kiro",
        }
    }

    /// Its name as a person reads it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Grok => "Grok Build",
            Self::Copilot => "Copilot",
            Self::OpenCode => "OpenCode",
            Self::Hermes => "Hermes Agent",
            Self::Kilo => "Kilo",
            Self::Cursor => "Cursor",
            Self::Cline => "Cline",
            Self::Goose => "Goose",
            Self::Gemini => "Gemini CLI",
            Self::Antigravity => "Antigravity",
            Self::Qwen => "Qwen Code",
            Self::OpenClaw => "OpenClaw",
            Self::Droid => "Droid",
            Self::Kiro => "Kiro",
        }
    }

    /// Its name on a tile.
    pub fn short(self) -> &'static str {
        match self {
            Self::Grok => "Gk",
            Self::Copilot => "Co",
            Self::OpenCode => "Oc",
            Self::Hermes => "He",
            Self::Kilo => "Kl",
            Self::Cursor => "Cu",
            Self::Cline => "Cl",
            Self::Goose => "Go",
            Self::Gemini => "Gm",
            Self::Antigravity => "Ag",
            Self::Qwen => "Qw",
            Self::OpenClaw => "Ow",
            Self::Droid => "Dr",
            Self::Kiro => "Ki",
        }
    }

    /// Its colour on a tile.
    pub fn tint(self) -> &'static str {
        match self {
            Self::Grok => "#1d1d1f",
            Self::Copilot => "#8534f3",
            Self::OpenCode => "#211e1e",
            Self::Hermes => "#4b2fd0",
            Self::Kilo => "#617a91",
            Self::Cursor => "#14120b",
            Self::Cline => "#1c1c24",
            Self::Goose => "#101010",
            Self::Gemini => "#4285f4",
            Self::Antigravity => "#b1502f",
            Self::Qwen => "#615ced",
            Self::OpenClaw => "#e8453c",
            Self::Droid => "#ee6018",
            Self::Kiro => "#9046ff",
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
