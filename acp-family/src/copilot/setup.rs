//! PLACEHOLDER (slice B replaces this file whole).

use std::path::Path;

use steery_sdk::Error;
use steery_sdk::wire;
use steery_sdk::wire::harness::{Planned, Setup};

pub fn facts() -> wire::SetupFacts {
    wire::SetupFacts {
        reads_skills: true,
        cuts_tool_text: false,
        call_bound: None,
    }
}

pub async fn plan(_home: &Path, _root: &Path, _setup: Setup, _needs: bool) -> Result<Planned, Error> {
    Err(Error::Unknown)
}
