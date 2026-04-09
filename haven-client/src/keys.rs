//! Chord-key prefix state machine.
//!
//! Recognizes two single-byte prefixes (`Ctrl-\` = 0x1c and `Ctrl-B` = 0x02)
//! followed by a one-byte chord. Anything else passes through to the PTY
//! verbatim. We deliberately keep this byte-stream-based (not key-event-based)
//! because the attach loop reads raw bytes from stdin in raw mode, and a real
//! keyboard parser would be considerable extra dependency surface for very
//! little gain.
//!
//! Why no `Cmd-/` byte sequence: macOS terminal apps intercept Cmd-chords by
//! default, so the only way Cmd-/ ever reaches the running process is if the
//! user has configured their terminal emulator to send a custom sequence on
//! that key. We document mapping it to "send hex 0x1c" in the CLI docs page,
//! which gives Cmd-/ the same behavior as Ctrl-\ for free, with no special
//! parsing here.

const CTRL_BACKSLASH: u8 = 0x1c;
const CTRL_B: u8 = 0x02;

/// Result of feeding one byte through the chord parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChordEvent {
    /// Forward this many bytes from the input slice (starting at `from`) to
    /// the PTY unchanged.
    Passthrough { from: usize, to: usize },
    /// A complete chord was recognized.
    Action(ChordAction),
    /// Bytes were consumed (held in internal buffer) but produced no output
    /// yet — used while waiting for the second key after a prefix.
    Pending,
}

/// What the chord parser asks the attach loop to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordAction {
    /// Detach cleanly: leave the session running, return to the local shell.
    Detach,
    /// Open the session picker and switch to a different session.
    Switch,
    /// Create a new session and attach to it.
    New,
    /// Show the chord-key help overlay.
    Help,
    /// Forward a literal copy of the prefix byte to the PTY (escape hatch
    /// for users who actually need to type Ctrl-\ or Ctrl-B inside a session).
    LiteralPrefix(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    AwaitChord(u8),
}

pub struct ChordParser {
    state: State,
}

impl Default for ChordParser {
    fn default() -> Self {
        Self { state: State::Idle }
    }
}

impl ChordParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process a chunk of bytes from stdin and return a sequence of events.
    /// The caller forwards `Passthrough` ranges to the PTY and reacts to
    /// `Action` events directly. `Pending` events are informational only.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<ChordEvent> {
        let mut events = Vec::new();
        let mut run_start: Option<usize> = None;

        let flush_run = |events: &mut Vec<ChordEvent>, run_start: &mut Option<usize>, end: usize| {
            if let Some(start) = run_start.take() {
                if end > start {
                    events.push(ChordEvent::Passthrough { from: start, to: end });
                }
            }
        };

        for (i, &b) in chunk.iter().enumerate() {
            match self.state {
                State::Idle => {
                    if b == CTRL_BACKSLASH || b == CTRL_B {
                        // Flush any forwardable bytes accumulated so far,
                        // then enter await-chord state without forwarding
                        // the prefix byte itself.
                        flush_run(&mut events, &mut run_start, i);
                        self.state = State::AwaitChord(b);
                        events.push(ChordEvent::Pending);
                    } else if run_start.is_none() {
                        run_start = Some(i);
                    }
                }
                State::AwaitChord(prefix) => {
                    let action = match b {
                        b'd' | b'D' => Some(ChordAction::Detach),
                        b's' | b'S' => Some(ChordAction::Switch),
                        b'n' | b'N' => Some(ChordAction::New),
                        b'?' | b'h' | b'H' => Some(ChordAction::Help),
                        // Prefix-prefix: send a literal copy through.
                        b if b == prefix => Some(ChordAction::LiteralPrefix(prefix)),
                        _ => None,
                    };
                    self.state = State::Idle;
                    if let Some(act) = action {
                        events.push(ChordEvent::Action(act));
                    }
                    // If `action` was None we silently drop the prefix and
                    // the unrecognized chord key — same as tmux's "no such
                    // binding" behavior, minus the bell.
                }
            }
        }

        // Flush any remaining run after the loop.
        flush_run(&mut events, &mut run_start, chunk.len());
        events
    }

    /// True if we're holding a prefix byte waiting for a chord key. The
    /// attach loop uses this to decide whether to apply a short timeout that
    /// drops a stale prefix when the user presses the prefix and walks away.
    pub fn is_pending(&self) -> bool {
        matches!(self.state, State::AwaitChord(_))
    }

    /// Drop any pending prefix without firing a chord. Called when the
    /// pending-prefix timeout fires.
    pub fn cancel_pending(&mut self) {
        self.state = State::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forward(chunk: &[u8], events: &[ChordEvent]) -> Vec<u8> {
        let mut out = Vec::new();
        for ev in events {
            if let ChordEvent::Passthrough { from, to } = ev {
                out.extend_from_slice(&chunk[*from..*to]);
            }
        }
        out
    }

    #[test]
    fn passes_through_normal_bytes() {
        let mut p = ChordParser::new();
        let chunk = b"hello world";
        let evs = p.feed(chunk);
        assert_eq!(forward(chunk, &evs), b"hello world");
    }

    #[test]
    fn ctrl_backslash_d_detaches() {
        let mut p = ChordParser::new();
        let chunk = &[b'a', 0x1c, b'd', b'b'];
        let evs = p.feed(chunk);
        // 'a' should pass through, then Detach action, then 'b' should pass through.
        assert_eq!(forward(chunk, &evs), b"ab");
        assert!(evs
            .iter()
            .any(|e| matches!(e, ChordEvent::Action(ChordAction::Detach))));
    }

    #[test]
    fn ctrl_b_d_detaches() {
        let mut p = ChordParser::new();
        let chunk = &[0x02, b'd'];
        let evs = p.feed(chunk);
        assert!(forward(chunk, &evs).is_empty());
        assert!(evs
            .iter()
            .any(|e| matches!(e, ChordEvent::Action(ChordAction::Detach))));
    }

    #[test]
    fn double_prefix_sends_literal() {
        let mut p = ChordParser::new();
        let chunk = &[0x1c, 0x1c];
        let evs = p.feed(chunk);
        assert!(forward(chunk, &evs).is_empty());
        assert!(matches!(
            evs.last(),
            Some(ChordEvent::Action(ChordAction::LiteralPrefix(0x1c)))
        ));
    }

    #[test]
    fn unrecognized_chord_drops_prefix() {
        let mut p = ChordParser::new();
        let chunk = &[0x1c, b'x', b'y'];
        let evs = p.feed(chunk);
        // 'x' is unbound; both 0x1c and 'x' are dropped, only 'y' passes through.
        assert_eq!(forward(chunk, &evs), b"y");
    }

    #[test]
    fn prefix_state_persists_across_chunks() {
        let mut p = ChordParser::new();
        p.feed(&[0x1c]);
        assert!(p.is_pending());
        let evs = p.feed(&[b'd']);
        assert!(!p.is_pending());
        assert!(evs
            .iter()
            .any(|e| matches!(e, ChordEvent::Action(ChordAction::Detach))));
    }

    #[test]
    fn cancel_pending_clears_state() {
        let mut p = ChordParser::new();
        p.feed(&[0x02]);
        assert!(p.is_pending());
        p.cancel_pending();
        assert!(!p.is_pending());
    }

    #[test]
    fn switch_new_help_actions() {
        for (key, expected) in &[
            (b's', ChordAction::Switch),
            (b'n', ChordAction::New),
            (b'?', ChordAction::Help),
        ] {
            let mut p = ChordParser::new();
            let chunk = &[0x1c, *key];
            let evs = p.feed(chunk);
            assert!(evs.iter().any(|e| matches!(e, ChordEvent::Action(a) if a == expected)));
        }
    }
}
