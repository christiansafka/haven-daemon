use serde::{Deserialize, Serialize};

use crate::error::HavenError;
use crate::session::{SessionCreate, SessionId, SessionInfo, TranscriptSearchResults};

/// A request from the app to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    /// Create a new session.
    SessionCreate(SessionCreate),

    /// List all sessions.
    SessionList,

    /// Attach to a session to receive output.
    SessionAttach {
        session_id: SessionId,
        /// How many bytes of history to replay on attach.
        history_bytes: u64,
    },

    /// Detach from a session (stop receiving output).
    SessionDetach { session_id: SessionId },

    /// Write data to a session's PTY.
    SessionWrite {
        session_id: SessionId,
        data: Vec<u8>,
    },

    /// Resize a session's PTY.
    SessionResize {
        session_id: SessionId,
        cols: u16,
        rows: u16,
    },

    /// Kill a session.
    SessionKill {
        session_id: SessionId,
        signal: Option<i32>,
    },

    /// Rename a session.
    SessionRename {
        session_id: SessionId,
        name: String,
    },

    /// Get durable history for a session.
    SessionHistory {
        session_id: SessionId,
        offset: u64,
        length: u64,
    },

    /// Search a session's full durable history.
    SessionSearchHistory {
        session_id: SessionId,
        pattern: String,
        case_insensitive: bool,
        regex: bool,
        limit: u32,
    },

    /// Ping the daemon.
    Ping,

    /// Get daemon status.
    DaemonStatus,

    /// Authenticate with the daemon (must be first frame on connection).
    Auth { token: String },
}

/// A response from the daemon to the app.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
    /// Session was created.
    SessionCreated(SessionInfo),

    /// List of all sessions.
    SessionList { sessions: Vec<SessionInfo> },

    /// Successfully attached to session.
    SessionAttached { session_id: SessionId },

    /// Successfully detached from session.
    SessionDetached,

    /// Data was written to the session.
    Written { bytes: usize },

    /// Session was resized.
    Resized,

    /// Session was killed.
    SessionKilled,

    /// Session was renamed.
    SessionRenamed,

    /// A chunk of session history.
    HistoryChunk {
        data: Vec<u8>,
        offset: u64,
        total: u64,
    },

    /// Results of a transcript search.
    SearchHistoryResults(TranscriptSearchResults),

    /// Pong response.
    Pong {
        uptime_secs: u64,
        session_count: usize,
    },

    /// Daemon status information.
    DaemonStatus {
        version: String,
        uptime_secs: u64,
        session_count: usize,
        pid: u32,
    },

    /// Authentication succeeded.
    AuthOk,

    /// An error occurred.
    Error(HavenError),
}
