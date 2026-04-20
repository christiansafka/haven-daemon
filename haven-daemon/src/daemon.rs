use anyhow::{Context, Result};
use haven_protocol::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, Mutex};
use uuid::Uuid;

use crate::session::SessionManager;

/// Shared state for the parent-watch task. Mutated by the `SetParentWatch`
/// request handler and read by the polling task.
///
/// `pid` is the process the daemon is currently watching. When `None`, the
/// watcher is paused — typically because the app announced it's about to
/// restart for an update. `grace_until` bounds how long that paused state
/// is honored: once it expires the daemon falls back to its previously
/// known PID, which (if the updater silently died) will trip the
/// "parent gone" path on the next tick and cleanly tear everything down.
#[derive(Debug, Clone)]
pub struct ParentWatchState {
    pub pid: Option<u32>,
    pub last_known_pid: Option<u32>,
    pub grace_until: Option<Instant>,
}

impl ParentWatchState {
    fn new(initial: Option<u32>) -> Self {
        Self {
            pid: initial,
            last_known_pid: initial,
            grace_until: None,
        }
    }
}

/// How often the parent-watch task probes the parent PID. 2 seconds is the
/// sweet spot between "instant cleanup" and "free polling cost" — a single
/// `kill(pid, 0)` syscall every 2s is essentially zero overhead.
const PARENT_WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// The daemon server.
pub struct Daemon {
    session_manager: Arc<SessionManager>,
    socket_path: PathBuf,
    token: String,
    start_time: Instant,
    parent_watch: Arc<Mutex<ParentWatchState>>,
}

impl Daemon {
    pub fn new(socket_path: PathBuf, data_dir: PathBuf) -> Self {
        Self::with_parent_watch(socket_path, data_dir, None)
    }

    /// Construct the daemon with an initial parent-watch PID. When `Some`,
    /// the daemon will spawn a background task that polls that PID and
    /// shuts down (killing every session) when it disappears.
    pub fn with_parent_watch(
        socket_path: PathBuf,
        data_dir: PathBuf,
        watch_parent: Option<u32>,
    ) -> Self {
        Daemon {
            session_manager: Arc::new(SessionManager::new(data_dir)),
            socket_path,
            token: String::new(),
            start_time: Instant::now(),
            parent_watch: Arc::new(Mutex::new(ParentWatchState::new(watch_parent))),
        }
    }

    /// Generate and persist a connection token.
    fn generate_token(&mut self) -> Result<()> {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        self.token = hex::encode(bytes);

        // Write token file next to socket: ~/.haven/daemon-{version}.token
        let token_path = self.socket_path.with_extension("token");
        std::fs::write(&token_path, &self.token)
            .with_context(|| format!("Failed to write token: {}", token_path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600))?;
        }

        tracing::info!("Auth token written to {}", token_path.display());
        Ok(())
    }

    /// Run the daemon, listening on the Unix socket.
    pub async fn run(&mut self) -> Result<()> {
        // Clean up stale socket
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path).ok();
        }

        // Ensure parent directory exists with proper permissions
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }

        // Generate auth token
        self.generate_token()?;

        let listener = UnixListener::bind(&self.socket_path)
            .with_context(|| format!("Failed to bind: {}", self.socket_path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600))?;
        }

        tracing::info!("Daemon listening on {}", self.socket_path.display());

        // Spawn the parent-watch task. It runs unconditionally — even when
        // no PID is set yet, the app may install one later via
        // `SetParentWatch`. The task is cheap when idle (one mutex lock and
        // a sleep per tick).
        spawn_parent_watch_task(self.parent_watch.clone(), self.session_manager.clone());

        let token = Arc::new(self.token.clone());

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let sm = self.session_manager.clone();
                    let start_time = self.start_time;
                    let token = token.clone();
                    let watch = self.parent_watch.clone();
                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_connection(stream, sm, start_time, &token, watch).await
                        {
                            tracing::debug!("Connection ended: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Accept error: {e}");
                }
            }
        }
    }
}

/// Background task: every `PARENT_WATCH_INTERVAL`, check whether the watched
/// PID is still alive. On disappearance, kill all sessions and exit. Honors
/// the grace window so an in-progress auto-update doesn't trip the watch.
fn spawn_parent_watch_task(
    state: Arc<Mutex<ParentWatchState>>,
    sm: Arc<SessionManager>,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(PARENT_WATCH_INTERVAL).await;

            let pid_to_check = {
                let mut s = state.lock().await;
                // Grace expiry: if we paused for an update and the new app
                // never showed up, fall back to the last known PID. The
                // next poll iteration will find that PID gone and trigger
                // the shutdown path — exactly what we want.
                if let Some(deadline) = s.grace_until {
                    if Instant::now() >= deadline {
                        s.grace_until = None;
                        if s.pid.is_none() {
                            s.pid = s.last_known_pid;
                            tracing::info!(
                                "parent-watch: grace expired, resuming watch on pid {:?}",
                                s.pid
                            );
                        }
                    }
                }
                s.pid
            };

            let Some(pid) = pid_to_check else {
                // Watching is disabled. Loop and re-check next tick — the
                // app may install a PID via SetParentWatch.
                continue;
            };

            if !pid_alive(pid) {
                tracing::warn!(
                    "parent-watch: parent pid {} is gone, shutting down daemon",
                    pid
                );
                sm.kill_all().await;
                // Give the OS a moment to actually deliver SIGTERM to the
                // PTY children before we yank the daemon.
                tokio::time::sleep(Duration::from_millis(150)).await;
                std::process::exit(0);
            }
        }
    });
}

/// Returns true if the given PID exists. Uses `kill(pid, 0)`:
///   * 0 → process exists and we have permission to signal it
///   * EPERM → process exists but we lack permission (still alive!)
///   * ESRCH → no such process
fn pid_alive(pid: u32) -> bool {
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if r == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Read a frame from the stream. Returns None on clean disconnect.
async fn read_frame(stream: &mut (impl AsyncReadExt + Unpin)) -> Result<Option<Frame>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }

    let len = u32::from_be_bytes(len_buf) as usize;
    if len < 5 || len > 16 * 1024 * 1024 {
        return Err(anyhow::anyhow!("Invalid frame length: {len}"));
    }

    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    let frame = Frame::decode(&body)?;
    Ok(Some(frame))
}

/// Write a frame to the stream.
async fn write_frame(stream: &mut (impl AsyncWriteExt + Unpin), frame: &Frame) -> Result<()> {
    let encoded = frame.encode();
    stream.write_all(&encoded).await?;
    stream.flush().await?;
    Ok(())
}

/// Handle a single client connection, multiplexing requests and events.
/// First frame must be an Auth request with the correct token.
async fn handle_connection(
    stream: UnixStream,
    sm: Arc<SessionManager>,
    start_time: Instant,
    expected_token: &str,
    parent_watch: Arc<Mutex<ParentWatchState>>,
) -> Result<()> {
    tracing::debug!("New client connection");

    let (mut reader, mut writer) = stream.into_split();

    // --- Token authentication: first frame must be Auth ---
    {
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(std::time::Duration::from_secs(5), reader.read_exact(&mut len_buf))
            .await
            .map_err(|_| anyhow::anyhow!("Auth timeout"))??;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > 1024 {
            return Err(anyhow::anyhow!("Auth frame too large"));
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).await?;
        let frame = Frame::decode(&body)?;

        // Expect a Request::Auth frame
        let req: Request = rmp_serde::from_slice(&frame.payload)
            .map_err(|_| anyhow::anyhow!("Invalid auth frame"))?;
        match req {
            Request::Auth { token } if token == expected_token => {
                // Authenticated — send success response
                let resp = Frame::response(frame.correlation_id, &Response::AuthOk)
                    .map_err(|e| anyhow::anyhow!("Frame encode: {e}"))?;
                let encoded = resp.encode();
                writer.write_all(&encoded).await?;
                writer.flush().await?;
                tracing::debug!("Client authenticated");
            }
            Request::Auth { .. } => {
                // Wrong token
                let resp = Frame::response(frame.correlation_id, &Response::Error(haven_protocol::HavenError::PermissionDenied { reason: "Authentication failed".to_string() }))
                    .map_err(|e| anyhow::anyhow!("Frame encode: {e}"))?;
                let encoded = resp.encode();
                writer.write_all(&encoded).await?;
                writer.flush().await?;
                return Err(anyhow::anyhow!("Client sent wrong auth token"));
            }
            _ => {
                // No auth frame sent
                let resp = Frame::response(frame.correlation_id, &Response::Error(haven_protocol::HavenError::PermissionDenied { reason: "Authentication required".to_string() }))
                    .map_err(|e| anyhow::anyhow!("Frame encode: {e}"))?;
                let encoded = resp.encode();
                writer.write_all(&encoded).await?;
                writer.flush().await?;
                return Err(anyhow::anyhow!("Client did not authenticate"));
            }
        }
    }

    // Channel for sending frames to the writer task
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel::<Frame>(256);

    // Writer task: drains frames and writes them to the socket
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = frame_rx.recv().await {
            let encoded = frame.encode();
            if writer.write_all(&encoded).await.is_err() {
                break;
            }
            if writer.flush().await.is_err() {
                break;
            }
        }
    });

    // Track per-session event forwarders (supports multiple simultaneous attachments)
    let mut event_forwarders: HashMap<Uuid, tokio::task::JoinHandle<()>> = HashMap::new();

    // Reader loop: read request frames and process them
    loop {
        let frame = match read_frame(&mut reader).await? {
            Some(f) => f,
            None => break,
        };

        if frame.frame_type != FrameType::Request {
            continue;
        }

        let correlation_id = frame.correlation_id;
        let request: Request = match rmp_serde::from_slice(&frame.payload) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::Error(HavenError::Internal {
                    reason: format!("Decode error: {e}"),
                });
                let f = Frame::response(correlation_id, &resp)?;
                let _ = frame_tx.send(f).await;
                continue;
            }
        };

        let response = match request {
            Request::Ping => Response::Pong {
                uptime_secs: start_time.elapsed().as_secs(),
                session_count: sm.count().await,
            },

            Request::DaemonStatus => Response::DaemonStatus {
                version: env!("CARGO_PKG_VERSION").to_string(),
                uptime_secs: start_time.elapsed().as_secs(),
                session_count: sm.count().await,
                pid: std::process::id(),
            },

            Request::SessionCreate(params) => match sm.create(params).await {
                Ok(info) => Response::SessionCreated(info),
                Err(e) => Response::Error(HavenError::PtySpawnFailed {
                    reason: e.to_string(),
                }),
            },

            Request::SessionList => Response::SessionList {
                sessions: sm.list().await,
            },

            Request::SessionAttach {
                session_id,
                history_bytes,
            } => {
                // Abort previous forwarder for THIS session only (re-attach)
                if let Some(handle) = event_forwarders.remove(&session_id) {
                    handle.abort();
                }

                // Send history first
                match sm.get_history(&session_id, history_bytes).await {
                    Ok(history) if !history.is_empty() => {
                        let evt = Event::Output {
                            session_id,
                            data: history,
                        };
                        if let Ok(f) = Frame::event(&evt) {
                            let _ = frame_tx.send(f).await;
                        }
                    }
                    _ => {}
                }

                // Subscribe to live output and forward as event frames
                match sm.subscribe(&session_id).await {
                    Ok(mut rx) => {
                        let tx = frame_tx.clone();
                        let sid = session_id;
                        let handle = tokio::spawn(async move {
                            loop {
                                match rx.recv().await {
                                    Ok(data) => {
                                        let evt = Event::Output {
                                            session_id: sid,
                                            data,
                                        };
                                        if let Ok(f) = Frame::event(&evt) {
                                            if tx.send(f).await.is_err() {
                                                break;
                                            }
                                        }
                                    }
                                    Err(broadcast::error::RecvError::Lagged(n)) => {
                                        tracing::warn!("Client lagged by {n} messages");
                                    }
                                    Err(broadcast::error::RecvError::Closed) => {
                                        let evt = Event::SessionExited {
                                            session_id: sid,
                                            exit_code: 0,
                                        };
                                        if let Ok(f) = Frame::event(&evt) {
                                            let _ = tx.send(f).await;
                                        }
                                        break;
                                    }
                                }
                            }
                        });
                        event_forwarders.insert(session_id, handle);
                        Response::SessionAttached { session_id }
                    }
                    Err(_) => Response::Error(HavenError::SessionNotFound {
                        session_id: session_id.to_string(),
                    }),
                }
            }

            Request::SessionDetach { session_id } => {
                if let Some(handle) = event_forwarders.remove(&session_id) {
                    handle.abort();
                }
                Response::SessionDetached
            }

            Request::SessionWrite { session_id, data } => {
                match sm.write(&session_id, &data).await {
                    Ok(_) => Response::Written { bytes: data.len() },
                    Err(_) => Response::Error(HavenError::SessionNotFound {
                        session_id: session_id.to_string(),
                    }),
                }
            }

            Request::SessionResize {
                session_id,
                cols,
                rows,
            } => match sm.resize(&session_id, cols, rows).await {
                Ok(_) => Response::Resized,
                Err(_) => Response::Error(HavenError::SessionNotFound {
                    session_id: session_id.to_string(),
                }),
            },

            Request::SessionKill {
                session_id,
                signal: _,
            } => match sm.kill(&session_id).await {
                Ok(_) => Response::SessionKilled,
                Err(_) => Response::Error(HavenError::SessionNotFound {
                    session_id: session_id.to_string(),
                }),
            },

            Request::SessionRename { session_id, name } => {
                match sm.rename(&session_id, name).await {
                    Ok(_) => Response::SessionRenamed,
                    Err(_) => Response::Error(HavenError::SessionNotFound {
                        session_id: session_id.to_string(),
                    }),
                }
            }

            Request::SessionSetWorkspace { session_id, workspace_id } => {
                match sm.set_workspace(&session_id, workspace_id).await {
                    Ok(_) => Response::WorkspaceSet,
                    Err(_) => Response::Error(HavenError::SessionNotFound {
                        session_id: session_id.to_string(),
                    }),
                }
            }

            Request::SessionHistory {
                session_id,
                offset,
                length,
            } => match sm.get(&session_id).await {
                Some(session) => {
                    let sess = session.lock().await;
                    match sess.transcript.read_range(offset, length) {
                        Ok(data) => Response::HistoryChunk {
                            data,
                            offset,
                            total: sess.transcript.total_bytes(),
                        },
                        Err(e) => Response::Error(HavenError::Internal {
                            reason: e.to_string(),
                        }),
                    }
                }
                None => Response::Error(HavenError::SessionNotFound {
                    session_id: session_id.to_string(),
                }),
            },

            Request::SessionSearchHistory {
                session_id,
                pattern,
                case_insensitive,
                regex,
                limit,
            } => match sm
                .search_history(
                    &session_id,
                    &pattern,
                    case_insensitive,
                    regex,
                    limit as usize,
                )
                .await
            {
                Ok(results) => Response::SearchHistoryResults(results),
                Err(e) => Response::Error(HavenError::Internal {
                    reason: e.to_string(),
                }),
            },

            // Auth is handled at connection start; if received again, just ack it
            Request::Auth { .. } => Response::AuthOk,

            Request::SetParentWatch { pid, grace_secs } => {
                let mut s = parent_watch.lock().await;
                s.pid = pid;
                if let Some(p) = pid {
                    s.last_known_pid = Some(p);
                }
                s.grace_until = grace_secs.map(|g| Instant::now() + Duration::from_secs(g));
                tracing::info!(
                    "SetParentWatch: pid={:?} grace_secs={:?} last_known={:?}",
                    s.pid,
                    grace_secs,
                    s.last_known_pid
                );
                Response::ParentWatchUpdated { watching: s.pid }
            }
        };

        let resp_frame = Frame::response(correlation_id, &response)?;
        if frame_tx.send(resp_frame).await.is_err() {
            break;
        }
    }

    // Cleanup: abort all forwarders
    for (_, handle) in event_forwarders.drain() {
        handle.abort();
    }
    drop(frame_tx);
    let _ = writer_task.await;

    Ok(())
}
