use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// Unique identifier for a session.
pub type SessionId = Uuid;

/// A single match from a transcript search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSearchMatch {
    /// Byte offset into the decrypted plaintext stream where the match starts.
    pub offset: u64,
    /// 1-indexed line number in the plaintext stream.
    pub line_number: u64,
    /// Containing line with ANSI escape sequences stripped.
    pub preview: String,
}

/// Result of a transcript search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSearchResults {
    pub matches: Vec<TranscriptSearchMatch>,
    pub total: usize,
    pub truncated: bool,
}

/// The kind of session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum SessionKind {
    /// A regular shell session.
    Shell,
    /// An agent session launched from a template.
    Agent { template: String },
}

/// Current status of a session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Session is running and has recent activity.
    Running,
    /// Session is running but idle (no recent output).
    Idle,
    /// Session process has exited.
    Exited,
    /// Session is suspended (e.g., SIGSTOP).
    Suspended,
}

/// Full metadata about a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: String,
    pub host_id: String,
    pub shell: String,
    pub cwd: Option<PathBuf>,
    pub status: SessionStatus,
    pub kind: SessionKind,
    pub created_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub tags: Vec<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

/// Parameters for creating a new session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCreate {
    pub name: Option<String>,
    pub shell: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: HashMap<String, String>,
    pub kind: SessionKind,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub workspace_id: Option<String>,
}

/// A session template defining how to launch a particular type of session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTemplate {
    /// Unique identifier for this template.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Command to run (e.g., "/bin/zsh", "claude", "codex").
    pub command: String,
    /// Command arguments.
    pub args: Vec<String>,
    /// Environment variables to set.
    pub env: HashMap<String, String>,
    /// Working directory.
    pub cwd: Option<PathBuf>,
    /// Session kind.
    pub kind: SessionKind,
    /// Icon identifier (for UI).
    pub icon: Option<String>,
    /// Description.
    pub description: Option<String>,
}

impl SessionTemplate {
    /// Built-in shell template.
    pub fn shell() -> Self {
        Self {
            id: "shell".into(),
            name: "Shell".into(),
            command: std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into()),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            kind: SessionKind::Shell,
            icon: Some("terminal".into()),
            description: Some("Default shell session".into()),
        }
    }

    /// Built-in Claude Code template.
    pub fn claude_code() -> Self {
        Self {
            id: "claude-code".into(),
            name: "Claude Code".into(),
            command: "claude".into(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            kind: SessionKind::Agent {
                template: "claude-code".into(),
            },
            icon: Some("brain".into()),
            description: Some("Claude Code AI assistant".into()),
        }
    }

    /// Built-in Codex template.
    pub fn codex() -> Self {
        Self {
            id: "codex".into(),
            name: "Codex".into(),
            command: "codex".into(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            kind: SessionKind::Agent {
                template: "codex".into(),
            },
            icon: Some("code".into()),
            description: Some("OpenAI Codex agent".into()),
        }
    }

    /// Return all built-in templates.
    pub fn builtins() -> Vec<Self> {
        vec![Self::shell(), Self::claude_code(), Self::codex()]
    }
}

impl Default for SessionCreate {
    fn default() -> Self {
        Self {
            name: None,
            shell: None,
            cwd: None,
            env: HashMap::new(),
            kind: SessionKind::Shell,
            cols: 80,
            rows: 24,
            workspace_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_remote_session_json() {
        let json = r#"{
            "id": "ec89a069-fb45-4646-9f4b-f11e3460ef7e",
            "name": "lambda-h100",
            "host_id": "local",
            "shell": "/bin/bash",
            "cwd": "/home/ubuntu",
            "status": "running",
            "kind": { "type": "Shell" },
            "created_at": "2026-03-29T12:24:26.451836629Z",
            "last_activity": "2026-03-29T12:24:26.451837281Z",
            "pid": 2652242,
            "exit_code": null,
            "tags": []
        }"#;
        let info: SessionInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.name, "lambda-h100");
        assert_eq!(info.kind, SessionKind::Shell);
    }
}
