//! Kiro at drive: Steery starts Kiro's turns itself. A turn is this driver's own binary in
//! its turn mode, speaking ACP to `kiro-cli acp` ([`crate::turn`]), which runs the
//! conversation in the turn's own process. A continue loads the conversation
//! (`session/load`: Kiro 3.3.0's ACP server offers no `session/resume`), a fork is
//! `session/fork`. Kiro asks the turn's client before each shell command
//! (`session/request_permission`), which the turn answers through Steery's hook.

/// Kiro, to a turn this driver runs ([`crate::turn`]): words told mid-turn go in as one
/// prompt once Kiro has answered the turn's.
pub struct Turns;

impl crate::turn::Host for Turns {
    fn harness(&self) -> crate::Harness {
        crate::Harness::Kiro
    }

    fn told(&self) -> crate::turn::Told {
        crate::turn::Told::After
    }

    fn opens(&self) -> crate::turn::Opens {
        crate::turn::Opens::Load
    }

    fn thinking(&self) -> &'static str {
        "effortLevel"
    }

    /// Kiro runs one prompt of a session at a time: once it has answered every prompt the
    /// turn sent, the turn's server runs nothing more of it.
    fn runs(
        &self,
        _acp: Option<u32>,
        _session: &str,
        _queued: &[String],
        _: &mut Option<tokio::time::Instant>,
    ) -> bool {
        false
    }
}
