//! The `haven` CLI — a terse, interactive frontend for browsing and entering
//! sessions on the local machine. Shares one multicall binary with the daemon
//! (dispatched on `argv[0]`).
//!
//! Philosophy: `haven` with no arguments should always do the right thing.
//! No sessions → create one. One session → attach. Multiple → picker. Every
//! sub-command should be a single word.

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand};
use haven_client::{
    connect_daemon, ensure_daemon_running, run_attach, send_request, AttachOptions,
    AttachOutcome,
};
use haven_protocol::{Request, Response, SessionCreate, SessionId, SessionInfo, SessionStatus};
use std::path::{Path, PathBuf};

use crate::picker::{self, PickerResult};

#[derive(Parser, Debug)]
#[command(
    name = "haven",
    about = "Browse and attach to your Haven sessions from any terminal",
    version
)]
struct HavenCli {
    /// Path to the daemon Unix socket (defaults to ~/.haven/daemon.sock)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<HavenCommand>,
}

#[derive(Subcommand, Debug)]
enum HavenCommand {
    /// List all sessions
    #[command(alias = "list")]
    Ls {
        /// Output as JSON (for scripting)
        #[arg(long)]
        json: bool,
    },

    /// Create a new session and attach to it
    New {
        /// Session name (optional)
        name: Option<String>,
        /// Shell to use (defaults to $SHELL)
        #[arg(long)]
        shell: Option<String>,
        /// Working directory
        #[arg(long)]
        cwd: Option<PathBuf>,
    },

    /// Attach to a session by id (prefix), name, or index from `haven ls`
    #[command(alias = "a")]
    Attach {
        /// Session id prefix, name, or 1-based index from the last `haven ls`
        target: String,
    },

    /// Kill a session
    Kill {
        /// Session id prefix, name, or index
        target: String,
    },

    /// Rename a session
    Rename {
        /// Session id prefix, name, or index
        target: String,
        /// New name
        name: String,
    },
}

/// Entry point used by the multicall dispatcher in `main.rs`.
pub fn run(args: Vec<String>) -> ! {
    // Deliberately do NOT init tracing-subscriber here: tracing writes to
    // stderr by default, which collides with raw mode. The daemon subcommand
    // owns tracing; the CLI stays quiet.
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("haven: failed to start tokio runtime: {e}");
            std::process::exit(1);
        }
    };

    let exit = rt.block_on(async {
        match run_async(args).await {
            Ok(code) => code,
            Err(e) => {
                eprintln!("haven: {e:#}");
                1
            }
        }
    });
    std::process::exit(exit);
}

async fn run_async(args: Vec<String>) -> Result<i32> {
    let cli = HavenCli::parse_from(args);
    let socket_path = cli
        .socket
        .clone()
        .unwrap_or_else(haven_protocol::default_socket_path);

    // Auto-spawn the daemon if needed. The current exe is the multicall
    // binary, so we pass it as the daemon binary hint — when the user
    // installs via our scripts, `haven` is a symlink to the daemon binary,
    // and `current_exe()` on Linux resolves symlinks, giving us the real path.
    let self_exe = std::env::current_exe().ok();
    if let Err(e) = ensure_daemon_running(&socket_path, self_exe.as_deref()).await {
        bail!(
            "daemon is not running and could not be started: {e}\n\
             Try starting it manually: haven-session-daemon daemon"
        );
    }

    match cli.command {
        None => interactive_default(&socket_path).await,
        Some(HavenCommand::Ls { json }) => cmd_ls(&socket_path, json).await,
        Some(HavenCommand::New { name, shell, cwd }) => {
            let info = create_session(&socket_path, name, shell, cwd).await?;
            attach_loop(&socket_path, info.id, true).await
        }
        Some(HavenCommand::Attach { target }) => cmd_attach(&socket_path, target).await,
        Some(HavenCommand::Kill { target }) => cmd_kill(&socket_path, target).await,
        Some(HavenCommand::Rename { target, name }) => {
            cmd_rename(&socket_path, target, name).await
        }
    }
}

// ---------------------------------------------------------------------------
// No-args behavior: context-aware "do the right thing".
// ---------------------------------------------------------------------------

async fn interactive_default(socket_path: &Path) -> Result<i32> {
    let sessions = list_sessions(socket_path).await?;

    match sessions.len() {
        0 => {
            eprintln!("[haven] no sessions yet — creating one");
            let info = create_session(socket_path, None, None, None).await?;
            attach_loop(socket_path, info.id, true).await
        }
        1 => {
            let id = sessions[0].id;
            attach_loop(socket_path, id, true).await
        }
        _ => picker_loop(socket_path, sessions).await,
    }
}

// ---------------------------------------------------------------------------
// Sub-command implementations.
// ---------------------------------------------------------------------------

async fn cmd_ls(socket_path: &Path, json: bool) -> Result<i32> {
    let sessions = list_sessions(socket_path).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&sessions)?);
        return Ok(0);
    }
    if sessions.is_empty() {
        println!("No sessions. Run `haven new` to create one.");
        return Ok(0);
    }
    print_session_table(&sessions);
    Ok(0)
}

async fn cmd_attach(socket_path: &Path, target: String) -> Result<i32> {
    let sessions = list_sessions(socket_path).await?;
    let id = resolve_target(&sessions, &target)?;
    attach_loop(socket_path, id, true).await
}

async fn cmd_kill(socket_path: &Path, target: String) -> Result<i32> {
    let sessions = list_sessions(socket_path).await?;
    let id = resolve_target(&sessions, &target)?;
    let mut stream = connect_daemon(socket_path).await?;
    let req = Request::SessionKill {
        session_id: id,
        signal: None,
    };
    match send_request(&mut stream, 1, &req).await? {
        Response::SessionKilled => {
            println!("killed {}", short_id(id));
            Ok(0)
        }
        Response::Error(e) => Err(anyhow!("{e}")),
        _ => Err(anyhow!("unexpected response")),
    }
}

async fn cmd_rename(socket_path: &Path, target: String, name: String) -> Result<i32> {
    let sessions = list_sessions(socket_path).await?;
    let id = resolve_target(&sessions, &target)?;
    let mut stream = connect_daemon(socket_path).await?;
    let req = Request::SessionRename {
        session_id: id,
        name: name.clone(),
    };
    match send_request(&mut stream, 1, &req).await? {
        Response::SessionRenamed => {
            println!("renamed {} → {}", short_id(id), name);
            Ok(0)
        }
        Response::Error(e) => Err(anyhow!("{e}")),
        _ => Err(anyhow!("unexpected response")),
    }
}

// ---------------------------------------------------------------------------
// Attach / picker loop. The attach loop returns an outcome that may tell us
// to re-enter the picker or create a new session, so we wrap it in a loop
// here instead of baking those transitions into haven-client.
// ---------------------------------------------------------------------------

/// Main interactive loop: attach to `id`, react to the outcome, and
/// potentially jump to another session (Switch) or create a new one
/// (NewSession) without leaving the CLI. All transitions stay inside this
/// function to avoid recursive async fns.
async fn attach_loop(socket_path: &Path, mut id: SessionId, print_hint: bool) -> Result<i32> {
    let mut hint = print_hint;
    loop {
        let stream = connect_daemon(socket_path).await?;
        let opts = AttachOptions {
            print_hint: hint,
            ..AttachOptions::default()
        };
        let outcome = run_attach(stream, id, opts).await?;
        hint = false; // only print the hint on the first attach of a run

        match outcome {
            AttachOutcome::Exited(code) => {
                eprintln!("\r\n[haven] session exited ({code})");
                return Ok(code);
            }
            AttachOutcome::Detached => {
                eprintln!("\r\n[haven] detached");
                return Ok(0);
            }
            AttachOutcome::Disconnected => {
                eprintln!("\r\n[haven] disconnected from daemon");
                return Ok(1);
            }
            AttachOutcome::Switch => {
                let sessions = list_sessions(socket_path).await?;
                match picker::pick(&sessions)? {
                    PickerResult::Attach(idx) => {
                        id = sessions[idx].id;
                        continue;
                    }
                    PickerResult::New => {
                        let info = create_session(socket_path, None, None, None).await?;
                        id = info.id;
                        continue;
                    }
                    PickerResult::Quit => return Ok(0),
                }
            }
            AttachOutcome::NewSession => {
                let info = create_session(socket_path, None, None, None).await?;
                id = info.id;
                continue;
            }
        }
    }
}

async fn picker_loop(socket_path: &Path, sessions: Vec<SessionInfo>) -> Result<i32> {
    match picker::pick(&sessions)? {
        PickerResult::Attach(idx) => {
            let id = sessions[idx].id;
            attach_loop(socket_path, id, true).await
        }
        PickerResult::New => {
            let info = create_session(socket_path, None, None, None).await?;
            attach_loop(socket_path, info.id, true).await
        }
        PickerResult::Quit => Ok(0),
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

async fn list_sessions(socket_path: &Path) -> Result<Vec<SessionInfo>> {
    let mut stream = connect_daemon(socket_path).await?;
    match send_request(&mut stream, 1, &Request::SessionList).await? {
        Response::SessionList { sessions } => Ok(sessions),
        Response::Error(e) => Err(anyhow!("{e}")),
        _ => Err(anyhow!("unexpected response to SessionList")),
    }
}

async fn create_session(
    socket_path: &Path,
    name: Option<String>,
    shell: Option<String>,
    cwd: Option<PathBuf>,
) -> Result<SessionInfo> {
    // Use the current terminal's size so the initial PTY is already the
    // right shape. run_attach will send another resize on attach, but this
    // avoids a momentary 80x24 flash on programs that react to resize
    // events.
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut stream = connect_daemon(socket_path).await?;
    let req = Request::SessionCreate(SessionCreate {
        name,
        shell,
        cwd,
        cols,
        rows,
        ..Default::default()
    });
    match send_request(&mut stream, 1, &req).await? {
        Response::SessionCreated(info) => Ok(info),
        Response::Error(e) => Err(anyhow!("{e}")),
        _ => Err(anyhow!("unexpected response to SessionCreate")),
    }
}

/// Resolve a user-supplied target string to a session id. The target may be:
///   - A full UUID
///   - A UUID prefix (>= 4 chars, unambiguous)
///   - A session name (exact match)
///   - A 1-based index into the list (matching `haven ls` display order)
fn resolve_target(sessions: &[SessionInfo], target: &str) -> Result<SessionId> {
    if sessions.is_empty() {
        bail!("no sessions");
    }

    // 1-based index.
    if let Ok(idx) = target.parse::<usize>() {
        if idx >= 1 && idx <= sessions.len() {
            return Ok(sessions[idx - 1].id);
        }
    }

    // Exact name match.
    let name_matches: Vec<_> = sessions.iter().filter(|s| s.name == target).collect();
    if name_matches.len() == 1 {
        return Ok(name_matches[0].id);
    }
    if name_matches.len() > 1 {
        bail!(
            "name '{target}' matches {} sessions — use an id prefix instead",
            name_matches.len()
        );
    }

    // Full UUID.
    if let Ok(id) = target.parse::<SessionId>() {
        if sessions.iter().any(|s| s.id == id) {
            return Ok(id);
        }
        bail!("no session with id {target}");
    }

    // UUID prefix (minimum 4 chars to avoid spurious collisions).
    if target.len() >= 4 {
        let prefix_matches: Vec<_> = sessions
            .iter()
            .filter(|s| s.id.to_string().starts_with(target))
            .collect();
        if prefix_matches.len() == 1 {
            return Ok(prefix_matches[0].id);
        }
        if prefix_matches.len() > 1 {
            bail!(
                "prefix '{target}' matches {} sessions — be more specific",
                prefix_matches.len()
            );
        }
    }

    bail!("no session matching '{target}' (try `haven ls`)")
}

fn short_id(id: SessionId) -> String {
    id.to_string()[..8].to_string()
}

fn print_session_table(sessions: &[SessionInfo]) {
    // Header.
    println!("  {:>2}  {:<24}  {:<8}  {}", "#", "NAME", "STATUS", "CWD");
    for (i, s) in sessions.iter().enumerate() {
        let cwd = s
            .cwd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".into());
        println!(
            "  {:>2}  {:<24}  {:<8}  {}",
            i + 1,
            truncate(&s.name, 24),
            status_label(s.status),
            cwd,
        );
    }
}

fn status_label(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Running => "running",
        SessionStatus::Idle => "idle",
        SessionStatus::Exited => "exited",
        SessionStatus::Suspended => "paused",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
