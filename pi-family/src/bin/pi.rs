//! Steery's `pi` driver.

// Many threads: most calls read files without yielding, and on one thread a slow one stops
// standard input being read; the runtime then kills the driver with every call in flight.
#[tokio::main]
async fn main() -> std::io::Result<()> {
    steery_driver_pi_family::serve(steery_driver_pi_family::Harness::Pi).await
}
