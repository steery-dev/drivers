//! The last answer of a sign-in probe. A probe can give no answer: it timed out, did not
//! start, or printed nothing that parses. A plan that reads no answer as "no need" makes the
//! runtime withdraw the sign-in card, and the next plan with an answer raises it again. So
//! the plan keeps the last answer the probe gave in this process ([`Known`]) and uses it
//! while the probe gives none.

use std::sync::{Mutex, PoisonError};

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
    use super::Known;

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
