//! switchelo — a lightweight dynamic service registry and reverse proxy.
//!
//! The registry core lives in [`daemon`] and is transport-agnostic. The
//! command line ([`inputs::cli`]) is the single entry point: with no subcommand
//! it runs the daemon; with `register`/`unregister` it auto-starts the daemon
//! if needed and sends the request over HTTP.

mod daemon;
mod inputs;

use clap::Parser;

use daemon::Daemon;
use inputs::cli::Cli;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "switchelo=info".to_string()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        None => {
            let addr = inputs::cli::resolve_bind(&cli.bind, cli.public);
            inputs::http::serve(Daemon::new(), &addr).await
        }
        Some(command) => inputs::cli::run_client(&cli.bind, cli.public, command).await,
    }
}
