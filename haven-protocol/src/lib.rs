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
