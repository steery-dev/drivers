//! PLACEHOLDER (slice B replaces this file whole).

use std::path::Path;

use steery_sdk::Error;
use steery_sdk::wire::harness::{McpList, McpServers};

pub async fn list(_root: &Path, _asked: McpList) -> Result<McpServers, Error> {
    Err(Error::Unknown)
}
