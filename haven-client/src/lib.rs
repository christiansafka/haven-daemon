//! Haven client library.
//!
//! Reusable transport + interactive attach machinery for talking to a local
//! `haven-session-daemon` over its Unix socket. The daemon CLI and the
//! `haven` CLI both depend on this crate so the connect / auth / attach loop
//! has exactly one implementation.

pub mod attach;
pub mod autostart;
pub mod client;
pub mod keys;

pub use attach::{run_attach, AttachOutcome, AttachOptions};
pub use autostart::{ensure_daemon_running, EnsureDaemonError};
pub use client::{
    connect_daemon, send_request, send_request_with_history, ClientError, DaemonClient,
};
