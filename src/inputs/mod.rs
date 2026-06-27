//! Input transports, decoupled from the [`crate::daemon`] core.
//!
//! - [`http`] — the long-running HTTP server (registration, listing, proxy).
//!   This *is* the daemon's only runtime input.
//! - [`cli`] — the command-line front-end. With no subcommand it runs the
//!   daemon; with `register`/`unregister` it acts as a client that auto-starts
//!   the daemon if needed and sends the request over HTTP.

pub mod cli;
pub mod http;
pub mod wire;
