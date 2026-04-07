use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "haven-session-daemon",
    about = "Haven session daemon - persistent terminal session manager",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Path to the Unix socket
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,

    /// Path to the data directory
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run the daemon (Unix socket server)
    Daemon {
        /// Run in foreground (don't daemonize)
        #[arg(long, default_value_t = true)]
        foreground: bool,
    },

    /// Manage sessions
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionAction {
    /// Create a new session
    Create {
        /// Session name
        #[arg(short, long)]
        name: Option<String>,

        /// Shell to use
        #[arg(short, long)]
        shell: Option<String>,

        /// Working directory
        #[arg(short, long)]
        cwd: Option<PathBuf>,

        /// Terminal columns
        #[arg(long, default_value_t = 80)]
        cols: u16,

        /// Terminal rows
        #[arg(long, default_value_t = 24)]
        rows: u16,

        /// Extra environment variables for the session shell (KEY=VALUE, repeatable).
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// List all sessions
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Attach to a session (stream I/O)
    Attach {
        /// Session ID
        id: String,

        /// Bytes of history to replay
        #[arg(long, default_value_t = 1_048_576)]
        history_bytes: u64,
    },

    /// Resize a session's PTY
    Resize {
        /// Session ID
        id: String,

        /// Terminal columns
        #[arg(long)]
        cols: u16,

        /// Terminal rows
        #[arg(long)]
        rows: u16,
    },

    /// Kill a session
    Kill {
        /// Session ID
        id: String,
    },

    /// Rename a session
    Rename {
        /// Session ID
        id: String,

        /// New name
        name: String,
    },
}
