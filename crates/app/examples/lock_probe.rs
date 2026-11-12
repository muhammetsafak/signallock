//! Manual probe for the lock strategy chain.
//!
//! `cargo run --example lock_probe -- --dry-run` prints the strategy
//! `LockService` would use on this machine and locks nothing.
//!
//! Without `--dry-run` it LOCKS THE SCREEN for real and prints the strategy
//! that did it.

use signallock::lock_service::LockService;

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let dry_run = std::env::args().skip(1).any(|arg| arg == "--dry-run");
    let service = LockService::new();

    if dry_run {
        match service.select_method() {
            Some(method) => println!("dry run: would lock using {method}"),
            None => {
                println!("dry run: no lock strategy is available on this machine");
                std::process::exit(1);
            }
        }
        return;
    }

    match service.lock_now() {
        Ok(method) => println!("locked using {method}"),
        Err(e) => {
            eprintln!("lock failed: {e}");
            std::process::exit(1);
        }
    }
}
