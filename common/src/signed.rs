//! The last answer of a sign-in probe. A probe can give no answer: it timed out, did not
//! start, or printed nothing that parses. A plan that reads no answer as "no need" makes the
//! runtime withdraw the sign-in card, and the next plan with an answer raises it again. So
//! the plan keeps the last answer the probe gave in this process ([`Known`]) and uses it
//! while the probe gives none.
//!
//! A probe can also say "not signed in" once while the harness is signed in: Claude Code
//! did on aram-personal-mac on 2026-11-15 and 2026-11-20, and its sign-in card came and went
//! in 65 s. The runtime asks each sign-in once on a host, so a wrong ask uses that one ask.
//! So a plan takes "not signed in" only where the probe says it twice ([`sure`]).

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// How long [`sure`] waits before it asks a probe that said "not signed in" again.
pub const AGAIN: Duration = Duration::from_secs(2);

/// The answer of `probe`, made sure: where it says "not signed in", it is asked again after
/// [`AGAIN`], and that second answer is the answer. So "not signed in" stands only where the
/// probe said it twice; a second probe with no answer gives none ([`Known::steady`] then
/// keeps the last answer). "Signed in" and no answer are taken at once.
pub async fn sure<F, Fut>(probe: F) -> Option<bool>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Option<bool>>,
{
    sure_after(probe, AGAIN).await
}

async fn sure_after<F, Fut>(probe: F, again: Duration) -> Option<bool>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Option<bool>>,
{
    match probe().await {
        Some(false) => {
            tokio::time::sleep(again).await;
            probe().await
        }
        other => other,
    }
}

/// The last answer a probe gave in this process: whether the harness is signed in.
pub struct Known(Mutex<Option<bool>>);

impl Known {
    /// A probe that has given no answer yet.
    pub const fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// The answer for the plan, given the probe's answer `now`. An answer is kept and used.
    /// No answer gives the last one kept; none kept yet gives none.
    pub fn steady(&self, now: Option<bool>) -> Option<bool> {
        let mut last = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if now.is_some() {
            *last = now;
        }
        *last
    }

    /// Forget the answer kept, after a sign-in the person gave: it says "not signed in" no
    /// more. The next probe with no answer then gives none, as with nothing known.
    pub fn forget(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

impl Default for Known {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::time::Duration;

    use super::{Known, sure_after};

    /// What `sure` answers, and how many times it asked, where the probe gives `said` in turn.
    async fn made_sure(said: &[Option<bool>]) -> (Option<bool>, usize) {
        let asked = RefCell::new(0);
        let answer = sure_after(
            || {
                let at = *asked.borrow();
                *asked.borrow_mut() += 1;
                async move { said[at] }
            },
            Duration::ZERO,
        )
        .await;
        (answer, asked.into_inner())
    }

    /// 2026-11-15 and 2026-11-20: Claude Code said "not signed in" once while it was signed
    /// in. One such answer plans no need; two in a row do.
    #[tokio::test]
    async fn not_signed_in_stands_only_when_said_twice() {
        assert_eq!(made_sure(&[Some(false), Some(true)]).await, (Some(true), 2));
        assert_eq!(made_sure(&[Some(false), None]).await, (None, 2));
        assert_eq!(made_sure(&[Some(false), Some(false)]).await, (Some(false), 2));
        assert_eq!(made_sure(&[Some(true)]).await, (Some(true), 1));
        assert_eq!(made_sure(&[None]).await, (None, 1));
        // A wrong "not signed in" whose second probe gives no answer leaves a plan that
        // knew nothing planning no need.
        let known = Known::new();
        assert!(!plans(&known, made_sure(&[Some(false), None]).await.0));
    }

    /// Whether the plan names the need: the steady answer says "not signed in".
    fn plans(known: &Known, now: Option<bool>) -> bool {
        known.steady(now) == Some(false)
    }

    #[test]
    fn a_probe_with_no_answer_keeps_the_last_answer() {
        let known = Known::new();
        let seen = [Some(false), None, None, Some(true), None].map(|probe| plans(&known, probe));
        assert_eq!(seen, [true, true, true, false, false]);
    }

    #[test]
    fn no_answer_known_yet_plans_no_need() {
        let known = Known::new();
        assert_eq!(known.steady(None), None);
        assert!(!plans(&known, None));
    }

    #[test]
    fn a_forgotten_answer_plans_no_need_until_the_probe_answers_again() {
        let known = Known::new();
        assert!(plans(&known, Some(false)));
        known.forget();
        assert!(!plans(&known, None));
        assert_eq!(known.steady(None), None);
        assert!(plans(&known, Some(false)));
    }
}
