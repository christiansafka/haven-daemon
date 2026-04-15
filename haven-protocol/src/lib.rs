pub mod error;
pub mod event;
pub mod host;
pub mod message;
pub mod session;

// Re-export commonly used types
pub use error::HavenError;
pub use event::Event;
pub use host::{AuthMethod, HostId, HostInfo};
pub use message::{Request, Response};
pub use session::{
    SessionCreate, SessionId, SessionInfo, SessionKind, SessionStatus, SessionTemplate,
    TranscriptSearchMatch, TranscriptSearchResults,
};

/// The default Unix socket path for the daemon.
pub fn default_socket_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(home).join(".haven").join("daemon.sock")
}

/// The default data directory for Haven.
pub fn default_data_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(home).join(".haven")
}

/// Discover an existing daemon socket in `~/.haven/`.
///
/// Order of preference:
///   1. The unversioned `daemon.sock` (a user-spawned local daemon — wins if
///      present AND a daemon is listening on it).
///   2. The highest-version `daemon-{version}.sock` with a listening daemon.
///      Versions are compared as dotted integer tuples, so `0.1.10` > `0.1.9`.
///   3. As a last resort, the highest-version socket file that exists, even
///      if nothing is currently listening — gives error messages a real path
///      and lets autostart retry.
///
/// Stale socket files (a crashed daemon, or a different-version daemon that
/// exited) are skipped: we do a non-blocking connect probe to confirm a live
/// listener before choosing a socket.
///
/// Returns `None` if no candidate sockets exist. Callers should fall back to
/// `default_socket_path()` so autostart and error messages use a known path.
pub fn discover_socket_path() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    discover_socket_path_in(&std::path::PathBuf::from(home).join(".haven"))
}

/// Same as `discover_socket_path` but searches an explicit data directory.
/// Lets the CLI (which may be installed under `~/.haven-dev/`) find its own
/// variant's sockets instead of defaulting to `~/.haven/`.
pub fn discover_socket_path_in(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let is_live = |p: &std::path::Path| -> bool {
        std::os::unix::net::UnixStream::connect(p).is_ok()
    };

    let unversioned = dir.join("daemon.sock");
    if unversioned.exists() && is_live(&unversioned) {
        return Some(unversioned);
    }

    let mut candidates: Vec<(Vec<u32>, std::path::PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Strict match: "daemon-<version>.sock" where <version> is dot-separated digits.
        let Some(version_str) = name
            .strip_prefix("daemon-")
            .and_then(|s| s.strip_suffix(".sock"))
        else {
            continue;
        };
        let parts: Option<Vec<u32>> = version_str.split('.').map(|s| s.parse().ok()).collect();
        if let Some(parts) = parts {
            if !parts.is_empty() {
                candidates.push((parts, path));
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));

    // Prefer the highest-version socket with a live listener.
    if let Some((_, path)) = candidates.iter().find(|(_, p)| is_live(p)) {
        return Some(path.clone());
    }

    // Nothing is listening anywhere. Return the highest-version socket file
    // if one exists, else the stale unversioned path, else None.
    candidates
        .into_iter()
        .next()
        .map(|(_, p)| p)
        .or_else(|| unversioned.exists().then_some(unversioned))
}

/// List every daemon socket file in `~/.haven/` (unversioned + all versioned).
/// Used by the CLI to warn about sessions on other daemon versions.
pub fn list_daemon_sockets() -> Vec<std::path::PathBuf> {
    let Ok(home) = std::env::var("HOME") else {
        return Vec::new();
    };
    list_daemon_sockets_in(&std::path::PathBuf::from(home).join(".haven"))
}

/// Same as `list_daemon_sockets` but searches an explicit data directory.
pub fn list_daemon_sockets_in(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            let is_match = name == "daemon.sock"
                || (name.starts_with("daemon-") && name.ends_with(".sock"));
            if is_match {
                out.push(path);
            }
        }
    }
    out
}

/// Wire protocol frame types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Request = 1,
    Response = 2,
    Event = 3,
}

impl TryFrom<u8> for FrameType {
    type Error = HavenError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(FrameType::Request),
            2 => Ok(FrameType::Response),
            3 => Ok(FrameType::Event),
            _ => Err(HavenError::Internal {
                reason: format!("invalid frame type: {value}"),
            }),
        }
    }
}

/// A wire protocol frame.
#[derive(Debug, Clone)]
pub struct Frame {
    pub frame_type: FrameType,
    pub correlation_id: u32,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Encode a frame to bytes: [4B length][1B type][4B correlation_id][payload]
    pub fn encode(&self) -> Vec<u8> {
        let payload_len = self.payload.len();
        let total_len = 1 + 4 + payload_len; // type + corr_id + payload
        let mut buf = Vec::with_capacity(4 + total_len);
        buf.extend_from_slice(&(total_len as u32).to_be_bytes());
        buf.push(self.frame_type as u8);
        buf.extend_from_slice(&self.correlation_id.to_be_bytes());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Decode a frame from bytes (after the 4-byte length prefix has been read).
    pub fn decode(data: &[u8]) -> Result<Self, HavenError> {
        if data.len() < 5 {
            return Err(HavenError::Internal {
                reason: "frame too short".to_string(),
            });
        }
        let frame_type = FrameType::try_from(data[0])?;
        let correlation_id = u32::from_be_bytes([data[1], data[2], data[3], data[4]]);
        let payload = data[5..].to_vec();
        Ok(Frame {
            frame_type,
            correlation_id,
            payload,
        })
    }

    /// Create a request frame.
    pub fn request(correlation_id: u32, req: &Request) -> Result<Self, HavenError> {
        let payload = rmp_serde::to_vec(req).map_err(|e| HavenError::Internal {
            reason: format!("serialize error: {e}"),
        })?;
        Ok(Frame {
            frame_type: FrameType::Request,
            correlation_id,
            payload,
        })
    }

    /// Create a response frame.
    pub fn response(correlation_id: u32, resp: &Response) -> Result<Self, HavenError> {
        let payload = rmp_serde::to_vec(resp).map_err(|e| HavenError::Internal {
            reason: format!("serialize error: {e}"),
        })?;
        Ok(Frame {
            frame_type: FrameType::Response,
            correlation_id,
            payload,
        })
    }

    /// Create an event frame (correlation_id = 0).
    pub fn event(evt: &Event) -> Result<Self, HavenError> {
        let payload = rmp_serde::to_vec(evt).map_err(|e| HavenError::Internal {
            reason: format!("serialize error: {e}"),
        })?;
        Ok(Frame {
            frame_type: FrameType::Event,
            correlation_id: 0,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let req = Request::Ping;
        let frame = Frame::request(42, &req).unwrap();
        let encoded = frame.encode();

        // Read length prefix
        let len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]) as usize;
        let decoded = Frame::decode(&encoded[4..4 + len]).unwrap();

        assert_eq!(decoded.frame_type, FrameType::Request);
        assert_eq!(decoded.correlation_id, 42);

        let decoded_req: Request = rmp_serde::from_slice(&decoded.payload).unwrap();
        assert!(matches!(decoded_req, Request::Ping));
    }

    #[test]
    fn session_create_serialization() {
        let create = SessionCreate::default();
        let bytes = rmp_serde::to_vec(&create).unwrap();
        let decoded: SessionCreate = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded.cols, 80);
        assert_eq!(decoded.rows, 24);
    }
}
