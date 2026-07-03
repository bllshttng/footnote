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
//! `d` detach · `[`/`]` jump prev/next command block · `v` select block ·
//! `y` copy selection · `r` rerun block (x-38c4) · leader-leader = one literal
//! leader byte. Leader + anything unmapped is swallowed with BEL - a chord typo
//! must never leak half a chord into the pane (AC2-UI's never-leak guarantee).
//!
//! Detach is leader+d ONLY (Phase 3 Locked 11): the Phase 1/2 raw-0x1C
//! match is gone, so Ctrl-\ forwards to the pane and SIGQUIT works again.
//!
//! Bracketed-paste passthrough (US5): `ESC[200~` puts the scanner in a
//! verbatim state where every byte - leader bytes, Ctrl-\, everything -
//! forwards untouched until `ESC[201~`; both markers forward too. Marker
//! matching is a rolling index that survives read boundaries (AC5-ERR), and
//! bytes are never held back: a marker prefix that fizzles was already
//! forwarded as the ordinary bytes it turned out to be. Residual (accepted,
//! documented): an unterminated paste (no `201~` ever) leaves chords
//! disabled until the close marker or reconnect - input keeps forwarding
//! verbatim and EOF/terminal-close still detaches, so the state machine can
//! disable chords at worst, never brick input (AC5-FR). Unbracketed paste
//! can still trigger leader chords - the tmux-class residual (Locked 11).

use crate::proto::{BlockDir, Command};
use crate::tree::Dir;

/// The leader byte: Ctrl-b (0x02).
pub const LEADER: u8 = 0x02;

/// Bracketed-paste markers, as the terminal emits them.
const PASTE_OPEN: &[u8] = b"\x1b[200~";
const PASTE_CLOSE: &[u8] = b"\x1b[201~";

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
    /// Jump the focused pane's shared scroll to the prev/next command block
    /// (leader+`[` / leader+`]`, x-38c4). The client resolves the focused pane.
    BlockJump(BlockDir),
    /// Move the focused pane's block selection (leader+v walks older, x-38c4).
    BlockSelect(BlockDir),
    /// Rerun the focused pane's selected block command (leader+r, x-38c4).
    BlockRerun,
    /// Swallowed unmapped chord: the client sounds BEL.
    Bell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Bytes forward; `usize` is the rolling PASTE_OPEN match index (how
    /// many marker bytes the forwarded tail already matches).
    Normal(usize),
    /// Saw the leader; the next key (or escape sequence) is a chord.
    Leader,
    /// Accumulating an escape sequence after the leader (arrows /
    /// Ctrl-arrows / a paste-open marker), possibly split across reads.
    LeaderEsc(Vec<u8>),
    /// Inside a bracketed paste: everything forwards verbatim; `usize` is
    /// the rolling PASTE_CLOSE match index.
    Paste(usize),
}

/// The scanner. One per client connection; state survives across reads so a
/// chord or marker split at a read boundary still lands.
#[derive(Debug)]
pub struct Scanner {
    state: State,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner {
            state: State::Normal(0),
        }
    }
}

/// Advance a rolling marker match: how many bytes of `marker` the stream
/// tail matches after consuming `b`. The only self-overlap in either marker
/// is a fresh ESC, so the KMP fallback table collapses to "mismatch: retry
/// as position 0, i.e. matched-1 iff b is ESC".
fn roll(idx: usize, b: u8, marker: &[u8]) -> usize {
    if b == marker[idx] {
        idx + 1
    } else if b == marker[0] {
        1
    } else {
        0
    }
}

impl Scanner {
    /// Scan one stdin chunk into events. Bytes between specials coalesce
    /// into as few `Forward` chunks as possible.
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        let mut plain: Vec<u8> = Vec::new();
        for &b in bytes {
            match std::mem::replace(&mut self.state, State::Normal(0)) {
                State::Normal(open_idx) => {
                    if b == LEADER {
                        flush(&mut plain, &mut out);
                        self.state = State::Leader;
                    } else {
                        // Forward immediately; the rolling match only decides
                        // whether chord interpretation turns off next.
                        plain.push(b);
                        let idx = roll(open_idx, b, PASTE_OPEN);
                        self.state = if idx == PASTE_OPEN.len() {
                            State::Paste(0)
                        } else {
                            State::Normal(idx)
                        };
                    }
                }
                State::Paste(close_idx) => {
                    // Verbatim passthrough: leader bytes, 0x1C, everything
                    // (AC5-HP). Only the close marker changes state.
                    plain.push(b);
                    let idx = roll(close_idx, b, PASTE_CLOSE);
                    self.state = if idx == PASTE_CLOSE.len() {
                        State::Normal(0)
                    } else {
                        State::Paste(idx)
                    };
                }
                State::Leader => {
                    if b == 0x1b {
                        self.state = State::LeaderEsc(vec![0x1b]);
                    } else {
                        out.push(chord(b));
                    }
                }
                State::LeaderEsc(mut seq) => {
                    seq.push(b);
                    if seq == PASTE_OPEN {
                        // AC5-EDGE: a paste-open lands while a chord is
                        // pending - BEL the dangling chord deterministically,
                        // forward the marker, enter paste mode. Paste content
                        // is never read as a chord.
                        out.push(Event::Bell);
                        flush(&mut plain, &mut out);
                        plain.extend_from_slice(PASTE_OPEN);
                        self.state = State::Paste(0);
                    } else if PASTE_OPEN.starts_with(&seq) {
                        // Still ambiguous between a chord and a marker: keep
                        // accumulating (split-across-reads safe).
                        self.state = State::LeaderEsc(seq);
                    } else {
                        match esc_chord(&seq) {
                            EscScan::Complete(ev) => out.push(ev),
                            EscScan::Partial => self.state = State::LeaderEsc(seq),
                            EscScan::Invalid => out.push(Event::Bell),
                        }
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
        // Block navigation (x-38c4). `[`/`]` follow tmux copy-mode muscle memory;
        // `x` stays ClosePane (block-select is `v`, vim-visual), `y` yanks the
        // selection, `r` reruns.
        b'[' => Event::BlockJump(BlockDir::Prev),
        b']' => Event::BlockJump(BlockDir::Next),
        b'v' => Event::BlockSelect(BlockDir::Prev),
        b'y' => Event::Cmd(Command::CopySelection),
        b'r' => Event::BlockRerun,
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
/// swallowed as one Bell. (The paste-open marker is peeled off by the caller
/// before this runs.)
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

    /// Concatenate every Forward chunk; assert nothing but forwards came out.
    fn forwarded_only(events: &[Event]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for e in events {
            match e {
                Event::Forward(chunk) => bytes.extend_from_slice(chunk),
                other => panic!("expected only forwards, got {other:?}"),
            }
        }
        bytes
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
    fn client_keys_block_navigation_chords_map_and_never_leak() {
        // AC-HP (Change 3): the x-38c4 chords produce their events and the chord
        // bytes never reach the pane. `x` stays ClosePane (block-select is `v`).
        assert_eq!(
            scan_all(&[b"\x02["]),
            vec![Event::BlockJump(BlockDir::Prev)]
        );
        assert_eq!(
            scan_all(&[b"\x02]"]),
            vec![Event::BlockJump(BlockDir::Next)]
        );
        assert_eq!(
            scan_all(&[b"\x02v"]),
            vec![Event::BlockSelect(BlockDir::Prev)]
        );
        assert_eq!(
            scan_all(&[b"\x02y"]),
            vec![Event::Cmd(Command::CopySelection)]
        );
        assert_eq!(scan_all(&[b"\x02r"]), vec![Event::BlockRerun]);
        assert_eq!(scan_all(&[b"\x02x"]), vec![Event::Cmd(Command::ClosePane)]);
    }

    #[test]
    fn client_keys_block_chord_bytes_are_verbatim_inside_a_paste() {
        // AC-EDGE (Change 3): a `[` / `]` arriving inside a bracketed paste is
        // pane content, not a chord - it forwards verbatim (same invariant the
        // existing table tests assert for leader bytes).
        let mut input = Vec::new();
        input.extend_from_slice(PASTE_OPEN);
        input.extend_from_slice(b"arr[0] = x\x02[\x02]");
        input.extend_from_slice(PASTE_CLOSE);
        let events = scan_all(&[&input]);
        assert_eq!(forwarded_only(&events), input);
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
    fn client_keys_ctrl_backslash_forwards_to_the_pane() {
        // Locked 11: the raw-0x1C detach is gone - Ctrl-\ is an ordinary
        // byte again (SIGQUIT reaches the child; AC5-UI's second half).
        assert_eq!(
            scan_all(&[b"abc\x1c"]),
            vec![Event::Forward(b"abc\x1c".to_vec())]
        );
        // Mid-chord it is just an unmapped chord key: swallowed with BEL.
        assert_eq!(scan_all(&[b"\x02\x1c"]), vec![Event::Bell]);
    }

    #[test]
    fn client_keys_paste_passes_leader_and_ctrl_backslash_verbatim() {
        // AC5-HP: everything between the markers - leader bytes, 0x1C -
        // forwards untouched, markers included; no chord, no detach.
        let mut input = Vec::new();
        input.extend_from_slice(PASTE_OPEN);
        input.extend_from_slice(b"safe \x02d and \x1c inside");
        input.extend_from_slice(PASTE_CLOSE);
        let events = scan_all(&[&input]);
        assert_eq!(forwarded_only(&events), input);
    }

    #[test]
    fn client_keys_paste_markers_split_one_byte_per_read_still_engage() {
        // AC5-ERR: the whole paste arrives one byte per read.
        let mut input = Vec::new();
        input.extend_from_slice(PASTE_OPEN);
        input.extend_from_slice(b"\x02"); // leader inside the paste
        input.extend_from_slice(PASTE_CLOSE);
        let chunks: Vec<&[u8]> = input.chunks(1).collect();
        let events = scan_all(&chunks);
        assert_eq!(forwarded_only(&events), input);
        // And chords work again after the close marker.
        let mut s = Scanner::default();
        for c in &chunks {
            s.scan(c);
        }
        assert_eq!(s.scan(b"\x02%"), vec![Event::Cmd(Command::SplitH)]);
    }

    #[test]
    fn client_keys_paste_open_during_pending_leader_bells_then_pastes() {
        // AC5-EDGE: leader pressed, then a paste-open arrives - the dangling
        // chord dies with one BEL, the marker forwards, paste mode engages
        // (the leader byte inside the paste is inert).
        let mut input = Vec::new();
        input.extend_from_slice(b"\x02");
        input.extend_from_slice(PASTE_OPEN);
        input.extend_from_slice(b"\x02x");
        input.extend_from_slice(PASTE_CLOSE);
        let events = scan_all(&[&input]);
        assert_eq!(events[0], Event::Bell, "dangling chord dies with BEL");
        let mut expect = Vec::new();
        expect.extend_from_slice(PASTE_OPEN);
        expect.extend_from_slice(b"\x02x");
        expect.extend_from_slice(PASTE_CLOSE);
        assert_eq!(forwarded_only(&events[1..]), expect);
    }

    #[test]
    fn client_keys_unterminated_paste_keeps_forwarding_leader_inert() {
        // AC5-FR: no close marker ever arrives. Bytes keep forwarding
        // verbatim (chords disabled, input never bricked).
        let mut s = Scanner::default();
        let mut input = PASTE_OPEN.to_vec();
        input.extend_from_slice(b"pasted");
        assert_eq!(s.scan(&input), vec![Event::Forward(input.clone())]);
        assert_eq!(
            s.scan(b"\x02d more"),
            vec![Event::Forward(b"\x02d more".to_vec())],
            "leader stays inert until 201~ or reconnect"
        );
    }

    #[test]
    fn client_keys_fizzled_marker_prefix_was_already_forwarded() {
        // ESC [ 2 J (clear screen, not a paste marker): every byte reaches
        // the pane and the scanner stays in Normal with chords live.
        let events = scan_all(&[b"\x1b[2J"]);
        assert_eq!(events, vec![Event::Forward(b"\x1b[2J".to_vec())]);
        let mut s = Scanner::default();
        s.scan(b"\x1b[20");
        assert_eq!(s.scan(b"\x02%"), vec![Event::Cmd(Command::SplitH)]);
    }
}
