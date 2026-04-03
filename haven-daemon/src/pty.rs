use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use tokio::sync::broadcast;

/// Handle to a PTY process. Owns the master side of the PTY pair.
/// All fields are Send + Sync safe via interior mutability.
pub struct PtyHandle {
    /// Master PTY, wrapped for thread safety (needed for resize).
    master: StdMutex<Box<dyn MasterPty + Send>>,
    /// Writer to the PTY stdin.
    master_writer: StdMutex<Box<dyn Write + Send>>,
    /// Child process handle.
    child: StdMutex<Box<dyn portable_pty::Child + Send + Sync>>,
    /// Background PTY reader thread.
    reader_handle: StdMutex<Option<tokio::task::JoinHandle<()>>>,
    /// Broadcast channel for PTY output.
    output_tx: broadcast::Sender<Vec<u8>>,
    /// Process ID.
    pid: Option<u32>,
}

// Safety: All inner fields are protected by Mutex
unsafe impl Sync for PtyHandle {}

impl PtyHandle {
    /// Spawn a new PTY with the given shell and configuration.
    pub fn spawn(
        shell: &str,
        cwd: Option<&PathBuf>,
        env: &HashMap<String, String>,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        let pty_system = native_pty_system();

        let pty_pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to open PTY pair")?;

        // Launch as a login shell so .bash_profile/.profile are sourced
        // (ensures user PATH additions like ~/.local/bin are available).
        let mut cmd = CommandBuilder::new(shell);
        cmd.arg("-l");
        if let Some(dir) = cwd {
            cmd.cwd(dir);
        }

        // Inject essential terminal environment variables.
        // Without TERM, the shell doesn't know terminal capabilities
        // (backspace, arrow keys, colors all break).
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        if std::env::var("LANG").is_ok() {
            cmd.env("LANG", std::env::var("LANG").unwrap());
        } else {
            cmd.env("LANG", "en_US.UTF-8");
        }
        if let Ok(home) = std::env::var("HOME") {
            cmd.env("HOME", home);
        }
        if let Ok(user) = std::env::var("USER") {
            cmd.env("USER", user);
        }
        // Build PATH: prepend common user-local bin dirs so tools like
        // `claude` installed via pipx/npm/etc. are available in spawned shells.
        {
            let home = std::env::var("HOME").unwrap_or_default();
            let mut path_parts: Vec<String> = Vec::new();
            if !home.is_empty() {
                path_parts.push(format!("{}/.local/bin", home));
                path_parts.push(format!("{}/bin", home));
            }
            if let Ok(existing) = std::env::var("PATH") {
                path_parts.push(existing);
            }
            cmd.env("PATH", path_parts.join(":"));
        }

        // User-provided env vars override the defaults above
        for (key, value) in env {
            cmd.env(key, value);
        }

        let child = pty_pair
            .slave
            .spawn_command(cmd)
            .context("Failed to spawn shell process")?;

        let pid = child.process_id();

        let mut reader = pty_pair
            .master
            .try_clone_reader()
            .context("Failed to clone PTY reader")?;

        let writer = pty_pair
            .master
            .take_writer()
            .context("Failed to take PTY writer")?;

        // Broadcast channel for output
        let (output_tx, _) = broadcast::channel::<Vec<u8>>(512);
        let tx = output_tx.clone();

        // Spawn a blocking thread to read PTY output
        let reader_handle = tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        let _ = tx.send(data);
                    }
                    Err(e) => {
                        tracing::debug!("PTY read error (normal on exit): {e}");
                        break;
                    }
                }
            }
        });

        // Spawn CWD polling thread — emits OSC 7 escape sequences into the
        // output stream whenever the shell's working directory changes.
        // This is shell-agnostic: no stdin injection, no echo, no filtering.
        if let Some(shell_pid) = pid {
            let cwd_tx = output_tx.clone();
            std::thread::Builder::new()
                .name("haven-cwd-poll".into())
                .spawn(move || {
                    let mut last_cwd = String::new();
                    let hostname = gethostname();
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        match get_process_cwd(shell_pid) {
                            Some(cwd) if cwd != last_cwd => {
                                last_cwd = cwd.clone();
                                let osc7 = format!("\x1b]7;file://{hostname}{cwd}\x07");
                                if cwd_tx.send(osc7.into_bytes()).is_err() {
                                    break; // No subscribers, session ended
                                }
                            }
                            None => break, // Process gone
                            _ => {}
                        }
                    }
                })
                .ok();
        }

        Ok(PtyHandle {
            master: StdMutex::new(pty_pair.master),
            master_writer: StdMutex::new(writer),
            child: StdMutex::new(child),
            reader_handle: StdMutex::new(Some(reader_handle)),
            output_tx,
            pid,
        })
    }

    /// Write data to the PTY (user input).
    pub fn write(&self, data: &[u8]) -> Result<()> {
        let mut writer = self.master_writer.lock().unwrap();
        writer.write_all(data).context("Failed to write to PTY")?;
        writer.flush().context("Failed to flush PTY")?;
        Ok(())
    }

    /// Resize the PTY.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let master = self.master.lock().unwrap();
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Failed to resize PTY")?;
        Ok(())
    }

    /// Subscribe to PTY output.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.output_tx.subscribe()
    }

    /// Get the process ID.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Check if the child process has exited.
    pub fn try_wait(&self) -> Result<Option<u32>> {
        let mut child = self.child.lock().unwrap();
        match child.try_wait() {
            Ok(Some(status)) => Ok(Some(status.exit_code())),
            Ok(None) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("Failed to wait on child: {e}")),
        }
    }

    /// Kill the child process.
    pub fn kill(&self) -> Result<()> {
        let mut child = self.child.lock().unwrap();
        child.kill().context("Failed to kill PTY child")?;
        Ok(())
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
        if let Ok(mut handle) = self.reader_handle.lock() {
            if let Some(h) = handle.take() {
                h.abort();
            }
        }
    }
}

// ── Process CWD tracking (shell-agnostic OSC 7) ──

/// Get the current working directory of a process by PID.
#[cfg(target_os = "linux")]
fn get_process_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
}

/// Get the current working directory of a process by PID (macOS).
#[cfg(target_os = "macos")]
fn get_process_cwd(pid: u32) -> Option<String> {
    // Use proc_pidinfo with PROC_PIDVNODEPATHINFO to get cwd
    #[repr(C)]
    struct VnodeInfoPath {
        _padding: [u8; 152], // vip_vi (vnode_info)
        vip_path: [u8; 1024], // MAXPATHLEN
    }

    #[repr(C)]
    struct ProcVnodePathInfo {
        pvi_cdir: VnodeInfoPath,
        pvi_rdir: VnodeInfoPath,
    }

    let mut info = std::mem::MaybeUninit::<ProcVnodePathInfo>::zeroed();
    let size = std::mem::size_of::<ProcVnodePathInfo>() as libc::c_int;

    let ret = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr() as *mut libc::c_void,
            size,
        )
    };

    if ret <= 0 {
        return None;
    }

    let info = unsafe { info.assume_init() };
    let path_bytes = &info.pvi_cdir.vip_path;
    let nul_pos = path_bytes.iter().position(|&b| b == 0).unwrap_or(path_bytes.len());
    std::str::from_utf8(&path_bytes[..nul_pos])
        .ok()
        .map(|s| s.to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn get_process_cwd(_pid: u32) -> Option<String> {
    None
}

/// Get the system hostname.
fn gethostname() -> String {
    let mut buf = [0u8; 256];
    let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if ret != 0 {
        return String::from("localhost");
    }
    let nul_pos = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..nul_pos])
        .unwrap_or("localhost")
        .to_string()
}
