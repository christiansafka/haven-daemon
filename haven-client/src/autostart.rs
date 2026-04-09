//! Auto-spawn the daemon if it isn't already running.
//!
//! The `haven` CLI is meant to "just work" — a user SSHing into a fresh
//! machine should be able to type `haven` and get a session, without first
//! having to figure out that there's a separate background process they need
//! to start. So we check whether `~/.haven/daemon.sock` exists, and if not,
//! fork the same binary as `haven-session-daemon daemon` in the background
//! and wait briefly for the socket to appear.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use thiserror::Error as ThisError;

#[derive(Debug, ThisError)]
pub enum EnsureDaemonError {
    #[error("could not locate the haven-session-daemon binary to spawn")]
    BinaryNotFound,

    #[error("daemon spawned but its socket never appeared at {0}")]
    SocketNeverAppeared(String),

    #[error("io error while starting daemon: {0}")]
    Io(#[from] std::io::Error),
}

/// Make sure a daemon is running and reachable on `socket_path`. If the socket
/// already exists, returns immediately. Otherwise locates the daemon binary
/// (preferring `daemon_binary_hint` if provided), spawns it detached with
/// stdin/stdout/stderr redirected to the daemon log, and polls for the socket
/// to appear (up to ~5 seconds).
pub async fn ensure_daemon_running(
    socket_path: &Path,
    daemon_binary_hint: Option<&Path>,
) -> Result<()> {
    if socket_path.exists() {
        return Ok(());
    }

    let bin = locate_daemon_binary(daemon_binary_hint)
        .ok_or(EnsureDaemonError::BinaryNotFound)?;

    let log_path = socket_path
        .parent()
        .map(|p| p.join("daemon.log"))
        .unwrap_or_else(|| PathBuf::from("/tmp/haven-daemon.log"));

    // Make sure the parent dir exists; the daemon will set its own perms but
    // it can't create the directory if its parent is unwritable.
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).map_err(EnsureDaemonError::Io)?;
    }

    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(EnsureDaemonError::Io)?;
    let log_file_err = log_file
        .try_clone()
        .map_err(EnsureDaemonError::Io)?;

    // Spawn detached. We deliberately do not use `nohup`/`setsid` here — the
    // child still gets reaped if the user kills its parent shell, but that's
    // fine: the daemon stores all session state on disk and the next `haven`
    // invocation will spawn a fresh one. For a fully-detached daemon, the
    // user should run `haven-session-daemon daemon` from their shell rc.
    Command::new(&bin)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .spawn()
        .map_err(EnsureDaemonError::Io)?;

    // Poll for the socket. The daemon takes ~50-200ms to bind on a healthy
    // box, so 5s of headroom is generous.
    for _ in 0..50 {
        if socket_path.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Err(anyhow!(EnsureDaemonError::SocketNeverAppeared(
        socket_path.display().to_string()
    )))
}

/// Find the `haven-session-daemon` executable.
///
/// Order of preference:
///   1. The hint passed in (typically `std::env::current_exe()`, since when
///      `haven` runs as a multicall symlink, the binary it resolves to *is*
///      the daemon).
///   2. `~/.haven/bin/haven-session-daemon` (the install path used by the
///      install script and by the Haven Mac app's remote installer).
///   3. `haven-session-daemon` on `$PATH`.
fn locate_daemon_binary(hint: Option<&Path>) -> Option<PathBuf> {
    if let Some(hint) = hint {
        if hint.exists() {
            return Some(hint.to_path_buf());
        }
    }

    if let Some(home) = std::env::var_os("HOME") {
        let candidate = PathBuf::from(home)
            .join(".haven")
            .join("bin")
            .join("haven-session-daemon");
        if candidate.exists() {
            return Some(candidate);
        }
    }

    // Last resort: $PATH lookup.
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = PathBuf::from(dir).join("haven-session-daemon");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    None
}
