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

        /// Watch a parent PID. When that PID disappears, the daemon kills
        /// every session and exits — used by haven-app to make local sessions
        /// die with the app on crash/quit. Omit for a fully detached daemon
        /// (the default for `haven-session-daemon daemon` from a shell).
        #[arg(long)]
        watch_parent: Option<u32>,
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

        /// Workspace ID to tag this session with. Also automatically
        /// injected as `HAVEN_WORKSPACE_ID` into the session's shell env
        /// (agents read it from there).
        #[arg(long = "workspace", value_name = "ID")]
        workspace: Option<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// List all sessions
    List {
        /// Output as JSON
        #[arg(long)]
        json: bool,

        /// Only show sessions tagged with this workspace ID. Sessions with
        /// no workspace tag (pre-Stage-1 sessions) are always shown.
        #[arg(long = "workspace", value_name = "ID")]
        workspace: Option<String>,
    },

    /// Attach to a session (stream I/O)
    Attach {
        /// Session ID
        id: String,

        /// Bytes of history to replay
        #[arg(long, default_value_t = 1_048_576)]
        history_bytes: u64,

        /// Non-interactive pipe mode for programmatic drivers (e.g. haven-app
        /// over an SSH exec channel without a PTY). Disables raw mode, chord
        /// keys, and SIGWINCH handling; stdin bytes pass straight through.
        #[arg(long)]
        pipe: bool,
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

    /// Append a single Claude Code hook event to the session's encrypted
    /// activity log. Reads the JSON payload from stdin (one event per
    /// invocation). A trailing newline is added if absent so the on-disk
    /// log stays valid JSONL. Best-effort: failures are logged and exit 0
    /// so the calling hook script never blocks the agent.
    RecordActivity {
        /// Session ID
        id: String,
    },

    /// Read a tail of the session's encrypted activity log as JSONL.
    /// Prints `{"start":N,"total":M}` on the first line followed by the
    /// decrypted JSONL payload. Used by haven-app to seed the activity
    /// timeline on session attach (and to page older history).
    Activity {
        /// Session ID
        id: String,

        /// How many bytes of activity to return.
        #[arg(long, default_value_t = 65_536)]
        tail_bytes: u64,

        /// When set, return bytes ending strictly before this byte offset
        /// in the activity log's plaintext stream. Used to page older.
        #[arg(long)]
        before: Option<u64>,
    },

    /// Read selected env vars from a session's spawn-time environment.
    /// Used by haven-app over SSH to recover per-session secrets like
    /// HAVEN_SESSION_TOKEN after its own restart.
    Env {
        /// Session ID
        id: String,

        /// Env var names to read (repeatable). If none given, returns all.
        #[arg(long = "key", value_name = "KEY")]
        keys: Vec<String>,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}
