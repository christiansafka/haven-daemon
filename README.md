# haven-daemon

> Source code and issue tracker for the session daemon that [Haven](https://haventerminal.com) runs on your remote machines.

Haven is a terminal app for ML engineers who work on remote GPU machines. When you connect to a host, Haven installs a small daemon (~4MB) that manages persistent terminal sessions over a Unix socket. **This repo is that daemon** -- fully readable, fully auditable.

## What it does

- Keeps your shell sessions alive across SSH disconnects, laptop sleeps, and network interruptions
- Multiplexes terminal sessions through a single Unix socket
- Encrypts session transcripts at rest (ChaCha20-Poly1305)
- Tracks working directory changes without modifying your shell config

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

## Usage (CLI)

```bash
haven-session-daemon daemon              # Start the daemon
haven-session-daemon session list        # List sessions
haven-session-daemon session create      # Create a session
haven-session-daemon session attach <id> # Attach to a session
haven-session-daemon session kill <id>   # Kill a session
```

## Uninstall

```bash
rm -rf ~/.haven/
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
