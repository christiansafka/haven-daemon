//! Interactive attach loop.
//!
//! Owns the user's terminal while connected to a session: puts stdin in raw
//! mode, forwards keystrokes (with chord-key interception) as `SessionWrite`
//! frames, renders incoming `Event::Output` payloads to stdout, and reacts to
//! `SIGWINCH` by sending `SessionResize`. Returns an `AttachOutcome` so the
//! caller (e.g. the `haven` CLI) can decide whether to detach, switch, create
//! a new session, or exit.
//!
//! The previous in-tree attach implementation (haven-daemon's `session attach`
//! subcommand) was a smoke-test stub that piped stdin/stdout without raw mode
//! or SIGWINCH handling, which made it unusable for any real interactive use.
//! This module replaces it for both the daemon CLI and the new `haven` CLI.

use anyhow::{anyhow, Result};
use crossterm::terminal;
use haven_protocol::{Event, Frame, FrameType, Request, Response, SessionId};
use std::io::{Read, Write};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::client::send_request_with_history;
use crate::keys::{ChordAction, ChordEvent, ChordParser};

/// What happened when the attach loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachOutcome {
    /// Session process exited; carries the exit code.
    Exited(i32),
    /// User pressed the detach chord (`prefix d`). Session keeps running.
    Detached,
    /// User pressed the switch chord (`prefix s`). Caller should open the
    /// picker and attach to a different session.
    Switch,
    /// User pressed the new chord (`prefix n`). Caller should create a new
    /// session and attach to it.
    NewSession,
    /// Socket disconnected unexpectedly.
    Disconnected,
}

#[derive(Debug, Clone)]
pub struct AttachOptions {
    pub history_bytes: u64,
    /// If true, print the one-line chord-key hint banner before history
    /// replays. Useful for the first attach in a `haven` session.
    pub print_hint: bool,
    /// Non-interactive "pipe" mode for programmatic drivers (e.g. haven-app
    /// running this command over an SSH exec channel without a PTY).
    ///
    /// In pipe mode we skip all the things that assume our stdin is a real
    /// TTY: no raw mode, no chord parsing (stdin bytes pass straight through
    /// as `SessionWrite` payloads), no `SIGWINCH` listener (the driver is
    /// expected to call `session resize` explicitly), no initial resize
    /// (`crossterm::terminal::size()` would fail on a non-tty anyway).
    ///
    /// The on-the-wire framing to the daemon is unchanged — pipe mode only
    /// affects what `run_attach` does to the user's terminal.
    pub pipe_mode: bool,
}

impl Default for AttachOptions {
    fn default() -> Self {
        Self {
            history_bytes: 1_048_576,
            print_hint: false,
            pipe_mode: false,
        }
    }
}

/// RAII guard that restores the terminal to cooked mode when dropped.
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self> {
        terminal::enable_raw_mode()
            .map_err(|e| anyhow!("failed to enable raw mode: {e}"))?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Attach to `session_id` and run the interactive loop until the session
/// exits, the user detaches, or the socket dies.
pub async fn run_attach(
    mut stream: UnixStream,
    session_id: SessionId,
    opts: AttachOptions,
) -> Result<AttachOutcome> {
    // 1. Send the SessionAttach request and capture the history replay.
    let req = Request::SessionAttach {
        session_id,
        history_bytes: opts.history_bytes,
    };
    let history_data = match send_request_with_history(&mut stream, 1, &req).await? {
        (Response::SessionAttached { .. }, history) => history,
        (Response::Error(e), _) => {
            return Err(anyhow!("attach failed: {e}"));
        }
        _ => return Err(anyhow!("unexpected response to SessionAttach")),
    };

    // 2. Print the one-line chord hint, if requested. Done before raw mode so
    //    the leading newline lands on its own row in cooked mode.
    if opts.print_hint {
        eprintln!("[haven] Ctrl-\\ d to detach   Ctrl-\\ ? for help");
    }

    // 3. Replay history to stdout (still cooked — but the bytes are already
    //    raw VT data, so cooked mode doesn't mangle them).
    if !history_data.is_empty() {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(&history_data);
        let _ = stdout.flush();
    }

    // 4. Enter raw mode for the live stream (interactive mode only). Restored
    //    on every return path via the RAII guard. In pipe mode our stdin is
    //    not a TTY, so we skip this entirely — `tcsetattr` would return
    //    ENXIO and break programmatic callers like haven-app.
    let _raw = if opts.pipe_mode {
        None
    } else {
        Some(RawModeGuard::enable()?)
    };

    // 5. Send an initial resize so the daemon's PTY matches the user's actual
    //    terminal size (the SessionAttached response carries no size info).
    //    Skipped in pipe mode: `terminal::size()` doesn't work on a non-tty,
    //    and the driver is responsible for sending its own `session resize`.
    if !opts.pipe_mode {
        if let Ok((cols, rows)) = terminal::size() {
            let resize = Request::SessionResize {
                session_id,
                cols,
                rows,
            };
            if let Ok(frame) = Frame::request(0, &resize) {
                let encoded = frame.encode();
                stream.write_all(&encoded).await.ok();
                stream.flush().await.ok();
            }
        }
    }

    // 6. Split the stream so the reader task and writer task can run
    //    independently.
    let (mut sock_reader, mut sock_writer) = stream.into_split();

    // Channel of frames going to the writer task. Both the stdin parser and
    // the SIGWINCH handler push frames here. Bounded to provide flow control
    // back to the producers.
    let (frame_tx, mut frame_rx) = mpsc::channel::<Frame>(64);

    // Channel of outcomes from the various tasks. Whichever fires first wins;
    // the others get cancelled when this task returns.
    let (outcome_tx, mut outcome_rx) = mpsc::channel::<AttachOutcome>(4);

    // --- Task A: socket reader → stdout writer.
    let outcome_tx_a = outcome_tx.clone();
    let reader_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut stdout = tokio::io::stdout();
        loop {
            let mut len_buf = [0u8; 4];
            if AsyncReadExt::read_exact(&mut sock_reader, &mut len_buf)
                .await
                .is_err()
            {
                let _ = outcome_tx_a.send(AttachOutcome::Disconnected).await;
                return;
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            if !(5..=16 * 1024 * 1024).contains(&len) {
                let _ = outcome_tx_a.send(AttachOutcome::Disconnected).await;
                return;
            }
            let mut body = vec![0u8; len];
            if AsyncReadExt::read_exact(&mut sock_reader, &mut body)
                .await
                .is_err()
            {
                let _ = outcome_tx_a.send(AttachOutcome::Disconnected).await;
                return;
            }
            let frame = match Frame::decode(&body) {
                Ok(f) => f,
                Err(_) => continue,
            };
            // Daemon may interleave Response frames (for our resize/write
            // requests) with Event frames; we only care about Events here.
            if frame.frame_type != FrameType::Event {
                continue;
            }
            let event: Event = match rmp_serde::from_slice(&frame.payload) {
                Ok(e) => e,
                Err(_) => continue,
            };
            match event {
                Event::Output { data, .. } => {
                    if stdout.write_all(&data).await.is_err() {
                        let _ = outcome_tx_a.send(AttachOutcome::Disconnected).await;
                        return;
                    }
                    let _ = stdout.flush().await;
                }
                Event::SessionExited { exit_code, .. } => {
                    let _ = outcome_tx_a.send(AttachOutcome::Exited(exit_code)).await;
                    return;
                }
                Event::SessionActivity { .. } => {}
            }
        }
    });

    // --- Task B: writer task drains the frame channel.
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            let encoded = frame.encode();
            if sock_writer.write_all(&encoded).await.is_err() {
                break;
            }
            if sock_writer.flush().await.is_err() {
                break;
            }
        }
    });

    // --- Task C: stdin reader thread → chord parser → SessionWrite frames.
    //
    // We use a blocking thread because std::io::stdin is the safest way to
    // read raw bytes after enable_raw_mode(); tokio::io::stdin in raw mode
    // has a history of swallowing single bytes on macOS due to its line
    // buffering assumptions. The thread feeds bytes into a tokio mpsc, which
    // an async task drains and runs through the chord parser.
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    let stdin_thread = std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut buf = [0u8; 4096];
        loop {
            match handle.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let frame_tx_c = frame_tx.clone();
    let outcome_tx_c = outcome_tx.clone();
    let pipe_mode = opts.pipe_mode;
    let stdin_task = tokio::spawn(async move {
        // Pipe mode: forward stdin chunks straight through as SessionWrite
        // frames with no chord parsing. This is what programmatic drivers
        // like haven-app expect — they want to be able to send any byte,
        // including Ctrl-\, without it being intercepted as a prefix.
        if pipe_mode {
            while let Some(chunk) = stdin_rx.recv().await {
                let req = Request::SessionWrite {
                    session_id,
                    data: chunk,
                };
                if let Ok(frame) = Frame::request(0, &req) {
                    if frame_tx_c.send(frame).await.is_err() {
                        return;
                    }
                }
            }
            // stdin closed — let the driver-side disconnect drive shutdown.
            let _ = outcome_tx_c.send(AttachOutcome::Disconnected).await;
            return;
        }

        let mut parser = ChordParser::new();
        loop {
            let chunk = match tokio::time::timeout(
                Duration::from_secs(1),
                stdin_rx.recv(),
            )
            .await
            {
                Ok(Some(chunk)) => chunk,
                Ok(None) => return, // stdin closed
                Err(_) => {
                    // Timeout: drop any stale pending prefix so a stray
                    // Ctrl-\ press without a follow-up doesn't get stuck
                    // forever.
                    if parser.is_pending() {
                        parser.cancel_pending();
                    }
                    continue;
                }
            };

            let events = parser.feed(&chunk);
            for ev in events {
                match ev {
                    ChordEvent::Passthrough { from, to } => {
                        let req = Request::SessionWrite {
                            session_id,
                            data: chunk[from..to].to_vec(),
                        };
                        if let Ok(frame) = Frame::request(0, &req) {
                            if frame_tx_c.send(frame).await.is_err() {
                                return;
                            }
                        }
                    }
                    ChordEvent::Action(ChordAction::Detach) => {
                        let _ = outcome_tx_c.send(AttachOutcome::Detached).await;
                        return;
                    }
                    ChordEvent::Action(ChordAction::Switch) => {
                        let _ = outcome_tx_c.send(AttachOutcome::Switch).await;
                        return;
                    }
                    ChordEvent::Action(ChordAction::New) => {
                        let _ = outcome_tx_c.send(AttachOutcome::NewSession).await;
                        return;
                    }
                    ChordEvent::Action(ChordAction::Help) => {
                        // Render an inline help overlay. We're in raw mode so
                        // we have to handle our own line endings.
                        let help = b"\r\n\
                            \x1b[1m[haven] chord keys\x1b[0m\r\n\
                            \r\n\
                            Prefix: \x1b[1mCtrl-\\\x1b[0m or \x1b[1mCtrl-B\x1b[0m\r\n\
                            \r\n\
                              prefix d   detach (session keeps running)\r\n\
                              prefix s   switch session (open picker)\r\n\
                              prefix n   new session\r\n\
                              prefix ?   show this help\r\n\
                              prefix prefix   send a literal prefix byte\r\n\
                            \r\n";
                        let mut stdout = std::io::stdout().lock();
                        let _ = stdout.write_all(help);
                        let _ = stdout.flush();
                    }
                    ChordEvent::Action(ChordAction::LiteralPrefix(b)) => {
                        let req = Request::SessionWrite {
                            session_id,
                            data: vec![b],
                        };
                        if let Ok(frame) = Frame::request(0, &req) {
                            if frame_tx_c.send(frame).await.is_err() {
                                return;
                            }
                        }
                    }
                    ChordEvent::Pending => {}
                }
            }
        }
    });

    // --- Task D: SIGWINCH listener → SessionResize frames.
    //
    // Pipe mode skips this entirely: there is no real terminal whose size
    // could change, and the driver (e.g. haven-app) sends explicit `session
    // resize` calls when its embedded xterm is resized.
    let winch_task = if opts.pipe_mode {
        tokio::spawn(async move {})
    } else {
        let frame_tx_d = frame_tx.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sig = match signal(SignalKind::window_change()) {
                Ok(s) => s,
                Err(_) => return,
            };
            while sig.recv().await.is_some() {
                if let Ok((cols, rows)) = terminal::size() {
                    let req = Request::SessionResize {
                        session_id,
                        cols,
                        rows,
                    };
                    if let Ok(frame) = Frame::request(0, &req) {
                        if frame_tx_d.send(frame).await.is_err() {
                            return;
                        }
                    }
                }
            }
        })
    };

    // Wait for the first outcome from any task.
    let outcome = outcome_rx
        .recv()
        .await
        .unwrap_or(AttachOutcome::Disconnected);

    // Tear everything down.
    reader_task.abort();
    stdin_task.abort();
    winch_task.abort();
    drop(frame_tx);
    let _ = writer_task.await;
    // The blocking stdin thread will exit on its next read attempt when stdin
    // gets closed at process exit; we don't join it here to avoid blocking on
    // a syscall we can't cancel. (It's a daemon thread, no resources leak.)
    let _ = stdin_thread; // explicitly drop the handle, do not join

    Ok(outcome)
}
