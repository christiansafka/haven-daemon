mod cli;
mod daemon;
mod history;
mod protocol;
mod pty;
mod session;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands, SessionAction};
use haven_protocol::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

fn main() {
    let cli = Cli::parse();

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

/// Send a request frame and read the response, capturing history Event data.
/// Returns (Response, Vec<u8>) where the Vec contains any Output event data
/// that arrived before the Response (i.e., history replay).
async fn send_request_with_history(stream: &mut UnixStream, correlation_id: u32, req: &Request) -> Result<(Response, Vec<u8>)> {
    let frame = Frame::request(correlation_id, req)?;
    let encoded = frame.encode();
    stream.write_all(&encoded).await?;
    stream.flush().await?;

    let mut history = Vec::new();
    loop {
        let mut len_buf = [0u8; 4];
        AsyncReadExt::read_exact(stream, &mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        AsyncReadExt::read_exact(stream, &mut body).await?;
        let resp_frame = Frame::decode(&body)?;

        if resp_frame.frame_type == FrameType::Response {
            let resp: Response = rmp_serde::from_slice(&resp_frame.payload)?;
            return Ok((resp, history));
        }
        // Capture Output events (history replay)
        if let Ok(event) = rmp_serde::from_slice::<Event>(&resp_frame.payload) {
            if let Event::Output { data, .. } = event {
                history.extend_from_slice(&data);
            }
        }
    }
}

/// Send a request frame and read the response.
/// Skips any Event frames that arrive before the Response (e.g., history replay).
async fn send_request(stream: &mut UnixStream, correlation_id: u32, req: &Request) -> Result<Response> {
    let frame = Frame::request(correlation_id, req)?;
    let encoded = frame.encode();
    stream.write_all(&encoded).await?;
    stream.flush().await?;

    // Read frames until we get a Response (skip Events that may arrive first)
    loop {
        let mut len_buf = [0u8; 4];
        AsyncReadExt::read_exact(stream, &mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        AsyncReadExt::read_exact(stream, &mut body).await?;
        let resp_frame = Frame::decode(&body)?;

        if resp_frame.frame_type == FrameType::Response {
            let resp: Response = rmp_serde::from_slice(&resp_frame.payload)?;
            return Ok(resp);
        }
        // Skip non-Response frames (Events sent before the Response)
    }
}

async fn connect_daemon(socket_path: &std::path::Path) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .map_err(|e| anyhow::anyhow!(
            "Failed to connect to daemon at {}: {e}\nIs the daemon running? Start with: haven-session-daemon daemon",
            socket_path.display()
        ))?;

    // Read auth token and authenticate
    let token_path = socket_path.with_extension("token");
    let token = std::fs::read_to_string(&token_path)
        .map_err(|e| anyhow::anyhow!(
            "Failed to read auth token at {}: {e}",
            token_path.display()
        ))?;

    let auth_req = Request::Auth { token };
    match send_request(&mut stream, 0, &auth_req).await? {
        Response::AuthOk => {}
        Response::Error(e) => return Err(anyhow::anyhow!("Authentication failed: {e}")),
        _ => return Err(anyhow::anyhow!("Unexpected auth response")),
    }

    Ok(stream)
}

async fn handle_cli_action(
    action: SessionAction,
    socket_path: &std::path::Path,
) -> Result<()> {
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
            let mut stream = connect_daemon(socket_path).await?;

            // Send attach request — use send_request_with_history to capture
            // history Event frames that arrive before the Response.
            let req = Request::SessionAttach { session_id, history_bytes };
            let history_data = match send_request_with_history(&mut stream, 1, &req).await? {
                (Response::SessionAttached { .. }, history) => history,
                (Response::Error(e), _) => return Err(anyhow::anyhow!("Attach failed: {e}")),
                _ => return Err(anyhow::anyhow!("Unexpected response")),
            };

            // Write history to stdout before entering live stream
            if !history_data.is_empty() {
                let mut stdout = tokio::io::stdout();
                let _ = stdout.write_all(&history_data).await;
                let _ = stdout.flush().await;
            }

            // Now we're attached. The daemon will send Event frames on this socket.
            // We forward: PTY output → stdout, stdin → SessionWrite requests.
            let (mut sock_reader, mut sock_writer) = stream.into_split();

            let mut stdout = tokio::io::stdout();
            let mut stdin = tokio::io::stdin();

            // Task: read Event frames from daemon → write raw output to stdout
            let reader_task = tokio::spawn(async move {
                loop {
                    // Read frame length
                    let mut len_buf = [0u8; 4];
                    if AsyncReadExt::read_exact(&mut sock_reader, &mut len_buf).await.is_err() {
                        break 0i32;
                    }
                    let len = u32::from_be_bytes(len_buf) as usize;
                    let mut body = vec![0u8; len];
                    if AsyncReadExt::read_exact(&mut sock_reader, &mut body).await.is_err() {
                        break 0;
                    }
                    let frame = match Frame::decode(&body) {
                        Ok(f) => f,
                        Err(_) => continue,
                    };
                    let event: Event = match rmp_serde::from_slice(&frame.payload) {
                        Ok(e) => e,
                        Err(_) => continue,
                    };

                    match event {
                        Event::Output { data, .. } => {
                            if stdout.write_all(&data).await.is_err() {
                                break 0;
                            }
                            let _ = stdout.flush().await;
                        }
                        Event::SessionExited { exit_code, .. } => {
                            break exit_code;
                        }
                        _ => {}
                    }
                }
            });

            // Task: read stdin → send SessionWrite frames to daemon
            let writer_task = tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                loop {
                    let n = match AsyncReadExt::read(&mut stdin, &mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let req = Request::SessionWrite {
                        session_id,
                        data: buf[..n].to_vec(),
                    };
                    let frame = match Frame::request(0, &req) {
                        Ok(f) => f,
                        Err(_) => break,
                    };
                    let encoded = frame.encode();
                    if sock_writer.write_all(&encoded).await.is_err() {
                        break;
                    }
                    let _ = sock_writer.flush().await;
                }
            });

            // Wait for session exit
            let exit_code = reader_task.await.unwrap_or(1);
            writer_task.abort();
            std::process::exit(exit_code);
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
