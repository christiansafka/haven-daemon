use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::session::SessionId;

/// Events streamed from the daemon to attached clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    /// Terminal output data from a session.
    Output {
        session_id: SessionId,
        data: Vec<u8>,
    },

    /// A session's process has exited.
    SessionExited {
        session_id: SessionId,
        exit_code: i32,
    },

    /// A session's metadata has changed (cwd, title, etc.).
    SessionActivity {
        session_id: SessionId,
        cwd: Option<PathBuf>,
        title: Option<String>,
    },
}
