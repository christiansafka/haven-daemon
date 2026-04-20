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
        session_id: &str,
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
            cmd.env("HOME", &home);
        }
        if let Ok(user) = std::env::var("USER") {
            cmd.env("USER", &user);
            cmd.env("LOGNAME", &user);
        }
        // SHELL tells child processes which shell the user prefers
        // (used by `$SHELL -c ...` patterns, screen/tmux, etc.)
        if let Ok(sh) = std::env::var("SHELL") {
            cmd.env("SHELL", sh);
        } else {
            cmd.env("SHELL", shell);
        }
        // SSH_AUTH_SOCK is critical for SSH to reach the user's key agent.
        // Without it, `ssh` falls back to password auth or fails entirely.
        if let Ok(sock) = std::env::var("SSH_AUTH_SOCK") {
            cmd.env("SSH_AUTH_SOCK", sock);
        }
        // macOS identity / services vars — some tools (especially Go-based
        // ones like Tailscale) use these for user-identity resolution via
        // CoreFoundation / XPC when getpwuid() alone isn't sufficient.
        for key in &["TMPDIR", "__CF_USER_TEXT_ENCODING", "XPC_SERVICE_NAME", "XPC_FLAGS"] {
            if let Ok(val) = std::env::var(key) {
                cmd.env(key, val);
            }
        }
        // Tag the shell with its session id so tools running inside (like the
        // `haven` CLI) can detect they'd be attaching to their own session.
        cmd.env("HAVEN_SESSION_ID", session_id);
        // Build PATH: prepend common user-local bin dirs so tools like
        // `claude` installed via pipx/npm/etc. are available in spawned shells.
        // We also prepend this daemon's own directory so the `haven` multicall
        // symlink sitting next to it resolves first — critical when the prod
        // and dev variants are both installed (prod at `~/.haven/bin`, dev at
        // `~/.haven-dev/bin`): each variant's session shell must pick up its
        // *own* `haven` so the CLI talks to the matching daemon's socket/token.
        let daemon_bin = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|p| p.display().to_string()));
        {
            let home = std::env::var("HOME").unwrap_or_default();
            let mut path_parts: Vec<String> = Vec::new();
            if let Some(bin) = &daemon_bin {
                path_parts.push(bin.clone());
            }
            if !home.is_empty() {
                path_parts.push(format!("{}/.haven/bin", home));
                path_parts.push(format!("{}/.local/bin", home));
                path_parts.push(format!("{}/bin", home));
            }
            if let Ok(existing) = std::env::var("PATH") {
                path_parts.push(existing);
            }
            cmd.env("PATH", path_parts.join(":"));
        }

        // The PATH we set above is clobbered the moment a login shell sources
        // the user's rc files (many installers inject `export PATH=...:$PATH`
        // lines). To make sure *this* daemon's matching `haven` CLI wins —
        // even when prod and dev installers have both touched the user's rc —
        // we re-assert the daemon's bin dir at the head of PATH *after* rc
        // files run, via each shell's post-rc hook. Idempotent: no-op if
        // already first.
        let path_reassert_sh = daemon_bin.as_ref().map(|bin| {
            format!(
                "[ \"${{PATH%%:*}}\" = \"{bin}\" ] || export PATH=\"{bin}:$PATH\""
            )
        });

        // Set up a clean, minimal prompt for local sessions. Each shell
        // family needs a different mechanism because login shells source
        // rc files that override env-var-based prompts.
        if let Ok(home) = std::env::var("HOME") {
            let haven_tmp = std::env::temp_dir().join(format!("haven-shell-{}", std::process::id()));
            let _ = std::fs::create_dir_all(&haven_tmp);

            let path_line = path_reassert_sh.as_deref().unwrap_or("");

            // Haven shell integration (OSC 133) — emits semantic prompt
            // markers so the UI can track where prompts, commands, and
            // outputs begin/end plus each command's exit code:
            //   133;A       prompt start
            //   133;B       command start (end of prompt)
            //   133;C       command output start
            //   133;D;CODE  command finished, with exit code
            // xterm on the Haven side parses these; shells emit them via
            // their respective hook mechanisms.
            if shell.ends_with("zsh") && !env.contains_key("ZDOTDIR") {
                // zsh: ZDOTDIR trick — point zsh at a temp dir whose
                // .zshrc sources the user's real rc files and then
                // overrides PROMPT + re-asserts our bin dir on PATH (rc
                // files often prepend installer paths that'd otherwise
                // shadow our `haven` multicall symlink).
                //
                // CRITICAL: our wrapper .zshenv must NOT reset ZDOTDIR
                // to $HOME. zsh sources .zshenv → .zprofile → .zshrc,
                // looking up each at $ZDOTDIR. If we change ZDOTDIR
                // inside .zshenv, zsh finds the user's real .zshrc at
                // step 6 instead of our wrapper, and the PROMPT
                // override never runs.
                let zshenv_wrapper = format!(
                    "[[ -f \"{home}/.zshenv\" ]] && source \"{home}/.zshenv\"\n"
                );
                // 133;B lives inside %{ %} so zsh treats it as zero-width
                // and the prompt column math stays correct. precmd fires
                // before every prompt render; the previous command's exit
                // code is still in $? at that moment.
                let zshrc_wrapper = format!(
                    "[[ -f \"{home}/.zprofile\" ]] && source \"{home}/.zprofile\"\n\
                     [[ -f \"{home}/.zshrc\" ]] && source \"{home}/.zshrc\"\n\
                     PROMPT=$'%1~ %# %{{\\e]133;B\\a%}}'\n\
                     {path_line}\n\
                     autoload -Uz add-zsh-hook 2>/dev/null\n\
                     __haven_precmd() {{ local e=$?; printf '\\e]133;D;%d\\a\\e]133;A\\a' \"$e\"; }}\n\
                     __haven_preexec() {{ printf '\\e]133;C\\a'; }}\n\
                     add-zsh-hook precmd __haven_precmd 2>/dev/null\n\
                     add-zsh-hook preexec __haven_preexec 2>/dev/null\n"
                );
                let _ = std::fs::write(haven_tmp.join(".zshenv"), zshenv_wrapper);
                let _ = std::fs::write(haven_tmp.join(".zshrc"), zshrc_wrapper);
                cmd.env("ZDOTDIR", haven_tmp.to_string_lossy().as_ref());
            } else if shell.ends_with("bash") && !env.contains_key("PROMPT_COMMAND") {
                // bash: PROMPT_COMMAND runs after .bashrc/.bash_profile, so
                // it reliably overrides whatever PS1 they set. We also use it
                // to idempotently re-assert our bin dir at the head of PATH.
                //
                // For OSC 133:
                //  • ;D and ;A are emitted from PROMPT_COMMAND (exit code
                //    is the $? at the time the prompt is about to render).
                //  • ;B is baked into PS1 inside \[ \] so bash ignores it
                //    for width calc.
                //  • ;C is emitted from PS0, which bash prints after read
                //    but before exec — the natural "command started" point.
                //    Using PS0 avoids the DEBUG-trap double-fire problem.
                cmd.env(
                    "PROMPT_COMMAND",
                    format!(
                        "__haven_e=$?; {path_line}; PS1='\\W \\$ \\[\\e]133;B\\a\\]'; PS0='\\[\\e]133;C\\a\\]'; printf '\\e]133;D;%d\\a\\e]133;A\\a' \"$__haven_e\""
                    ),
                );
            } else if shell.ends_with("fish") && !env.contains_key("XDG_CONFIG_HOME") {
                // fish: XDG_CONFIG_HOME trick — wrapper config.fish sources
                // the real one, then overrides fish_prompt.
                //
                // OSC 133: fish_preexec / fish_postexec are dedicated
                // event handlers for before/after a command runs, which
                // maps cleanly to ;C and ;D. ;A/;B are emitted by wrapping
                // fish_prompt so the user's real prompt stays untouched
                // (we copy it to __haven_orig_prompt and call around it).
                let fish_dir = haven_tmp.join("fish");
                let _ = std::fs::create_dir_all(&fish_dir);
                let real_config = format!("{home}/.config/fish/config.fish");
                let fish_path_line = daemon_bin
                    .as_ref()
                    .map(|bin| {
                        format!(
                            "if test \"$PATH[1]\" != \"{bin}\"; set -gx PATH \"{bin}\" $PATH; end"
                        )
                    })
                    .unwrap_or_default();
                let wrapper = format!(
                    "set -gx XDG_CONFIG_HOME \"{home}/.config\"\n\
                     if test -f \"{real_config}\"\n\
                       source \"{real_config}\"\n\
                     end\n\
                     function fish_prompt\n\
                       echo (basename $PWD)' > '\n\
                     end\n\
                     {fish_path_line}\n\
                     function __haven_preexec --on-event fish_preexec\n\
                       printf '\\e]133;C\\a'\n\
                     end\n\
                     function __haven_postexec --on-event fish_postexec\n\
                       printf '\\e]133;D;%d\\a' $status\n\
                     end\n\
                     functions -c fish_prompt __haven_orig_prompt 2>/dev/null\n\
                     function fish_prompt\n\
                       printf '\\e]133;A\\a'\n\
                       __haven_orig_prompt\n\
                       printf '\\e]133;B\\a'\n\
                     end\n"
                );
                let _ = std::fs::write(fish_dir.join("config.fish"), wrapper);
                cmd.env("XDG_CONFIG_HOME", haven_tmp.to_string_lossy().as_ref());
            }
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
