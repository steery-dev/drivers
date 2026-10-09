//! Steery's official `pi` driver.

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    steery_driver_pi_family::serve(steery_driver_pi_family::Harness::Pi).await
}
