//! Steery's drivers for the harnesses that speak the Agent Client Protocol (ACP).
//!
//! One library, one binary per harness. [`Harness`] names each one; each harness's driver
//! lives in its own module and answers the program protocol for it alone. What two of them
//! share, such as reading ACP's `session/update` records, is a module of its own.

mod antigravity;
mod cline;
mod copilot;
mod cursor;
mod gemini;
mod goose;
mod grok;
mod harness;
mod hermes;
mod jsonc;
mod kilo;
mod opencode;
mod turn;

pub use harness::Harness;

/// Serve the driver for `harness` on standard input and output. Copilot's binary is also
/// the turn mode that its `start` plans: `<binary> turn ...`.
pub async fn serve(harness: Harness) -> std::io::Result<()> {
    match harness {
        Harness::Grok => steery_sdk::serve(|_notifier| grok::Driver::default()).await,
        Harness::Copilot => {
            let mut args = std::env::args().skip(1);
            match args.next() {
                Some(mode) if mode == copilot::TURN_MODE => copilot::turn_main(args).await,
                Some(mode) if mode == copilot::READER_MODE => copilot::reader_main(args).await,
                _ => steery_sdk::serve(copilot::Driver::new).await,
            }
        }
        Harness::OpenCode => match std::env::args().nth(1).as_deref() {
            Some("turn") => {
                turn::main(opencode::drive::Turns::new(), std::env::args().skip(2)).await
            }
            _ => steery_sdk::serve(|_notifier| opencode::Driver::default()).await,
        },
        Harness::Hermes => match std::env::args().nth(1).as_deref() {
            Some("turn") => turn::main(hermes::drive::Turns::new(), std::env::args().skip(2)).await,
            _ => steery_sdk::serve(hermes::Driver::new).await,
        },
        Harness::Kilo => match std::env::args().nth(1).as_deref() {
            Some("turn") => turn::main(kilo::drive::Turns::new(), std::env::args().skip(2)).await,
            _ => steery_sdk::serve(|_notifier| kilo::Driver::default()).await,
        },
        Harness::Cursor => steery_sdk::serve(|_notifier| cursor::Driver::default()).await,
        Harness::Cline => match std::env::args().nth(1).as_deref() {
            Some("turn") => turn::main(cline::drive::Turns::new(), std::env::args().skip(2)).await,
            _ => steery_sdk::serve(cline::Driver::new).await,
        },
        Harness::Goose => match std::env::args().nth(1).as_deref() {
            Some("turn") => turn::main(goose::drive::Turns, std::env::args().skip(2)).await,
            _ => steery_sdk::serve(|_notifier| goose::Driver::default()).await,
        },
        Harness::Gemini => steery_sdk::serve(|_notifier| gemini::Driver::default()).await,
        Harness::Antigravity => match std::env::args().nth(1).as_deref() {
            Some("turn") => turn::main(antigravity::drive::Turns, std::env::args().skip(2)).await,
            _ => steery_sdk::serve(|_notifier| antigravity::Driver::default()).await,
        },
    }
}
