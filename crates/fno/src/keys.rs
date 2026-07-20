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
//! `s` toggle status row · `?` key-table overlay · `d` detach · `[`/`]` jump
//! prev/next command block · `v` select block · `y` copy selection · `r` rerun
//! block (x-38c4) · `,` rename tab (x-c150) · leader-leader = one literal
//! leader byte · `<`/`>` reorder the active tab (x-0333). Leader + anything
//! unmapped is swallowed with BEL - a chord typo must never leak half a chord
//! into the pane (AC2-UI's never-leak guarantee).
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

use std::time::{Duration, Instant};

use crate::proto::{BlockDir, Command};
use crate::tree::Dir;

/// The leader byte: Ctrl-b (0x02).
pub const LEADER: u8 = 0x02;

/// After a resize chord fires, bare resize keys (`H/J/K/L`) keep resizing for
/// this long without re-pressing leader (tmux `bind -r` / `repeat-time`, 500ms
/// default). Each accepted repeat extends the window, so holding the key -
/// which the terminal auto-repeats far faster than 500ms - keeps resizing until
/// a genuine pause. Locked 2: this literal lives here and nowhere else.
pub const REPEAT_WINDOW: Duration = Duration::from_millis(500);

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
    /// Open the answer overlay (leader+a, x-c929). Overlay-mode keys (a digit
    /// answers, `n`/`N` cycle the blocked queue, Enter focuses, Esc closes) are
    /// interpreted by the client's view layer, not here (like OpenSelector).
    OpenAnswers,
    /// Show/hide the sideline (leader+b).
    TogglePanel,
    /// (x-b186) Cycle the sideline density slim -> regular -> extended
    /// (leader+B). Orthogonal to [`Event::TogglePanel`]: this changes how much
    /// each row shows, that changes whether the panel renders at all.
    CycleDensity,
    /// (x-b186) Toggle the extended table's order between by-squad and
    /// by-status (leader+o). Inert in the other densities, which render no
    /// table - but the preference still persists, so the choice survives a
    /// round trip through slim.
    ToggleAgentSort,
    /// Show/hide the status row (leader+s). Client-local (US4, AC4-FR).
    ToggleStatus,
    /// Show the full key-table overlay (leader+?). The next keypress
    /// dismisses it (US4, AC4-EDGE).
    ShowKeys,
    /// Jump the focused pane's shared scroll to the prev/next command block
    /// (leader+`[` / leader+`]`, x-38c4). The client resolves the focused pane.
    BlockJump(BlockDir),
    /// Move the focused pane's block selection (leader+v walks older, x-38c4).
    BlockSelect(BlockDir),
    /// Rerun the focused pane's selected block command (leader+r, x-38c4).
    BlockRerun,
    /// Dispatch the next ready backlog node into a new pane (leader+g, "grab
    /// work", x-6f77). The server shells the Python porcelain; no-work /
    /// lanes-full comes back as a one-line notice.
    DispatchNext,
    /// Open in-scrollback search on the focused pane (leader+/, x-e780). The
    /// client enters a local typing mode; the query and n/N/Esc are interpreted
    /// by the client's view layer, not here (like OpenSelector / OpenAnswers).
    SearchOpen,
    /// Open the session navigator (leader+f, x-653d): a global goto picker over
    /// a flat catalog of every squad/tab/agent/card. The client owns the typing
    /// mode (text filter, Tab state filter, Ctrl-n/p cursor, Enter goto); the
    /// chord only opens it (like SearchOpen).
    OpenNav,
    /// Open the rename-tab name overlay for the active tab (leader+,, tmux
    /// `rename-window` convention, x-c150). The client owns the typing mode
    /// and resolves the active tab's stable id; the chord only opens it.
    OpenRename,
    /// Reorder the active tab one slot within its squad (leader+`<`/`>`,
    /// x-0333). The client resolves the active tab's stable id before sending.
    ReorderTab(i32),
    /// Cycle the ACTIVE squad's sideline section one step through
    /// expanded -> live-only -> collapsed (leader+z, x-975a). The client owns
    /// the state and resolves the active squad; the chord only fires the step.
    CycleSection,
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
    /// When a resize repeat window is open, the instant it lapses. `None` when
    /// idle. Repeat is resize-only (Locked 3): a future focus-repeat would add
    /// a discriminant here, so this is deliberately not a bare bool.
    repeat_until: Option<Instant>,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner {
            state: State::Normal(0),
            repeat_until: None,
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
    /// into as few `Forward` chunks as possible. `now` is the caller's clock
    /// (client loop passes `Instant::now()`); the scanner reads time ONLY from
    /// it (Locked 4) so the resize repeat window is deterministic under test.
    pub fn scan(&mut self, bytes: &[u8], now: Instant) -> Vec<Event> {
        let mut out = Vec::new();
        let mut plain: Vec<u8> = Vec::new();
        for &b in bytes {
            match std::mem::replace(&mut self.state, State::Normal(0)) {
                State::Normal(open_idx) => {
                    if b == LEADER {
                        // Leader disarms first, then chords normally (Locked 5);
                        // a leader+resize re-arms at its emission site below.
                        self.repeat_until = None;
                        flush(&mut plain, &mut out);
                        self.state = State::Leader;
                    } else if let (true, Event::Cmd(Command::ResizeDir(dir))) =
                        (self.repeat_armed(now), chord(b))
                    {
                        // Bare resize key inside an open window: repeat the
                        // resize and extend the window (no leader needed).
                        flush(&mut plain, &mut out);
                        out.push(Event::Cmd(Command::ResizeDir(dir)));
                        self.repeat_until = Some(now + REPEAT_WINDOW);
                        self.state = State::Normal(0);
                    } else {
                        // Any non-repeat byte disarms (a no-op when idle) and is
                        // then processed exactly as if no window existed (Locked
                        // 5): forwarded immediately, rolling the paste-open match.
                        self.repeat_until = None;
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
                        let ev = chord(b);
                        self.arm_if_resize(&ev, now);
                        out.push(ev);
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
                            EscScan::Complete(ev) => {
                                self.arm_if_resize(&ev, now);
                                out.push(ev);
                            }
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

    /// A leader chord is mid-flight (US4): the client arms the which-key
    /// hint timer while this holds and clears the hint when it stops.
    pub fn leader_pending(&self) -> bool {
        matches!(self.state, State::Leader | State::LeaderEsc(_))
    }

    /// True while a resize repeat window is open at `now`.
    fn repeat_armed(&self, now: Instant) -> bool {
        self.repeat_until.is_some_and(|until| now < until)
    }

    /// Open (or extend) the repeat window to `now + REPEAT_WINDOW`. Public so a
    /// resize dispatched OUTSIDE `scan` (the which-key modal executes chords
    /// through its own path) arms the window the same as a typed resize would,
    /// keeping the modal's execution parity with directly-typed chords.
    pub fn arm_repeat(&mut self, now: Instant) {
        self.repeat_until = Some(now + REPEAT_WINDOW);
    }

    /// Close the repeat window now. Public so an input path that bypasses `scan`
    /// (a mouse click/scroll is stripped before the scanner sees it) can disarm
    /// the same as a non-resize keystroke does - otherwise a click that may have
    /// refocused a pane could be followed by a bare `H/J/K/L` that silently
    /// resizes.
    pub fn disarm_repeat(&mut self) {
        self.repeat_until = None;
    }

    /// Open the repeat window iff the just-emitted event is a resize; every
    /// resize emission funnels through here so the window arms the same way
    /// whether it fired from a letter chord or a Ctrl-arrow.
    fn arm_if_resize(&mut self, ev: &Event, now: Instant) {
        if matches!(ev, Event::Cmd(Command::ResizeDir(_))) {
            self.arm_repeat(now);
        }
    }
}

fn flush(plain: &mut Vec<u8>, out: &mut Vec<Event>) {
    if !plain.is_empty() {
        out.push(Event::Forward(std::mem::take(plain)));
    }
}

/// Which help-modal section a leader chord belongs to (x-8ccf). Declaration
/// order is the render order the which-key modal groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySection {
    Global,
    Navigation,
    WorkspacesTabs,
    Panes,
}

impl KeySection {
    /// The section header the modal renders (herdr anatomy: accent-colored).
    pub fn title(self) -> &'static str {
        match self {
            KeySection::Global => "global",
            KeySection::Navigation => "navigation",
            KeySection::WorkspacesTabs => "workspaces & tabs",
            KeySection::Panes => "panes",
        }
    }
}

/// One leader-chord binding: the single source of truth shared by the chord
/// dispatcher ([`chord`]) and the which-key modal renderer (x-8ccf, Locked 3).
/// Help that reads THIS cannot drift from what the dispatcher runs; the parity
/// test (`bindings_are_the_chord_table`) fails loudly if the two disagree.
pub struct KeyBinding {
    /// The post-leader byte. `disp` is how it prints (`%`, `hjkl`, `[`).
    pub key: u8,
    pub disp: &'static str,
    pub event: Event,
    pub section: KeySection,
    /// The action phrase the modal's right column shows.
    pub label: &'static str,
}

/// The authoritative leader-chord table. `chord()` looks a byte up here; the
/// modal renders these rows. The two `1-9` (select tab) and `C-b C-b` (literal)
/// chords are structural specials handled directly in `chord()` and shown by
/// [`meta_rows`], so they are deliberately absent here.
pub fn key_bindings() -> Vec<KeyBinding> {
    use Command as C;
    use Event::*;
    use KeySection::*;
    let b = |key, disp, event, section, label| KeyBinding {
        key,
        disp,
        event,
        section,
        label,
    };
    vec![
        // panes
        b(b'%', "%", Cmd(C::SplitH), Panes, "split horizontal"),
        b(b'"', "\"", Cmd(C::SplitV), Panes, "split vertical"),
        b(b'h', "h", Cmd(C::FocusDir(Dir::Left)), Panes, "focus left"),
        b(b'j', "j", Cmd(C::FocusDir(Dir::Down)), Panes, "focus down"),
        b(b'k', "k", Cmd(C::FocusDir(Dir::Up)), Panes, "focus up"),
        b(
            b'l',
            "l",
            Cmd(C::FocusDir(Dir::Right)),
            Panes,
            "focus right",
        ),
        b(
            b'H',
            "H",
            Cmd(C::ResizeDir(Dir::Left)),
            Panes,
            "resize left",
        ),
        b(
            b'J',
            "J",
            Cmd(C::ResizeDir(Dir::Down)),
            Panes,
            "resize down",
        ),
        b(b'K', "K", Cmd(C::ResizeDir(Dir::Up)), Panes, "resize up"),
        b(
            b'L',
            "L",
            Cmd(C::ResizeDir(Dir::Right)),
            Panes,
            "resize right",
        ),
        b(b'x', "x", Cmd(C::ClosePane), Panes, "close pane"),
        // workspaces & tabs
        b(b'c', "c", Cmd(C::NewTab), WorkspacesTabs, "new tab"),
        b(b'n', "n", Cmd(C::NextTab), WorkspacesTabs, "next tab"),
        b(b'p', "p", Cmd(C::PrevTab), WorkspacesTabs, "prev tab"),
        b(b'&', "&", Cmd(C::CloseTab), WorkspacesTabs, "close tab"),
        b(b',', ",", OpenRename, WorkspacesTabs, "rename tab"),
        b(
            b'z',
            "z",
            CycleSection,
            WorkspacesTabs,
            "cycle section view",
        ),
        b(b'<', "<", ReorderTab(-1), WorkspacesTabs, "move tab left"),
        b(b'>', ">", ReorderTab(1), WorkspacesTabs, "move tab right"),
        // navigation (scrollback blocks + goto/search)
        b(
            b'[',
            "[",
            BlockJump(BlockDir::Prev),
            Navigation,
            "jump prev block",
        ),
        b(
            b']',
            "]",
            BlockJump(BlockDir::Next),
            Navigation,
            "jump next block",
        ),
        b(
            b'v',
            "v",
            BlockSelect(BlockDir::Prev),
            Navigation,
            "select block",
        ),
        b(
            b'y',
            "y",
            Cmd(C::CopySelection),
            Navigation,
            "copy selection",
        ),
        b(b'r', "r", BlockRerun, Navigation, "rerun block"),
        b(b'/', "/", SearchOpen, Navigation, "search scrollback"),
        b(b'f', "f", OpenNav, Navigation, "find: goto pane/agent"),
        // global
        b(b'w', "w", OpenSelector, Global, "panel selector"),
        b(b'a', "a", OpenAnswers, Global, "answer queue"),
        b(b'b', "b", TogglePanel, Global, "toggle sideline"),
        b(b'B', "B", CycleDensity, Global, "cycle sideline density"),
        b(b'o', "o", ToggleAgentSort, Global, "sort agents: squad/status"),
        b(b's', "s", ToggleStatus, Global, "toggle status"),
        b(b'?', "?", ShowKeys, Global, "this key table"),
        b(
            b'g',
            "g",
            DispatchNext,
            Global,
            "grab work (dispatch next ready)",
        ),
        b(b'd', "d", Detach, Global, "detach"),
    ]
}

/// Display-only pseudo-bindings the modal shows but `chord()` handles as
/// structural specials (not simple byte lookups): the digit tab-select range
/// and the leader-leader literal. Kept beside [`key_bindings`] so the modal's
/// row set stays complete without polluting the executable table.
pub fn meta_rows() -> &'static [(&'static str, &'static str, KeySection)] {
    &[
        ("1-9", "select tab", KeySection::WorkspacesTabs),
        ("C-b C-b", "literal Ctrl-b", KeySection::Global),
    ]
}

/// The single-byte chord table. LEADER (literal) and the digit range are
/// structural specials; every other byte is resolved from [`key_bindings`], the
/// same table the which-key modal renders, so dispatch and help cannot diverge.
/// Resolve a post-leader byte to its [`Event`] as if the leader were held - the
/// which-key modal's execution path (x-8ccf US3): a keypress in the modal runs
/// EXACTLY what `prefix+<key>` runs, because both go through this one table.
/// `Event::Bell` means the byte is unbound (the modal dismisses on it).
pub fn resolve_chord(byte: u8) -> Event {
    chord(byte)
}

fn chord(b: u8) -> Event {
    match b {
        LEADER => Event::Forward(vec![LEADER]), // leader-leader = literal
        b'1'..=b'9' => Event::SelectTabIdx((b - b'1') as usize),
        _ => key_bindings()
            .into_iter()
            .find(|kb| kb.key == b)
            .map(|kb| kb.event)
            .unwrap_or(Event::Bell),
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
        // A single fixed instant: no chunk advances the clock, so the resize
        // repeat window (if a chord arms one) stays open across the chunks -
        // exactly what the non-timing tests want (they never send a bare resize
        // key after a resize chord, so arming is invisible to them).
        let now = Instant::now();
        let mut s = Scanner::default();
        chunks.iter().flat_map(|c| s.scan(c, now)).collect()
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
        assert_eq!(scan_all(&[b"\x02a"]), vec![Event::OpenAnswers]);
        assert_eq!(scan_all(&[b"\x02b"]), vec![Event::TogglePanel]);
        assert_eq!(scan_all(&[b"\x02s"]), vec![Event::ToggleStatus]);
        assert_eq!(scan_all(&[b"\x02?"]), vec![Event::ShowKeys]);
        assert_eq!(scan_all(&[b"\x02d"]), vec![Event::Detach]);
        assert_eq!(scan_all(&[b"\x02g"]), vec![Event::DispatchNext]);
        // leader+/ opens in-scrollback search (x-e780); the `/` never leaks.
        let searched = scan_all(&[b"a\x02/b"]);
        assert_eq!(
            searched,
            vec![
                Event::Forward(b"a".to_vec()),
                Event::SearchOpen,
                Event::Forward(b"b".to_vec()),
            ]
        );
        // leader+f opens the session navigator (x-653d); the `f` never leaks,
        // and leader+g stays "grab work" (DispatchNext, unchanged).
        assert_eq!(
            scan_all(&[b"a\x02fb"]),
            vec![
                Event::Forward(b"a".to_vec()),
                Event::OpenNav,
                Event::Forward(b"b".to_vec()),
            ]
        );
        assert_eq!(scan_all(&[b"\x02g"]), vec![Event::DispatchNext]);
    }

    #[test]
    fn client_keys_tab_organize_chords_leave_existing_bindings_intact() {
        assert_eq!(scan_all(&[b"\x02<"]), vec![Event::ReorderTab(-1)]);
        assert_eq!(scan_all(&[b"\x02>"]), vec![Event::ReorderTab(1)]);
        assert_eq!(scan_all(&[b"\x02,"]), vec![Event::OpenRename]);
        assert_eq!(
            scan_all(&[b"\x02J"]),
            vec![Event::Cmd(Command::ResizeDir(Dir::Down))]
        );
        assert_eq!(
            scan_all(&[b"\x02K"]),
            vec![Event::Cmd(Command::ResizeDir(Dir::Up))]
        );
        assert_eq!(scan_all(&[b"\x02x"]), vec![Event::Cmd(Command::ClosePane)]);
    }

    #[test]
    fn client_keys_leader_pending_tracks_chord_in_flight() {
        // US4: the which-key timer arms exactly while a chord is mid-flight.
        let now = Instant::now();
        let mut s = Scanner::default();
        s.scan(b"plain", now);
        assert!(!s.leader_pending());
        s.scan(b"\x02", now);
        assert!(s.leader_pending(), "bare leader held");
        s.scan(b"\x1b[", now); // partial leader-escape still pending
        assert!(s.leader_pending(), "split escape chord still pending");
        s.scan(b"C", now); // resolves to FocusDir(Right)
        assert!(!s.leader_pending(), "resolution clears pending");
        // A paste never reads as a pending chord.
        s.scan(b"\x1b[200~\x02", now);
        assert!(!s.leader_pending());
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
    fn bindings_are_the_chord_table() {
        // x-8ccf Locked 3 / parity: the which-key modal renders `key_bindings()`;
        // `chord()` dispatches through the same table. Assert they cannot diverge:
        // every table row's key resolves (via the real chord path) to exactly the
        // event the row advertises, and every key is listed once.
        let mut seen = std::collections::HashSet::new();
        for kb in key_bindings() {
            assert!(
                seen.insert(kb.key),
                "duplicate key {:?} in key_bindings()",
                kb.key as char
            );
            assert_eq!(
                chord(kb.key),
                kb.event,
                "chord({:?}) diverged from its key_bindings() row",
                kb.key as char
            );
            // The digit range and LEADER are structural specials, never table rows.
            assert!(
                !(b'1'..=b'9').contains(&kb.key) && kb.key != LEADER,
                "structural special {:?} must not appear in key_bindings()",
                kb.key as char
            );
        }
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
        let now = Instant::now();
        let mut s = Scanner::default();
        for c in &chunks {
            s.scan(c, now);
        }
        assert_eq!(s.scan(b"\x02%", now), vec![Event::Cmd(Command::SplitH)]);
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
        let now = Instant::now();
        let mut s = Scanner::default();
        let mut input = PASTE_OPEN.to_vec();
        input.extend_from_slice(b"pasted");
        assert_eq!(s.scan(&input, now), vec![Event::Forward(input.clone())]);
        assert_eq!(
            s.scan(b"\x02d more", now),
            vec![Event::Forward(b"\x02d more".to_vec())],
            "leader stays inert until 201~ or reconnect"
        );
    }

    #[test]
    fn client_keys_fizzled_marker_prefix_was_already_forwarded() {
        // ESC [ 2 J (clear screen, not a paste marker): every byte reaches
        // the pane and the scanner stays in Normal with chords live.
        let now = Instant::now();
        let events = scan_all(&[b"\x1b[2J"]);
        assert_eq!(events, vec![Event::Forward(b"\x1b[2J".to_vec())]);
        let mut s = Scanner::default();
        s.scan(b"\x1b[20", now);
        assert_eq!(s.scan(b"\x02%", now), vec![Event::Cmd(Command::SplitH)]);
    }

    const RESIZE_R: Event = Event::Cmd(Command::ResizeDir(Dir::Right));

    #[test]
    fn repeat_window_holds_resize_without_leader() {
        // AC1-HP: leader+L arms the window; bare L keeps resizing, each repeat
        // extending it. One leader chord + N bare keys -> N+1 Resize events.
        let mut s = Scanner::default();
        let t0 = Instant::now();
        assert_eq!(
            s.scan(b"\x02L", t0),
            vec![RESIZE_R],
            "leader+L resizes + arms"
        );
        // Three bare L within the window, 30ms apart (terminal auto-repeat rate).
        let mut t = t0;
        for _ in 0..3 {
            t += Duration::from_millis(30);
            assert_eq!(s.scan(b"L", t), vec![RESIZE_R], "bare L repeats the resize");
        }
    }

    #[test]
    fn repeat_window_extends_on_each_repeat() {
        // A bare L near the end of the window pushes the deadline out, so a
        // second bare L that would have missed the ORIGINAL window still lands.
        let mut s = Scanner::default();
        let t0 = Instant::now();
        // Arm at t0; the window lapses at t0 + 500.
        s.scan(b"\x02L", t0);
        // 400ms in: repeats, pushing the deadline out to t0 + 900.
        assert_eq!(
            s.scan(b"L", t0 + Duration::from_millis(400)),
            vec![RESIZE_R]
        );
        // 700ms in: past the ORIGINAL 500ms deadline but inside the extension.
        assert_eq!(
            s.scan(b"L", t0 + Duration::from_millis(700)),
            vec![RESIZE_R]
        );
    }

    #[test]
    fn repeat_window_lapses_after_the_window() {
        // AC2-HP: no input for >500ms lapses the window; the next bare resize
        // key takes its ordinary meaning (forwarded to the pane, no resize).
        let mut s = Scanner::default();
        let t0 = Instant::now();
        s.scan(b"\x02J", t0); // arm; until = t0 + 500
        assert_eq!(
            s.scan(b"J", t0 + Duration::from_millis(501)),
            vec![Event::Forward(b"J".to_vec())],
            "a bare J after the window forwards; it does not resize"
        );
    }

    #[test]
    fn repeat_window_disarms_and_forwards_a_non_resize_byte() {
        // AC3-ERR: any non-resize byte during the window disarms it and reaches
        // the pane byte-identically, and a following resize key no longer repeats.
        let mut s = Scanner::default();
        let t0 = Instant::now();
        s.scan(b"\x02L", t0);
        assert_eq!(
            s.scan(b"x", t0 + Duration::from_millis(100)),
            vec![Event::Forward(b"x".to_vec())],
            "the disarming byte passes straight through"
        );
        assert_eq!(
            s.scan(b"L", t0 + Duration::from_millis(130)),
            vec![Event::Forward(b"L".to_vec())],
            "window is gone: bare L now forwards instead of resizing"
        );
    }

    #[test]
    fn repeat_window_esc_disarms_immediately() {
        // AC5-FR: Esc is the explicit hatch - it disarms and is processed as
        // today (forwarded), and no resize fires from it.
        let mut s = Scanner::default();
        let t0 = Instant::now();
        s.scan(b"\x02K", t0);
        assert_eq!(
            s.scan(b"\x1b", t0 + Duration::from_millis(50)),
            vec![Event::Forward(b"\x1b".to_vec())]
        );
        assert_eq!(
            s.scan(b"K", t0 + Duration::from_millis(80)),
            vec![Event::Forward(b"K".to_vec())],
            "disarmed by Esc: bare K forwards"
        );
    }

    #[test]
    fn repeat_window_leader_disarms_then_chords_normally() {
        // Invariant: leader inside the window disarms first, then the chord runs
        // as usual - a leader+resize re-arms; a leader+other does not.
        let mut s = Scanner::default();
        let t0 = Instant::now();
        s.scan(b"\x02L", t0); // arm
        assert_eq!(
            s.scan(b"\x02%", t0 + Duration::from_millis(100)),
            vec![Event::Cmd(Command::SplitH)],
            "leader+% still splits inside the window"
        );
        // leader+% is not a resize, so the window is now closed: bare L forwards.
        assert_eq!(
            s.scan(b"L", t0 + Duration::from_millis(130)),
            vec![Event::Forward(b"L".to_vec())]
        );
    }

    #[test]
    fn repeat_window_ctrl_arrow_resize_also_arms() {
        // A resize can arm from a Ctrl-arrow chord too (not just a letter); the
        // repeat set itself stays the letters (the muscle-memory hold path).
        let mut s = Scanner::default();
        let t0 = Instant::now();
        assert_eq!(
            s.scan(b"\x02\x1b[1;5C", t0),
            vec![RESIZE_R],
            "leader+Ctrl-Right resizes right"
        );
        assert_eq!(
            s.scan(b"L", t0 + Duration::from_millis(40)),
            vec![RESIZE_R],
            "the window it armed accepts a bare L"
        );
    }

    #[test]
    fn repeat_window_never_arms_without_a_resize_chord() {
        // Today's behavior byte-for-byte when no resize has fired: a bare L is
        // just pane input. (scan_all uses a fixed clock and never resizes first.)
        assert_eq!(scan_all(&[b"L"]), vec![Event::Forward(b"L".to_vec())]);
        // A focus chord (leader+l) must NOT arm a resize window.
        let mut s = Scanner::default();
        let t0 = Instant::now();
        s.scan(b"\x02l", t0);
        assert_eq!(
            s.scan(b"L", t0 + Duration::from_millis(40)),
            vec![Event::Forward(b"L".to_vec())],
            "focus chord does not open a resize repeat window"
        );
    }

    #[test]
    fn repeat_window_public_arm_and_disarm_drive_the_window() {
        // arm_repeat opens a window a bare resize key repeats in (the modal
        // dispatch path uses this); disarm_repeat closes it (the mouse path).
        let mut s = Scanner::default();
        let t0 = Instant::now();
        s.arm_repeat(t0);
        assert_eq!(
            s.scan(b"L", t0 + Duration::from_millis(40)),
            vec![RESIZE_R],
            "arm_repeat opens the window without a preceding chord"
        );
        s.disarm_repeat();
        assert_eq!(
            s.scan(b"L", t0 + Duration::from_millis(60)),
            vec![Event::Forward(b"L".to_vec())],
            "disarm_repeat closes it: bare L forwards again"
        );
    }

    #[test]
    fn repeat_window_flood_emits_one_resize_per_key() {
        // AC4-EDGE (scanner half): a flood of bare H within the window emits one
        // ResizeDir(Left) each - the MIN-size clamp is the server's job, tested
        // there; the scanner just keeps emitting without error.
        let mut s = Scanner::default();
        let t0 = Instant::now();
        s.scan(b"\x02H", t0);
        let mut t = t0;
        for _ in 0..20 {
            t += Duration::from_millis(15);
            assert_eq!(
                s.scan(b"H", t),
                vec![Event::Cmd(Command::ResizeDir(Dir::Left))]
            );
        }
    }
}
