use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors that can occur in the Haven protocol.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum HavenError {
    #[error("session not found: {session_id}")]
    SessionNotFound { session_id: String },

    #[error("session already exists: {name}")]
    SessionAlreadyExists { name: String },

    #[error("failed to spawn PTY: {reason}")]
    PtySpawnFailed { reason: String },

    #[error("session is not attached: {session_id}")]
    SessionNotAttached { session_id: String },

    #[error("session has exited: {session_id}")]
    SessionExited { session_id: String },

    #[error("host not found: {host_id}")]
    HostNotFound { host_id: String },

    #[error("connection failed: {reason}")]
    ConnectionFailed { reason: String },

    #[error("permission denied: {reason}")]
    PermissionDenied { reason: String },

    #[error("internal error: {reason}")]
    Internal { reason: String },
}
