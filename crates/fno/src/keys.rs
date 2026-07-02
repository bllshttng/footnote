//! The client-side leader-key layer: a pure, stateful scanner over raw stdin
//! bytes producing forward-chunks and mux events.
//!
//! Interpretation is CLIENT-side by design (Locked Decision 5): the server
//! only ever sees `Command`s, never key chords. The scanner is a pure state
//! machine so it is exhaustively unit-testable, including escape sequences
//! split across reads (raw-mode stdin arrives in arbitrary chunks).
//!
//! Table (leader = Ctrl-b, tmux-compatible where a binding exists):
//! `%`/`"` split H/V · `h j k l` + arrows focus · `H J K L` + Ctrl-arrows
//! resize · `x` close pane · `c` new tab · `n`/`p` cycle tabs · `1`-`9`
//! select tab · `&` close tab · `w` panel selector · `b` toggle sideline ·
//! `d` detach · leader-leader = one literal leader byte. Leader + anything
//! unmapped is swallowed with BEL - a chord typo must never leak half a
//! chord into the pane (AC2-UI's never-leak guarantee).
//!
//! Ctrl-\ (0x1C) detaches from ANY state, preserving the Phase 1 key.
//! ponytail: like Phase 1, a 0x1C inside a paste still detaches; bracketed-
//! paste awareness in the scanner is the Phase-3 upgrade if it bites.

use crate::proto::Command;
use crate::tree::Dir;

/// The leader byte: Ctrl-b (0x02).
pub const LEADER: u8 = 0x02;
/// Ctrl-\ : detach, from any scanner state (the Phase 1 detach key).
pub const DETACH: u8 = 0x1C;

/// One scanned outcome. `Forward` chunks are byte-exact pass-through - bare
/// bytes are NEVER re-encoded (AC2-UI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Forward(Vec<u8>),
    Cmd(Command),
    /// Leader+digit: select the Nth tab of the viewed squad. The scanner
    /// only knows the index; the client resolves it to a stable `TabId`
    /// against its last `Layout` (v3: `SelectTab` names ids, not indices).
    SelectTabIdx(usize),
    Detach,
    /// Open the sideline selector (leader+w). Selector-mode keys are
    /// interpreted by the client's view layer, not here.
    OpenSelector,
    /// Show/hide the sideline (leader+b).
    TogglePanel,
    /// Swallowed unmapped chord: the client sounds BEL.
    Bell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Normal,
    /// Saw the leader; the next key (or escape sequence) is a chord.
    Leader,
    /// Accumulating an escape sequence after the leader (arrows /
    /// Ctrl-arrows), possibly split across reads.
    LeaderEsc(Vec<u8>),
}

/// The scanner. One per client connection; state survives across reads so a
/// chord split at a read boundary still lands.
#[derive(Debug)]
pub struct Scanner {
    state: State,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner {
            state: State::Normal,
        }
    }
}

impl Scanner {
    /// Scan one stdin chunk into events. Bytes between specials coalesce
    /// into as few `Forward` chunks as possible.
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        let mut plain: Vec<u8> = Vec::new();
        for &b in bytes {
            match std::mem::replace(&mut self.state, State::Normal) {
                State::Normal => {
                    if b == DETACH {
                        flush(&mut plain, &mut out);
                        out.push(Event::Detach);
                    } else if b == LEADER {
                        flush(&mut plain, &mut out);
                        self.state = State::Leader;
                    } else {
                        plain.push(b);
                    }
                }
                State::Leader => {
                    if b == DETACH {
                        // Detach wins from any state; the pending chord dies.
                        out.push(Event::Detach);
                    } else if b == 0x1b {
                        self.state = State::LeaderEsc(vec![0x1b]);
                    } else {
                        out.push(chord(b));
                    }
                }
                State::LeaderEsc(mut seq) => {
                    if b == DETACH {
                        out.push(Event::Detach);
                        continue;
                    }
                    seq.push(b);
                    match esc_chord(&seq) {
                        EscScan::Complete(ev) => out.push(ev),
                        EscScan::Partial => self.state = State::LeaderEsc(seq),
                        EscScan::Invalid => out.push(Event::Bell),
                    }
                }
            }
        }
        flush(&mut plain, &mut out);
        out
    }
}

fn flush(plain: &mut Vec<u8>, out: &mut Vec<Event>) {
    if !plain.is_empty() {
        out.push(Event::Forward(std::mem::take(plain)));
    }
}

/// The single-byte chord table.
fn chord(b: u8) -> Event {
    match b {
        LEADER => Event::Forward(vec![LEADER]), // leader-leader = literal
        b'%' => Event::Cmd(Command::SplitH),
        b'"' => Event::Cmd(Command::SplitV),
        b'h' => Event::Cmd(Command::FocusDir(Dir::Left)),
        b'j' => Event::Cmd(Command::FocusDir(Dir::Down)),
        b'k' => Event::Cmd(Command::FocusDir(Dir::Up)),
        b'l' => Event::Cmd(Command::FocusDir(Dir::Right)),
        b'H' => Event::Cmd(Command::ResizeDir(Dir::Left)),
        b'J' => Event::Cmd(Command::ResizeDir(Dir::Down)),
        b'K' => Event::Cmd(Command::ResizeDir(Dir::Up)),
        b'L' => Event::Cmd(Command::ResizeDir(Dir::Right)),
        b'x' => Event::Cmd(Command::ClosePane),
        b'c' => Event::Cmd(Command::NewTab),
        b'n' => Event::Cmd(Command::NextTab),
        b'p' => Event::Cmd(Command::PrevTab),
        b'&' => Event::Cmd(Command::CloseTab),
        b'1'..=b'9' => Event::SelectTabIdx((b - b'1') as usize),
        b'w' => Event::OpenSelector,
        b'b' => Event::TogglePanel,
        b'd' => Event::Detach,
        _ => Event::Bell,
    }
}

enum EscScan {
    Complete(Event),
    Partial,
    Invalid,
}

/// Arrows (`ESC [ A..D` -> focus) and Ctrl-arrows (`ESC [ 1 ; 5 A..D` ->
/// resize) after the leader. Anything that stops matching either prefix is
/// swallowed as one Bell.
fn esc_chord(seq: &[u8]) -> EscScan {
    const PLAIN: &[u8] = b"\x1b[";
    const CTRL: &[u8] = b"\x1b[1;5";
    let arrow = |b: u8| -> Option<Dir> {
        match b {
            b'A' => Some(Dir::Up),
            b'B' => Some(Dir::Down),
            b'C' => Some(Dir::Right),
            b'D' => Some(Dir::Left),
            _ => None,
        }
    };
    // Complete plain arrow: ESC [ X
    if seq.len() == 3 && seq.starts_with(PLAIN) {
        if let Some(dir) = arrow(seq[2]) {
            return EscScan::Complete(Event::Cmd(Command::FocusDir(dir)));
        }
        // Might still be the Ctrl-arrow prefix (ESC [ 1 ...).
        if seq[2] != b'1' {
            return EscScan::Invalid;
        }
    }
    // Complete Ctrl-arrow: ESC [ 1 ; 5 X
    if seq.len() == 6 && seq.starts_with(CTRL) {
        return match arrow(seq[5]) {
            Some(dir) => EscScan::Complete(Event::Cmd(Command::ResizeDir(dir))),
            None => EscScan::Invalid,
        };
    }
    if seq.len() < 6 && (CTRL.starts_with(seq) || seq.starts_with(PLAIN) && CTRL.starts_with(seq)) {
        return EscScan::Partial;
    }
    if seq.len() < 3 && PLAIN.starts_with(seq) {
        return EscScan::Partial;
    }
    EscScan::Invalid
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(chunks: &[&[u8]]) -> Vec<Event> {
        let mut s = Scanner::default();
        chunks.iter().flat_map(|c| s.scan(c)).collect()
    }

    #[test]
    fn client_keys_bare_bytes_pass_through_byte_exact() {
        // AC2-UI carried: no re-encoding, one coalesced chunk.
        let events = scan_all(&[b"echo hi \xf0\x9f\x8e\x89\r"]);
        assert_eq!(
            events,
            vec![Event::Forward(b"echo hi \xf0\x9f\x8e\x89\r".to_vec())]
        );
    }

    #[test]
    fn client_keys_leader_chords_map_and_never_leak() {
        let events = scan_all(&[b"a\x02%b"]);
        // 'a' forwards, leader+% commands, 'b' forwards - the chord bytes
        // themselves never reach the pane.
        assert_eq!(
            events,
            vec![
                Event::Forward(b"a".to_vec()),
                Event::Cmd(Command::SplitH),
                Event::Forward(b"b".to_vec()),
            ]
        );
        assert_eq!(scan_all(&[b"\x02\""]), vec![Event::Cmd(Command::SplitV)]);
        assert_eq!(
            scan_all(&[b"\x02l"]),
            vec![Event::Cmd(Command::FocusDir(Dir::Right))]
        );
        assert_eq!(
            scan_all(&[b"\x02K"]),
            vec![Event::Cmd(Command::ResizeDir(Dir::Up))]
        );
        assert_eq!(scan_all(&[b"\x02x"]), vec![Event::Cmd(Command::ClosePane)]);
        assert_eq!(scan_all(&[b"\x027"]), vec![Event::SelectTabIdx(6)]);
        assert_eq!(scan_all(&[b"\x02&"]), vec![Event::Cmd(Command::CloseTab)]);
        assert_eq!(scan_all(&[b"\x02w"]), vec![Event::OpenSelector]);
        assert_eq!(scan_all(&[b"\x02b"]), vec![Event::TogglePanel]);
        assert_eq!(scan_all(&[b"\x02d"]), vec![Event::Detach]);
    }

    #[test]
    fn client_keys_leader_leader_sends_one_literal_leader() {
        assert_eq!(scan_all(&[b"\x02\x02"]), vec![Event::Forward(vec![LEADER])]);
    }

    #[test]
    fn client_keys_leader_unmapped_swallows_with_bell() {
        // The 'q' must NOT be forwarded - swallow + BEL.
        assert_eq!(scan_all(&[b"\x02q"]), vec![Event::Bell]);
    }

    #[test]
    fn client_keys_arrows_and_ctrl_arrows_split_across_reads() {
        // A leader+arrow chord arriving one byte per read still lands.
        assert_eq!(
            scan_all(&[b"\x02", b"\x1b", b"[", b"C"]),
            vec![Event::Cmd(Command::FocusDir(Dir::Right))]
        );
        // Ctrl-Up = resize up, split at an awkward boundary.
        assert_eq!(
            scan_all(&[b"\x02\x1b[1;", b"5A"]),
            vec![Event::Cmd(Command::ResizeDir(Dir::Up))]
        );
        // A non-arrow escape after leader is swallowed as one Bell.
        assert_eq!(scan_all(&[b"\x02\x1b[Z"]), vec![Event::Bell]);
    }

    #[test]
    fn client_keys_detach_byte_works_from_any_state() {
        assert_eq!(
            scan_all(&[b"abc\x1c"]),
            vec![Event::Forward(b"abc".to_vec()), Event::Detach]
        );
        // Even mid-chord.
        assert_eq!(scan_all(&[b"\x02\x1c"]), vec![Event::Detach]);
    }
}
