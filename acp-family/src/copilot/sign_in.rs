//! PLACEHOLDER (slice B replaces this file whole).

use steery_sdk::Error;
use steery_sdk::wire::harness::{Give, Given};

#[derive(Default)]
pub struct SignIns;

impl SignIns {
    pub async fn give(&self, _give: Give) -> Result<Given, Error> {
        Err(Error::Unknown)
    }
}
