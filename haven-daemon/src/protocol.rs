use anyhow::{Context, Result};
use haven_protocol::{Event, Frame, Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Read a single frame from the stream.
pub async fn read_frame(stream: &mut UnixStream) -> Result<Option<Frame>> {
    // Read 4-byte length prefix
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    let len = u32::from_be_bytes(len_buf) as usize;
    if len < 5 {
        return Err(anyhow::anyhow!("Frame too short: {len} bytes"));
    }
    if len > 16 * 1024 * 1024 {
        return Err(anyhow::anyhow!("Frame too large: {len} bytes"));
    }

    // Read the frame body
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .await
        .context("Failed to read frame body")?;

    let frame = Frame::decode(&body)?;
    Ok(Some(frame))
}

/// Write a single frame to the stream.
pub async fn write_frame(stream: &mut UnixStream, frame: &Frame) -> Result<()> {
    let encoded = frame.encode();
    stream
        .write_all(&encoded)
        .await
        .context("Failed to write frame")?;
    stream.flush().await.context("Failed to flush stream")?;
    Ok(())
}

/// Decode a request from a frame's payload.
pub fn decode_request(frame: &Frame) -> Result<Request> {
    rmp_serde::from_slice(&frame.payload).context("Failed to decode request")
}

/// Decode a response from a frame's payload.
pub fn decode_response(frame: &Frame) -> Result<Response> {
    rmp_serde::from_slice(&frame.payload).context("Failed to decode response")
}

/// Decode an event from a frame's payload.
pub fn decode_event(frame: &Frame) -> Result<Event> {
    rmp_serde::from_slice(&frame.payload).context("Failed to decode event")
}
