//! Steery's drivers for the harnesses that speak the Agent Client Protocol (ACP).
//!
//! One library, one binary per harness. [`Harness`] names each one; each harness's driver
//! lives in its own module and answers the program protocol for it alone. What two of them
//! share, such as reading ACP's `session/update` records, is a module of its own.

mod cursor;
mod grok;
mod harness;
mod hermes;
mod kilo;

pub use harness::Harness;

/// Serve the driver for `harness` on standard input and output.
pub async fn serve(harness: Harness) -> std::io::Result<()> {
    match harness {
        Harness::Grok => steery_sdk::serve(|_notifier| grok::Driver::default()).await,
        Harness::Hermes => steery_sdk::serve(|_notifier| hermes::Driver::default()).await,
        Harness::Kilo => match std::env::args().nth(1).as_deref() {
            Some("turn") => kilo::turn::main(std::env::args().skip(2)).await,
            _ => steery_sdk::serve(|_notifier| kilo::Driver::default()).await,
        },
        Harness::Cursor => steery_sdk::serve(|_notifier| cursor::Driver::default()).await,
    }
}
