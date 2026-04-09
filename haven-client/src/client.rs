//! Low-level Unix-socket client: framing, auth, request/response helpers.
//!
//! The daemon's wire protocol (see `haven-protocol::Frame`) is a length-prefixed
//! MessagePack stream. The first frame on every connection MUST be a
//! `Request::Auth { token }` carrying the contents of `~/.haven/daemon.token`,
//! which is mode 0600 — only the user who owns the daemon can read it. The
//! kernel enforces multi-user isolation via the 0700 parent directory long
//! before we ever see a byte on the wire.

use anyhow::{anyhow, Result};
use haven_protocol::{Event, Frame, FrameType, Request, Response};
use std::path::Path;
use thiserror::Error as ThisError;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

#[derive(Debug, ThisError)]
pub enum ClientError {
    #[error("daemon socket not found at {0}")]
    SocketMissing(String),

    #[error("daemon auth token not readable at {path}: {source}")]
    TokenUnreadable {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("daemon authentication rejected: {0}")]
    AuthRejected(String),

    #[error("connection error: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol error: {0}")]
    Protocol(String),
}

/// A thin wrapper that pairs a connected stream with the daemon's socket path
/// (so callers can re-spawn the daemon if the socket dies mid-session).
pub struct DaemonClient {
    pub stream: UnixStream,
}

impl DaemonClient {
    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}

/// Connect to the daemon at `socket_path`, perform token auth, and return the
/// authenticated stream. The token is expected next to the socket at
/// `<socket_path>.token` (the daemon's `with_extension("token")` convention).
pub async fn connect_daemon(socket_path: &Path) -> Result<UnixStream> {
    if !socket_path.exists() {
        return Err(ClientError::SocketMissing(socket_path.display().to_string()).into());
    }

    let mut stream = UnixStream::connect(socket_path).await.map_err(|e| {
        anyhow!(
            "Failed to connect to daemon at {}: {e}\n\
             Is the daemon running? Start with: haven-session-daemon daemon",
            socket_path.display()
        )
    })?;

    let token_path = socket_path.with_extension("token");
    let token = std::fs::read_to_string(&token_path).map_err(|e| {
        anyhow!(
            "Failed to read auth token at {}: {e}",
            token_path.display()
        )
    })?;

    let auth_req = Request::Auth { token };
    match send_request(&mut stream, 0, &auth_req).await? {
        Response::AuthOk => Ok(stream),
        Response::Error(e) => Err(ClientError::AuthRejected(e.to_string()).into()),
        _ => Err(ClientError::Protocol("unexpected auth response".into()).into()),
    }
}

/// Send a request frame and read the next Response, skipping any Event frames
/// that arrive in between (the daemon may send history Events before the
/// SessionAttached response on attach, for example).
pub async fn send_request(
    stream: &mut UnixStream,
    correlation_id: u32,
    req: &Request,
) -> Result<Response> {
    let frame = Frame::request(correlation_id, req)?;
    let encoded = frame.encode();
    stream.write_all(&encoded).await?;
    stream.flush().await?;

    loop {
        let resp_frame = read_frame(stream).await?;
        if resp_frame.frame_type == FrameType::Response {
            let resp: Response = rmp_serde::from_slice(&resp_frame.payload)
                .map_err(|e| ClientError::Protocol(format!("decode response: {e}")))?;
            return Ok(resp);
        }
        // Skip Events that arrive before the Response.
    }
}

/// Like `send_request`, but captures any `Event::Output` payloads that arrive
/// before the Response. Used by attach: the daemon sends scrollback as Output
/// events ahead of `SessionAttached`, and we want to render that history
/// before entering the live stream.
pub async fn send_request_with_history(
    stream: &mut UnixStream,
    correlation_id: u32,
    req: &Request,
) -> Result<(Response, Vec<u8>)> {
    let frame = Frame::request(correlation_id, req)?;
    let encoded = frame.encode();
    stream.write_all(&encoded).await?;
    stream.flush().await?;

    let mut history = Vec::new();
    loop {
        let resp_frame = read_frame(stream).await?;
        if resp_frame.frame_type == FrameType::Response {
            let resp: Response = rmp_serde::from_slice(&resp_frame.payload)
                .map_err(|e| ClientError::Protocol(format!("decode response: {e}")))?;
            return Ok((resp, history));
        }
        if let Ok(event) = rmp_serde::from_slice::<Event>(&resp_frame.payload) {
            if let Event::Output { data, .. } = event {
                history.extend_from_slice(&data);
            }
        }
    }
}

/// Read one length-prefixed frame from a stream. Errors on eof or malformed
/// length headers.
pub(crate) async fn read_frame(stream: &mut UnixStream) -> Result<Frame> {
    let mut len_buf = [0u8; 4];
    AsyncReadExt::read_exact(stream, &mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    // Same upper bound as the daemon side (haven-daemon/src/daemon.rs::read_frame).
    if len < 5 || len > 16 * 1024 * 1024 {
        return Err(ClientError::Protocol(format!("invalid frame length: {len}")).into());
    }
    let mut body = vec![0u8; len];
    AsyncReadExt::read_exact(stream, &mut body).await?;
    Frame::decode(&body)
        .map_err(|e| ClientError::Protocol(format!("decode frame: {e}")).into())
}
