//! Steery's OpenHands driver.

#[tokio::main]
async fn main() -> std::io::Result<()> {
    steery_driver_acp_family::serve(steery_driver_acp_family::Harness::OpenHands).await
}
