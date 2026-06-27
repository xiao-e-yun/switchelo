//! switchelo — a lightweight dynamic service registry and reverse proxy.
//!
//! The registry core lives in [`daemon`] and is transport-agnostic. The
//! command line ([`inputs::cli`]) is the single entry point: with no subcommand
//! it runs the daemon; with `register`/`unregister` it auto-starts the daemon
//! if needed and sends the request over HTTP.

mod daemon;
mod inputs;

use daemon::Daemon;
use inputs::cli::Action;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "switchelo=info".to_string()),
        )
        .init();

    match Action::parse() {
        Action::Serve { bind } => inputs::http::serve(Daemon::new(), &bind).await,
        action => inputs::cli::run_client(action).await,
    }
}
