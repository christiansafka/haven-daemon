use serde::{Deserialize, Serialize};

/// Unique identifier for a host. "local" for the local machine.
pub type HostId = String;

/// SSH authentication method.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum AuthMethod {
    /// Use SSH agent for authentication.
    Agent,
    /// Use a key file.
    KeyFile { path: String },
    /// Use password authentication.
    Password,
}

/// Information about a managed host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub id: HostId,
    pub name: String,
    pub hostname: String,
    pub port: u16,
    pub username: String,
    pub auth_method: AuthMethod,
    pub platform: Option<String>,
    pub daemon_version: Option<String>,
    pub default_shell: Option<String>,
    pub workspace_path: Option<String>,
    pub labels: Vec<String>,
}

impl HostInfo {
    /// Create a host info for the local machine.
    pub fn local() -> Self {
        Self {
            id: "local".to_string(),
            name: "Local".to_string(),
            hostname: "localhost".to_string(),
            port: 0,
            username: whoami(),
            auth_method: AuthMethod::Agent,
            platform: Some(std::env::consts::OS.to_string()),
            daemon_version: None,
            default_shell: None,
            workspace_path: None,
            labels: vec![],
        }
    }
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}
