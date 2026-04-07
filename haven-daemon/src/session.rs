use anyhow::{Context, Result};
use chrono::Utc;
use haven_protocol::{SessionCreate, SessionId, SessionInfo, SessionStatus};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, RwLock};
use uuid::Uuid;

use crate::history::{SearchResults, TranscriptWriter};
use crate::pty::PtyHandle;

/// A live session with its PTY and transcript.
pub struct Session {
    pub info: SessionInfo,
    pub pty: PtyHandle,
    pub transcript: TranscriptWriter,
    /// Background task that writes PTY output to transcript
    transcript_task: Option<tokio::task::JoinHandle<()>>,
}

/// Manages all sessions in the daemon.
pub struct SessionManager {
    sessions: RwLock<HashMap<SessionId, Arc<Mutex<Session>>>>,
    data_dir: PathBuf,
    default_shell: String,
}

impl SessionManager {
    pub fn new(data_dir: PathBuf) -> Self {
        let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        SessionManager {
            sessions: RwLock::new(HashMap::new()),
            data_dir,
            default_shell,
        }
    }

    /// Create a new session.
    pub async fn create(&self, params: SessionCreate) -> Result<SessionInfo> {
        let id = Uuid::new_v4();
        let shell = params.shell.unwrap_or_else(|| self.default_shell.clone());
        let name = params.name.unwrap_or_else(|| {
            let count = {
                // Use a simple counter based on existing sessions
                // We can't await inside this closure easily, so use a default
                format!("Session {}", id.as_simple().to_string()[..4].to_uppercase())
            };
            count
        });

        let session_dir = self.data_dir.join("sessions").join(id.to_string());
        let cwd = params.cwd.or_else(|| {
            dirs::home_dir()
        });

        // Spawn PTY
        let pty = PtyHandle::spawn(&shell, cwd.as_ref(), &params.env, params.cols, params.rows)
            .context("Failed to spawn PTY")?;

        let pid = pty.pid();

        // Create transcript writer
        let transcript = TranscriptWriter::new(&session_dir)
            .context("Failed to create transcript writer")?;

        let info = SessionInfo {
            id,
            name,
            host_id: "local".to_string(),
            shell,
            cwd,
            status: SessionStatus::Running,
            kind: params.kind,
            created_at: Utc::now(),
            last_activity: Utc::now(),
            pid,
            exit_code: None,
            tags: vec![],
        };

        // Subscribe to PTY output for transcript writing
        let mut output_rx = pty.subscribe();
        let session = Arc::new(Mutex::new(Session {
            info: info.clone(),
            pty,
            transcript,
            transcript_task: None,
        }));

        // Spawn transcript writer task
        let session_clone = session.clone();
        let transcript_task = tokio::spawn(async move {
            loop {
                match output_rx.recv().await {
                    Ok(data) => {
                        let mut sess = session_clone.lock().await;
                        sess.info.last_activity = Utc::now();
                        if let Err(e) = sess.transcript.append(&data) {
                            tracing::error!("Failed to write transcript: {e}");
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("Transcript writer lagged by {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::debug!("PTY output channel closed, stopping transcript writer");
                        // Mark session as exited
                        let mut sess = session_clone.lock().await;
                        sess.info.status = SessionStatus::Exited;
                        break;
                    }
                }
            }
        });

        {
            let mut sess = session.lock().await;
            sess.transcript_task = Some(transcript_task);
        }

        // Store session
        {
            let mut sessions = self.sessions.write().await;
            sessions.insert(id, session);
        }

        tracing::info!("Created session {id} (name={}, pid={pid:?})", info.name);
        Ok(info)
    }

    /// List all sessions.
    pub async fn list(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.read().await;
        let mut infos = Vec::new();
        for session in sessions.values() {
            let sess = session.lock().await;
            infos.push(sess.info.clone());
        }
        infos
    }

    /// Get a session by ID.
    pub async fn get(&self, id: &SessionId) -> Option<Arc<Mutex<Session>>> {
        let sessions = self.sessions.read().await;
        sessions.get(id).cloned()
    }

    /// Write data to a session's PTY.
    pub async fn write(&self, id: &SessionId, data: &[u8]) -> Result<()> {
        let session = self
            .get(id)
            .await
            .ok_or_else(|| anyhow::anyhow!("Session not found: {id}"))?;
        let sess = session.lock().await;
        sess.pty.write(data)?;
        Ok(())
    }

    /// Resize a session's PTY.
    pub async fn resize(&self, id: &SessionId, cols: u16, rows: u16) -> Result<()> {
        let session = self
            .get(id)
            .await
            .ok_or_else(|| anyhow::anyhow!("Session not found: {id}"))?;
        let sess = session.lock().await;
        sess.pty.resize(cols, rows)?;
        Ok(())
    }

    /// Subscribe to a session's output.
    pub async fn subscribe(
        &self,
        id: &SessionId,
    ) -> Result<broadcast::Receiver<Vec<u8>>> {
        let session = self
            .get(id)
            .await
            .ok_or_else(|| anyhow::anyhow!("Session not found: {id}"))?;
        let sess = session.lock().await;
        Ok(sess.pty.subscribe())
    }

    /// Search a session's full transcript.
    pub async fn search_history(
        &self,
        id: &SessionId,
        pattern: &str,
        case_insensitive: bool,
        regex: bool,
        limit: usize,
    ) -> Result<SearchResults> {
        let session = self
            .get(id)
            .await
            .ok_or_else(|| anyhow::anyhow!("Session not found: {id}"))?;
        let sess = session.lock().await;
        sess.transcript.search(pattern, case_insensitive, regex, limit)
    }

    /// Get recent history for a session.
    pub async fn get_history(&self, id: &SessionId, bytes: u64) -> Result<Vec<u8>> {
        let session = self
            .get(id)
            .await
            .ok_or_else(|| anyhow::anyhow!("Session not found: {id}"))?;
        let sess = session.lock().await;
        sess.transcript.read_last(bytes)
    }

    /// Kill a session.
    pub async fn kill(&self, id: &SessionId) -> Result<()> {
        let session = {
            let mut sessions = self.sessions.write().await;
            sessions
                .remove(id)
                .ok_or_else(|| anyhow::anyhow!("Session not found: {id}"))?
        };
        let mut sess = session.lock().await;
        sess.pty.kill()?;
        sess.info.status = SessionStatus::Exited;
        if let Some(task) = sess.transcript_task.take() {
            task.abort();
        }
        tracing::info!("Killed session {id}");
        Ok(())
    }

    /// Rename a session.
    pub async fn rename(&self, id: &SessionId, name: String) -> Result<()> {
        let session = self
            .get(id)
            .await
            .ok_or_else(|| anyhow::anyhow!("Session not found: {id}"))?;
        let mut sess = session.lock().await;
        sess.info.name = name;
        Ok(())
    }

    /// Get the session count.
    pub async fn count(&self) -> usize {
        self.sessions.read().await.len()
    }
}

