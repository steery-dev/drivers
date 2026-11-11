//! Steery's Cline driver.

// Many threads: most calls read the database without yielding, and on one thread a slow one
// stops standard input being read; the runtime then kills the driver with every call in flight.
#[tokio::main]
async fn main() -> std::io::Result<()> {
    steery_driver_acp_family::serve(steery_driver_acp_family::Harness::Cline).await
}
