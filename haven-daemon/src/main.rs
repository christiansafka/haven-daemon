mod cli;
mod cli_haven;
mod daemon;
mod history;
mod picker;
mod protocol;
mod pty;
mod session;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands, SessionAction};
use haven_client::{
    connect_daemon, run_attach, send_request, AttachOptions, AttachOutcome,
};
use haven_protocol::*;
use std::path::PathBuf;

/// Decide whether this invocation should dispatch to the `haven` CLI based on
/// `argv[0]`. Two triggers:
///   1. The binary was invoked as `haven` (most commonly via a symlink next
///      to `haven-session-daemon`).
///   2. The invocation is `haven-session-daemon haven ...` — a convenience
///      for development so the CLI is reachable without installing a symlink.
fn should_run_as_haven_cli(args: &[String]) -> bool {
    if let Some(arg0) = args.first() {
        let base = std::path::Path::new(arg0)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if base == "haven" {
            return true;
        }
    }
    matches!(args.get(1).map(String::as_str), Some("haven"))
}

fn main() {
    let raw_args: Vec<String> = std::env::args().collect();

    // --- Multicall dispatch: `haven` → cli_haven::run ---
    if should_run_as_haven_cli(&raw_args) {
        // Normalize argv so clap sees "haven" as the program name regardless
        // of how we were invoked.
        let mut haven_args = vec!["haven".to_string()];
        if raw_args.first().map(|s| {
            std::path::Path::new(s)
                .file_stem()
                .and_then(|f| f.to_str())
                .map(|f| f != "haven")
                .unwrap_or(true)
        }) == Some(true)
        {
            // Invocation was `haven-session-daemon haven ...`; skip raw_args[0]
            // and raw_args[1] ("haven"), pass the rest.
            haven_args.extend(raw_args.into_iter().skip(2));
        } else {
            // Invocation was `haven ...`; skip just raw_args[0].
            haven_args.extend(raw_args.into_iter().skip(1));
        }
        cli_haven::run(haven_args);
    }

    // --- Otherwise run as haven-session-daemon ---
    let cli = Cli::parse();

    // Tracing is only initialized for the daemon-facing CLI. The `haven` path
    // above skips this because tracing writes to stderr and would smash raw
    // mode during an attach.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("haven=info".parse().unwrap()),
        )
        .init();

    let data_dir = cli
        .data_dir
        .unwrap_or_else(|| haven_protocol::default_data_dir());
    let socket_path = cli
        .socket
        .unwrap_or_else(|| haven_protocol::default_socket_path());

    match cli.command {
        Commands::Daemon { foreground: _ } => {
            tracing::info!("Haven session daemon v{}", env!("CARGO_PKG_VERSION"));

            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            rt.block_on(async {
                let mut daemon = daemon::Daemon::new(socket_path, data_dir);
                if let Err(e) = daemon.run().await {
                    tracing::error!("Daemon error: {e}");
                    std::process::exit(1);
                }
            });
        }
        Commands::Session { action } => {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
            rt.block_on(async {
                if let Err(e) = handle_cli_action(action, &socket_path).await {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            });
        }
    }
}

async fn handle_cli_action(action: SessionAction, socket_path: &PathBuf) -> Result<()> {
    match action {
        SessionAction::Create {
            name, shell, cwd, cols, rows, env, json,
        } => {
            let mut stream = connect_daemon(socket_path).await?;
            let mut env_map = std::collections::HashMap::new();
            for entry in env {
                if let Some((k, v)) = entry.split_once('=') {
                    env_map.insert(k.to_string(), v.to_string());
                } else {
                    eprintln!("Warning: ignoring --env '{entry}' (expected KEY=VALUE)");
                }
            }
            let req = Request::SessionCreate(SessionCreate {
                name, shell, cwd, cols, rows, env: env_map,
                ..Default::default()
            });
            match send_request(&mut stream, 1, &req).await? {
                Response::SessionCreated(info) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&info)?);
                    } else {
                        println!("Created session: {} ({})", info.name, info.id);
                    }
                }
                Response::Error(e) => eprintln!("Error: {e}"),
                _ => eprintln!("Unexpected response"),
            }
        }

        SessionAction::List { json } => {
            let mut stream = connect_daemon(socket_path).await?;
            match send_request(&mut stream, 1, &Request::SessionList).await? {
                Response::SessionList { sessions } => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&sessions)?);
                    } else if sessions.is_empty() {
                        println!("No sessions.");
                    } else {
                        for s in &sessions {
                            println!(
                                "  {} {:8} {:?} {}",
                                &s.id.to_string()[..8],
                                s.name,
                                s.status,
                                s.cwd.as_ref().map(|p| p.display().to_string()).unwrap_or_default()
                            );
                        }
                    }
                }
                Response::Error(e) => eprintln!("Error: {e}"),
                _ => eprintln!("Unexpected response"),
            }
        }

        SessionAction::Attach { id, history_bytes } => {
            let session_id: uuid::Uuid = id.parse()
                .map_err(|e| anyhow::anyhow!("Invalid session ID: {e}"))?;
            let stream = connect_daemon(socket_path).await?;

            // Delegate to haven-client's real attach loop (raw mode, SIGWINCH,
            // chord keys — all of it). The daemon CLI gets the same behavior
            // as the `haven` CLI, not the previous smoke-test implementation.
            let opts = AttachOptions {
                history_bytes,
                print_hint: false,
            };
            match run_attach(stream, session_id, opts).await? {
                AttachOutcome::Exited(code) => std::process::exit(code),
                AttachOutcome::Detached => {}
                AttachOutcome::Switch | AttachOutcome::NewSession => {
                    // The daemon CLI doesn't know what to do with these
                    // (it has no picker), so treat them as detach.
                    eprintln!("\r\n[haven-session-daemon] detached");
                }
                AttachOutcome::Disconnected => std::process::exit(1),
            }
        }

        SessionAction::Resize { id, cols, rows } => {
            let session_id: uuid::Uuid = id.parse()
                .map_err(|e| anyhow::anyhow!("Invalid session ID: {e}"))?;
            let mut stream = connect_daemon(socket_path).await?;
            let req = Request::SessionResize { session_id, cols, rows };
            match send_request(&mut stream, 1, &req).await? {
                Response::Resized => {}
                Response::Error(e) => eprintln!("Error: {e}"),
                _ => eprintln!("Unexpected response"),
            }
        }

        SessionAction::Kill { id } => {
            let session_id: uuid::Uuid = id.parse()
                .map_err(|e| anyhow::anyhow!("Invalid session ID: {e}"))?;
            let mut stream = connect_daemon(socket_path).await?;
            let req = Request::SessionKill { session_id, signal: None };
            match send_request(&mut stream, 1, &req).await? {
                Response::SessionKilled => println!("Session killed."),
                Response::Error(e) => eprintln!("Error: {e}"),
                _ => eprintln!("Unexpected response"),
            }
        }

        SessionAction::Rename { id, name } => {
            let session_id: uuid::Uuid = id.parse()
                .map_err(|e| anyhow::anyhow!("Invalid session ID: {e}"))?;
            let mut stream = connect_daemon(socket_path).await?;
            let req = Request::SessionRename { session_id, name: name.clone() };
            match send_request(&mut stream, 1, &req).await? {
                Response::SessionRenamed => println!("Session renamed to '{name}'."),
                Response::Error(e) => eprintln!("Error: {e}"),
                _ => eprintln!("Unexpected response"),
            }
        }
    }

    Ok(())
}
