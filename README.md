# haven-daemon

> Source code and issue tracker for the session daemon **and the `haven` CLI** that [Haven](https://haventerminal.com) runs on your remote machines.

Haven is a terminal app for ML engineers who work on remote GPU machines. When you connect to a host, Haven installs a small binary (~4MB) that manages persistent terminal sessions over a Unix socket. **This repo is that binary** — fully readable, fully auditable.

## What it does

- Keeps your shell sessions alive across SSH disconnects, laptop sleeps, and network interruptions
- Multiplexes terminal sessions through a single Unix socket
- Encrypts session transcripts at rest (ChaCha20-Poly1305)
- Tracks working directory changes without modifying your shell config
- Provides a `haven` CLI so you can browse and attach to your sessions from any terminal, not just the Haven app

## What it does NOT do

- Open network ports
- Send data anywhere
- Phone home or collect telemetry
- Require root privileges
- Modify your shell configuration or dotfiles

All communication happens over a local Unix socket at `~/.haven/daemon.sock`. The Haven app connects to it through your existing SSH tunnel.

## Filing issues

Found a bug or unexpected behavior with the daemon on your remote machine? [Open an issue](https://github.com/christiansafka/haven-daemon/issues). Include your OS, architecture, and daemon log (`~/.haven/daemon-*.log`) if possible.

## Building from source

```bash
cargo build --release -p haven-daemon
```

## Usage

The repo builds one binary, `haven-session-daemon`, that behaves as two tools depending on the name it's invoked with. The installer creates a `haven` symlink next to it so you can use either entry point:

```bash
# Daemon / low-level subcommands
haven-session-daemon daemon              # Start the daemon
haven-session-daemon session list        # List sessions
haven-session-daemon session create      # Create a session
haven-session-daemon session attach <id> # Attach to a session (raw mode, SIGWINCH, chord keys)
haven-session-daemon session kill <id>   # Kill a session

# haven CLI (terse, interactive — the one you'll actually use)
haven                    # no args: pick-or-create-or-attach, whichever makes sense
haven ls                 # list sessions
haven new [name]         # create a session and attach
haven attach <target>    # attach by name, 1-based index, or uuid prefix
haven kill <target>      # kill a session
haven rename <target> <name>
```

While attached, the chord-key prefix is `Ctrl-\` (or `Ctrl-B` for tmux muscle memory). Press `Ctrl-\ d` to detach, `Ctrl-\ s` to switch sessions, `Ctrl-\ n` for a new one, `Ctrl-\ ?` for help. Full docs: <https://haventerminal.com/docs/cli>.

## Uninstall

The CLI and the daemon are the same binary, and all session state lives in `~/.haven/`. **Removing either one removes everything**, including all running sessions and their encrypted transcripts:

```bash
rm -rf ~/.haven/
rm -f ~/.local/bin/haven  # if you symlinked it into your PATH
```

## Security

| | |
|-|-|
| **Auth** | Random 256-bit token per daemon instance (`~/.haven/daemon.token`, mode 0600) |
| **Encryption** | Session transcripts encrypted with ChaCha20-Poly1305, unique key per session |
| **Network** | None. Unix socket only -- no TCP, no outbound connections |
| **Privileges** | Runs as your user, no root required |

## License

MIT
