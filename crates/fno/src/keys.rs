//! The client-side prefix-key layer: a pure, stateful scanner over raw stdin
//! bytes producing forward-chunks and mux events.
//!
//! Interpretation is CLIENT-side by design (Locked Decision 5): the server
//! only ever sees `Command`s, never key chords. The scanner is a pure state
//! machine so it is exhaustively unit-testable, including escape sequences
//! split across reads (raw-mode stdin arrives in arbitrary chunks).
//!
//! Table (prefix = Ctrl-b, tmux-compatible where a binding exists):
//! `%`/`"` split H/V · `h j k l` + arrows focus · `H J K L` + Ctrl-arrows
//! resize · Shift-arrows move the pane · `x` close pane · `c` new tab ·
//! `n`/`p` cycle tabs · `1`-`9`
//! select tab · `&` close tab · `w` sideline row selector · `b` toggle sideline ·
//! `s` toggle status row · `?` key-table overlay · `d` detach · `[`/`]` jump
//! prev/next command block · `v` select block · `y` copy selection · `r` rerun
//! block (x-38c4) · `,` rename tab (x-c150) · prefix-prefix = one literal
//! prefix byte · `<`/`>` reorder the active tab (x-0333). Prefix + anything
//! unmapped is swallowed with BEL - a chord typo must never leak half a chord
//! into the pane (AC2-UI's never-leak guarantee).
//!
//! Detach is prefix+d ONLY (Phase 3 Locked 11): the Phase 1/2 raw-0x1C
//! match is gone, so Ctrl-\ forwards to the pane and SIGQUIT works again.
//!
//! Bracketed-paste passthrough (US5): `ESC[200~` puts the scanner in a
//! verbatim state where every byte - prefix bytes, Ctrl-\, everything -
//! forwards untouched until `ESC[201~`; both markers forward too. Marker
//! matching is a rolling index that survives read boundaries (AC5-ERR), and
//! bytes are never held back: a marker prefix that fizzles was already
//! forwarded as the ordinary bytes it turned out to be. Residual (accepted,
//! documented): an unterminated paste (no `201~` ever) leaves chords
//! disabled until the close marker or reconnect - input keeps forwarding
//! verbatim and EOF/terminal-close still detaches, so the state machine can
//! disable chords at worst, never brick input (AC5-FR). Unbracketed paste
//! can still trigger prefix chords - the tmux-class residual (Locked 11).

use std::time::{Duration, Instant};

use crate::proto::{BlockDir, Command};
use crate::tree::Dir;

/// The built-in prefix byte: Ctrl-b (0x02), tmux's. `config.mux.prefix`
/// replaces it; [`prefix`] is what the scanner actually compares against.
pub const DEFAULT_PREFIX: u8 = 0x02;

/// The resolved key layer for this process: the prefix byte plus any per-action
/// rebinds from `config.mux`.
///
/// Installed ONCE, before the scanner runs, by whichever front door owns the
/// client (see [`install`]). Everything downstream - the chord dispatcher, the
/// which-key modal, the parity test - reads it through [`key_bindings`], so a
/// rebind cannot reach the dispatcher without also reaching the help that
/// documents it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    pub prefix: u8,
    /// `(action id, post-prefix byte)`, overriding that action's table default.
    pub rebinds: Vec<(String, u8)>,
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap {
            prefix: DEFAULT_PREFIX,
            rebinds: Vec::new(),
        }
    }
}

/// One rejected config entry, so the caller can say WHY a rebind did not take
/// rather than silently running the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapWarning(pub String);

/// Parse a key spec into the byte a terminal sends for it.
///
/// Accepted: `C-a` / `Ctrl-a` / `^a` for a control byte, or a single printable
/// ASCII character for itself. Deliberately narrow: a spec vocabulary wider than
/// the scanner (which dispatches on ONE post-prefix byte) would advertise binds
/// that could never fire.
pub fn parse_key(spec: &str) -> Option<u8> {
    let s = spec.trim();
    let ctrl_letter = |c: char| -> Option<u8> {
        c.is_ascii_alphabetic()
            .then(|| (c.to_ascii_lowercase() as u8) - b'a' + 1)
    };
    let mut chars = s.chars();
    match (chars.next(), s.len()) {
        (Some(c), 1) if c.is_ascii_graphic() => Some(c as u8),
        (Some('^'), 2) => ctrl_letter(s.chars().nth(1)?),
        _ => {
            let lower = s.to_ascii_lowercase();
            let rest = lower
                .strip_prefix("ctrl-")
                .or_else(|| lower.strip_prefix("c-"))?;
            (rest.chars().count() == 1).then(|| ctrl_letter(rest.chars().next()?))?
        }
    }
}

/// How a byte prints in the which-key modal: `C-b` for a control byte, the
/// character itself otherwise.
fn key_disp(b: u8) -> String {
    match b {
        1..=26 => format!("C-{}", (b - 1 + b'a') as char),
        _ => (b as char).to_string(),
    }
}

/// Build a [`Keymap`] from raw config strings, reporting every entry it had to
/// reject. Pure, so the resolution rules are testable without a config file.
///
/// A rebind is refused (not silently applied) when it would make the keyboard
/// unusable: an unparseable spec, an unknown action, a digit (the `1-9` tab
/// range is structural), a byte two actions would share, or the prefix byte
/// itself. Refusing keeps the previous behaviour, which is always reachable;
/// accepting a collision would silently shadow one of the two. The PREFIX is
/// held to the digit rule too: it is checked against the shipped table, and a
/// digit prefix would quietly delete one entry from the `1-9` tab range.
///
/// Collisions are judged against the FINAL assignment. Checking entries one at a
/// time rejects every swap and cycle, because whichever the TOML map order hands
/// over first sees the other action still on its target key.
///
/// The prefix participates: `chord()` resolves it before consulting the table,
/// so an action left on the prefix byte is unreachable while the key table still
/// advertises it. A prefix landing on a DEFAULT binding loses (one refusal keeps
/// every chord); a REBIND landing on the prefix loses instead.
pub fn resolve_keymap(
    prefix_spec: Option<&str>,
    rebinds: &[(String, String)],
) -> (Keymap, Vec<KeymapWarning>) {
    let mut warnings = Vec::new();
    let mut prefix = DEFAULT_PREFIX;
    if let Some(spec) = prefix_spec.map(str::trim).filter(|s| !s.is_empty()) {
        match parse_key(spec) {
            // The same structural rule the rebinds get, and for a sharper
            // reason: `chord()` resolves the prefix BEFORE the `1-9` branch, so
            // a digit prefix does not lose a chord, it removes one tab from a
            // range the modal goes on advertising in full.
            Some(b) if b.is_ascii_digit() && b != b'0' => warnings.push(KeymapWarning(format!(
                "config.mux.prefix: 1-9 select tabs and cannot be the prefix; keeping {}",
                key_disp(DEFAULT_PREFIX)
            ))),
            Some(b) => prefix = b,
            None => warnings.push(KeymapWarning(format!(
                "config.mux.prefix: cannot read {spec:?} as a key; keeping {}",
                key_disp(DEFAULT_PREFIX)
            ))),
        }
    }
    let defaults: Vec<(String, u8)> = default_bindings()
        .iter()
        .map(|kb| (kb.action.to_string(), kb.key))
        .collect();

    // Pass 1: everything judgeable about an entry on its own.
    let mut proposed: Vec<(String, u8)> = Vec::new();
    for (action, spec) in rebinds {
        let action = action.trim().to_ascii_lowercase();
        if !defaults.iter().any(|(a, _)| *a == action) {
            warnings.push(KeymapWarning(format!(
                "config.mux.keys.{action}: no such action (see `prefix+?` for the list)"
            )));
            continue;
        }
        let Some(byte) = parse_key(spec) else {
            warnings.push(KeymapWarning(format!(
                "config.mux.keys.{action}: cannot read {spec:?} as a key"
            )));
            continue;
        };
        if byte.is_ascii_digit() && byte != b'0' {
            warnings.push(KeymapWarning(format!(
                "config.mux.keys.{action}: 1-9 select tabs and cannot be rebound"
            )));
            continue;
        }
        proposed.retain(|(a, _)| *a != action); // a repeated action: last wins
        proposed.push((action, byte));
    }

    // Pass 2: drop conflicts from the assignment they would actually produce,
    // one per round, until it settles. Each round removes a proposal or gives up
    // the configured prefix, so it terminates.
    loop {
        let mut final_map = defaults.clone();
        for (action, byte) in &proposed {
            if let Some(slot) = final_map.iter_mut().find(|(a, _)| a == action) {
                slot.1 = *byte;
            }
        }
        // A key sitting on the prefix byte can never dispatch.
        if let Some((action, _)) = final_map.iter().find(|(_, k)| *k == prefix) {
            let action = action.clone();
            if let Some(i) = proposed.iter().position(|(a, _)| *a == action) {
                warnings.push(KeymapWarning(format!(
                    "config.mux.keys.{action}: {} is the prefix, so the chord \
                     would never fire",
                    key_disp(prefix)
                )));
                proposed.remove(i);
            } else {
                warnings.push(KeymapWarning(format!(
                    "config.mux.prefix: {} is already {action}; rebind that \
                     action first, or the prefix shadows it. Keeping {}",
                    key_disp(prefix),
                    key_disp(DEFAULT_PREFIX)
                )));
                prefix = DEFAULT_PREFIX;
            }
            continue;
        }
        // Two actions on one byte: the later PROPOSAL loses, so a swap survives
        // (each half moves off the other's key) while a genuine double-booking
        // is refused.
        let dup = final_map.iter().enumerate().find_map(|(i, (_, k))| {
            final_map[i + 1..]
                .iter()
                .find(|(_, k2)| k2 == k)
                .map(|(a2, _)| (final_map[i].0.clone(), a2.clone(), *k))
        });
        let Some((first, second, byte)) = dup else {
            break;
        };
        let loser = [&second, &first]
            .into_iter()
            .find_map(|a| proposed.iter().position(|(p, _)| p == a).map(|i| (i, a)));
        match loser {
            Some((i, action)) => {
                let other = if action == &first { &second } else { &first };
                warnings.push(KeymapWarning(format!(
                    "config.mux.keys.{action}: {} would also be {other}",
                    key_disp(byte)
                )));
                proposed.remove(i);
            }
            // Two DEFAULTS on one byte would be a table bug, not a config one;
            // the parity test guards it, and there is nothing to drop here.
            None => break,
        }
    }
    (
        Keymap {
            prefix,
            rebinds: proposed,
        },
        warnings,
    )
}

static KEYMAP: std::sync::OnceLock<Keymap> = std::sync::OnceLock::new();

/// Install the resolved keymap. First call wins; later calls are ignored, so a
/// re-attach in the same process cannot swap the keyboard mid-session.
pub fn install(map: Keymap) {
    let _ = KEYMAP.set(map);
}

fn keymap() -> &'static Keymap {
    KEYMAP.get_or_init(Keymap::default)
}

/// The prefix byte in force. The scanner compares against THIS, never the
/// const, so `config.mux.prefix` reaches every chord.
pub fn prefix() -> u8 {
    keymap().prefix
}

/// After a resize chord fires, bare resize keys (`H/J/K/L`) keep resizing for
/// this long without re-pressing prefix (tmux `bind -r` / `repeat-time`, 500ms
/// default). Each accepted repeat extends the window, so holding the key -
/// which the terminal auto-repeats far faster than 500ms - keeps resizing until
/// a genuine pause. Locked 2: this literal lives here and nowhere else.
pub const REPEAT_WINDOW: Duration = Duration::from_millis(500);

/// The hold grace for pane identity reveal. It covers the initial terminal
/// autorepeat delay while keeping a tap transient.
pub const PANE_IDS_REPEAT_WINDOW: Duration = Duration::from_millis(750);

/// Bracketed-paste markers, as the terminal emits them.
const PASTE_OPEN: &[u8] = b"\x1b[200~";
const PASTE_CLOSE: &[u8] = b"\x1b[201~";

/// (x-e10f) The GLOBAL sideline chord's prefix: Ctrl+Opt+arrow is
/// `ESC [ 1 ; 7 X` (xterm modifier 7 = 1 + Alt 2 + Ctrl 4), the rung above
/// the prefix-gated ctrl(5)/shift(2) arrows in [`esc_chord`]. Shared by the
/// `ChordEsc` scanner branch (which holds and releases candidates against it)
/// and [`esc_chord`] itself, so the scanner and the parser cannot disagree
/// about what the chord looks like. Only `D` (Left) is bound.
const GLOBAL_CHORD_PREFIX: &[u8] = b"\x1b[1;7";

/// One scanned outcome. `Forward` chunks are byte-exact pass-through - bare
/// bytes are NEVER re-encoded (AC2-UI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Forward(Vec<u8>),
    Cmd(Command),
    /// Prefix+digit: select the Nth tab of the viewed squad. The scanner
    /// only knows the index; the client resolves it to a stable `TabId`
    /// against its last `Layout` (v3: `SelectTab` names ids, not indices).
    SelectTabIdx(usize),
    Detach,
    /// Open the sideline selector (prefix+w). Selector-mode keys are
    /// interpreted by the client's view layer, not here.
    OpenSelector,
    /// Open the answer overlay (prefix+a, x-c929). Overlay-mode keys (a digit
    /// answers, `n`/`N` cycle the blocked queue, Enter focuses, Esc closes) are
    /// interpreted by the client's view layer, not here (like OpenSelector).
    OpenAnswers,
    /// Open the yard overlay (prefix+m, x-b2bf): the fleet as f[no]nimals -
    /// collection - one eye glyph per citizen, one spotlight sprite at a
    /// time. Overlay-mode keys (`n`/`N` pick, `q`/Esc close) are interpreted
    /// by the client's view layer, not here (like OpenAnswers).
    OpenYard,
    /// Show/hide the sideline (prefix+b).
    TogglePanel,
    /// (x-b186) Cycle the sideline density slim -> regular -> extended
    /// (prefix+B). Orthogonal to [`Event::TogglePanel`]: this changes how much
    /// each row shows, that changes whether the panel renders at all.
    CycleDensity,
    /// (x-b186) Toggle the extended table's order between by-squad and
    /// by-status (prefix+o). Inert in the other densities, which render no
    /// table - but the preference still persists, so the choice survives a
    /// round trip through slim.
    ToggleAgentSort,
    /// Show/hide the status row (prefix+s). Client-local (US4, AC4-FR).
    ToggleStatus,
    /// Reveal each visible pane's stable id while the key repeats.
    ShowPaneIds,
    /// Show the full key-table overlay (prefix+?). The next keypress
    /// dismisses it (US4, AC4-EDGE).
    ShowKeys,
    /// Jump the focused pane's shared scroll to the prev/next command block
    /// (prefix+`[` / prefix+`]`, x-38c4). The client resolves the focused pane.
    BlockJump(BlockDir),
    /// Move the focused pane's block selection (prefix+v walks older, x-38c4).
    BlockSelect(BlockDir),
    /// Rerun the focused pane's selected block command (prefix+r, x-38c4).
    BlockRerun,
    /// Dispatch the next ready backlog node into a new pane (prefix+g, "grab
    /// work", x-6f77). The server shells the Python porcelain; no-work and
    /// refusal outcomes come back as a one-line notice.
    DispatchNext,
    /// Open in-scrollback search on the focused pane (prefix+/, x-e780). The
    /// client enters a local typing mode; the query and n/N/Esc are interpreted
    /// by the client's view layer, not here (like OpenSelector / OpenAnswers).
    SearchOpen,
    /// Open the session navigator (prefix+f, x-653d): a global goto picker over
    /// a flat catalog of every squad/tab/agent/card. The client owns the typing
    /// mode (text filter, Tab state filter, Ctrl-n/p cursor, Enter goto); the
    /// chord only opens it (like SearchOpen).
    OpenNav,
    /// Open the rename-tab name overlay for the active tab (prefix+,, tmux
    /// `rename-window` convention, x-c150). The client owns the typing mode
    /// and resolves the active tab's stable id; the chord only opens it.
    OpenRename,
    /// Reorder the active tab one slot within its squad (prefix+`<`/`>`,
    /// x-0333). The client resolves the active tab's stable id before sending.
    ReorderTab(i32),
    /// Cycle the ACTIVE squad's sideline section one step through
    /// expanded -> live-only -> collapsed (prefix+z, x-975a). The client owns
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
    /// (x-e10f) Accumulating a GLOBAL chord candidate that began in `Normal`
    /// (Ctrl+Opt+Left = `ESC [ 1 ; 7 D`): bytes are HELD, not forwarded, and
    /// release to the pane the moment the sequence diverges from the chord
    /// prefix - with the paste-open roll re-run over the released bytes - so
    /// a partial chord never leaks mid-sequence and a lone Esc is released as
    /// soon as the next byte says it is not the chord.
    ChordEsc(Vec<u8>),
    /// Saw the prefix; the next key (or escape sequence) is a chord.
    Prefix,
    /// Accumulating an escape sequence after the prefix (arrows / Ctrl-arrows
    /// / Shift-arrows / a paste-open marker), possibly split across reads.
    PrefixEsc(Vec<u8>),
    /// Inside a bracketed paste: everything forwards verbatim; `usize` is
    /// the rolling PASTE_CLOSE match index.
    Paste(usize),
}

/// The scanner. One per client connection; state survives across reads so a
/// chord or marker split at a read boundary still lands.
#[derive(Debug)]
pub struct Scanner {
    state: State,
    /// The repeatable action and its deadline. Keeping the action beside the
    /// deadline prevents a pane-id pulse from being mistaken for resize input.
    repeat: Option<RepeatState>,
    keymap: Keymap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepeatAction {
    Resize,
    ShowPaneIds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RepeatState {
    action: RepeatAction,
    until: Instant,
}

impl Default for Scanner {
    fn default() -> Self {
        Scanner {
            state: State::Normal(0),
            repeat: None,
            keymap: keymap().clone(),
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
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            i += 1;
            // (x-e10f fix) A released chord candidate re-dispatches its LAST
            // byte through the Normal arms (see ChordEsc below): replay drops
            // the pre-increment so the same byte runs again in the new state.
            let mut replay = false;
            match std::mem::replace(&mut self.state, State::Normal(0)) {
                State::Normal(open_idx) => {
                    if b == self.keymap.prefix {
                        // Prefix disarms first, then chords normally (Locked 5);
                        // a prefix+resize re-arms at its emission site below.
                        self.repeat = None;
                        flush(&mut plain, &mut out);
                        self.state = State::Prefix;
                    } else if b == 0x1b {
                        // (x-e10f) A bare ESC may open the global sideline
                        // chord (Ctrl+Opt+Left). Hold it in ChordEsc instead
                        // of forwarding; it releases on the next byte unless
                        // the chord prefix keeps matching. A non-repeat byte
                        // disarms the repeat window, like any other.
                        self.repeat = None;
                        self.state = State::ChordEsc(vec![0x1b]);
                    } else if self.repeat_armed(RepeatAction::Resize, now) {
                        let Event::Cmd(Command::ResizeDir(dir)) = self.chord(b) else {
                            self.repeat = None;
                            plain.push(b);
                            let idx = roll(open_idx, b, PASTE_OPEN);
                            self.state = if idx == PASTE_OPEN.len() {
                                State::Paste(0)
                            } else {
                                State::Normal(idx)
                            };
                            continue;
                        };
                        // Bare resize key inside an open window: repeat the
                        // resize and extend the window (no prefix needed).
                        flush(&mut plain, &mut out);
                        out.push(Event::Cmd(Command::ResizeDir(dir)));
                        self.arm_repeat(now);
                        self.state = State::Normal(0);
                    } else if self.repeat_armed(RepeatAction::ShowPaneIds, now)
                        && self.chord(b) == Event::ShowPaneIds
                    {
                        flush(&mut plain, &mut out);
                        out.push(Event::ShowPaneIds);
                        self.arm_show_pane_ids(now);
                        self.state = State::Normal(0);
                    } else {
                        // Any non-repeat byte disarms (a no-op when idle) and is
                        // then processed exactly as if no window existed (Locked
                        // 5): forwarded immediately, rolling the paste-open match.
                        self.repeat = None;
                        plain.push(b);
                        let idx = roll(open_idx, b, PASTE_OPEN);
                        self.state = if idx == PASTE_OPEN.len() {
                            State::Paste(0)
                        } else {
                            State::Normal(idx)
                        };
                    }
                }
                State::ChordEsc(mut seq) => {
                    // (x-e10f) Held global-chord candidate. Accumulate only
                    // while the bytes still spell the Ctrl+Opt-arrow prefix;
                    // a completed prefix plus its final byte resolves through
                    // `esc_chord` - the SAME parse the prefix path uses, not a
                    // second parser. On divergence everything EXCEPT the
                    // diverging byte releases to the pane (paste-open roll
                    // re-run over it; rolling from 0 is exact because the
                    // candidate starts ESC) and the diverging byte RE-DISPATCHES
                    // through the Normal arms - a lone Esc answered by the
                    // prefix key must still enter Prefix, exactly as it did
                    // before the hold (the F2 regression the first cut shipped).
                    seq.push(b);
                    if GLOBAL_CHORD_PREFIX.starts_with(&seq) {
                        self.state = State::ChordEsc(seq);
                    } else if seq.len() == GLOBAL_CHORD_PREFIX.len() + 1 {
                        match esc_chord(&seq) {
                            EscScan::Complete(ev) => {
                                // Consumed, never forwarded: the accepted cost
                                // of a global grab (AC12) - a program inside a
                                // pane that binds Ctrl+Opt+Arrow loses it.
                                flush(&mut plain, &mut out);
                                out.push(ev);
                                self.state = State::Normal(0);
                            }
                            // An unbound Ctrl+Opt final: release + re-dispatch.
                            _ => {
                                let held = &seq[..seq.len() - 1];
                                let idx = release_to_plain(&mut plain, held);
                                self.state = paste_state(idx);
                                replay = true;
                            }
                        }
                    } else {
                        let held = &seq[..seq.len() - 1];
                        let idx = release_to_plain(&mut plain, held);
                        self.state = paste_state(idx);
                        replay = true;
                    }
                }
                State::Paste(close_idx) => {
                    // Verbatim passthrough: prefix bytes, 0x1C, everything
                    // (AC5-HP). Only the close marker changes state.
                    plain.push(b);
                    let idx = roll(close_idx, b, PASTE_CLOSE);
                    self.state = if idx == PASTE_CLOSE.len() {
                        State::Normal(0)
                    } else {
                        State::Paste(idx)
                    };
                }
                State::Prefix => {
                    if b == 0x1b {
                        self.state = State::PrefixEsc(vec![0x1b]);
                    } else {
                        let ev = self.chord(b);
                        self.arm_if_repeat(&ev, now);
                        out.push(ev);
                    }
                }
                State::PrefixEsc(mut seq) => {
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
                        self.state = State::PrefixEsc(seq);
                    } else {
                        match esc_chord(&seq) {
                            EscScan::Complete(ev) => {
                                self.arm_if_repeat(&ev, now);
                                out.push(ev);
                            }
                            EscScan::Partial => self.state = State::PrefixEsc(seq),
                            EscScan::Invalid => out.push(Event::Bell),
                        }
                    }
                }
            }
            if replay {
                i -= 1;
            }
        }
        flush(&mut plain, &mut out);
        out
    }

    /// (x-e10f) A global-chord candidate is held, waiting for more bytes.
    /// The client read loop polls this to arm its quiet-window flush, the
    /// analog of tmux's escape-time: a lone Esc must not wait for the next
    /// keypress forever.
    pub fn chord_pending(&self) -> bool {
        matches!(self.state, State::ChordEsc(_))
    }

    /// (x-e10f fix) Release a held global-chord candidate once input has gone
    /// quiet past the flush window: the held bytes forward to the pane exactly
    /// as a divergence release would send them, and the paste-open roll
    /// resumes where the hold paused it. `None` when nothing is held. This is
    /// the F1 fix: without it, Esc-to-cancel inside a pane (vim, fzf) stalled
    /// until the next keystroke.
    pub fn flush_chord(&mut self) -> Option<Event> {
        if let State::ChordEsc(seq) = std::mem::replace(&mut self.state, State::Normal(0)) {
            let mut plain = Vec::new();
            let idx = release_to_plain(&mut plain, &seq);
            self.state = paste_state(idx);
            (!plain.is_empty()).then(|| Event::Forward(plain))
        } else {
            None
        }
    }

    /// A prefix chord is mid-flight (US4): the client arms the which-key
    /// hint timer while this holds and clears the hint when it stops.
    pub fn prefix_pending(&self) -> bool {
        matches!(self.state, State::Prefix | State::PrefixEsc(_))
    }

    /// True while the selected repeat action's window is open at `now`.
    fn repeat_armed(&self, action: RepeatAction, now: Instant) -> bool {
        self.repeat
            .is_some_and(|state| state.action == action && now < state.until)
    }

    /// Open (or extend) the repeat window to `now + REPEAT_WINDOW`. Public so a
    /// resize dispatched OUTSIDE `scan` (the which-key modal executes chords
    /// through its own path) arms the window the same as a typed resize would,
    /// keeping the modal's execution parity with directly-typed chords.
    pub fn arm_repeat(&mut self, now: Instant) {
        self.repeat = Some(RepeatState {
            action: RepeatAction::Resize,
            until: now + REPEAT_WINDOW,
        });
    }

    fn arm_show_pane_ids(&mut self, now: Instant) {
        self.repeat = Some(RepeatState {
            action: RepeatAction::ShowPaneIds,
            until: now + PANE_IDS_REPEAT_WINDOW,
        });
    }

    /// Close the repeat window now. Public so an input path that bypasses `scan`
    /// (a mouse click/scroll is stripped before the scanner sees it) can disarm
    /// the same as a non-resize keystroke does - otherwise a click that may have
    /// refocused a pane could be followed by a bare `H/J/K/L` that silently
    /// resizes.
    pub fn disarm_repeat(&mut self) {
        self.repeat = None;
    }

    /// Open the matching repeat window for an emitted repeatable event. Resize
    /// emission funnels through here so letter and Ctrl-arrow paths agree.
    pub fn arm_if_repeat(&mut self, ev: &Event, now: Instant) {
        match ev {
            Event::Cmd(Command::ResizeDir(_)) => self.arm_repeat(now),
            Event::ShowPaneIds => self.arm_show_pane_ids(now),
            _ => {}
        }
    }

    #[cfg(test)]
    fn with_keymap(map: Keymap) -> Self {
        Scanner {
            state: State::Normal(0),
            repeat: None,
            keymap: map,
        }
    }

    fn chord(&self, byte: u8) -> Event {
        chord_for(&self.keymap, byte)
    }
}

fn flush(plain: &mut Vec<u8>, out: &mut Vec<Event>) {
    if !plain.is_empty() {
        out.push(Event::Forward(std::mem::take(plain)));
    }
}

/// (x-e10f) Release a held global-chord candidate to the forwarded stream,
/// re-running the paste-open roll over every released byte (the hold paused
/// the roll, so it catches up byte-for-byte). Rolling from 0 is exact: the
/// candidate starts with ESC, and `roll` reaches 1 on an ESC from ANY prior
/// index, so the first released byte erases whatever index was held.
fn release_to_plain(plain: &mut Vec<u8>, seq: &[u8]) -> usize {
    let mut idx = 0;
    for &b in seq {
        plain.push(b);
        idx = roll(idx, b, PASTE_OPEN);
    }
    idx
}

/// The scanner state a paste-open roll index leaves behind.
fn paste_state(idx: usize) -> State {
    if idx == PASTE_OPEN.len() {
        State::Paste(0)
    } else {
        State::Normal(idx)
    }
}

/// Which help-modal section a prefix chord belongs to (x-8ccf). Declaration
/// order is the render order the which-key modal groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySection {
    Global,
    Navigation,
    WorkspacesTabs,
    Panes,
    /// (x-f300) Bare keys the sideline handles on the SELECTED row - no prefix.
    /// They are not chords, so they carry no [`KeyBinding`]; the modal shows them
    /// through [`meta_rows`] purely as reference, which is why removing a dead
    /// row was undiscoverable before.
    SidelineRows,
}

impl KeySection {
    /// The section header the modal renders (herdr anatomy: accent-colored).
    pub fn title(self) -> &'static str {
        match self {
            KeySection::Global => "global",
            KeySection::Navigation => "navigation",
            KeySection::WorkspacesTabs => "workspaces & tabs",
            KeySection::Panes => "panes",
            KeySection::SidelineRows => "sideline rows (no prefix)",
        }
    }
}

/// One prefix-chord binding: the single source of truth shared by the chord
/// dispatcher ([`chord`]) and the which-key modal renderer (x-8ccf, Locked 3).
/// Help that reads THIS cannot drift from what the dispatcher runs; the parity
/// test (`bindings_are_the_chord_table`) fails loudly if the two disagree.
pub struct KeyBinding {
    /// The post-prefix byte. `disp` is how it prints (`%`, `hjkl`, `[`).
    pub key: u8,
    pub disp: String,
    pub event: Event,
    pub section: KeySection,
    /// The action phrase the modal's right column shows.
    pub label: &'static str,
    /// The stable id `config.mux.keys.<action>` rebinds. Distinct from `label`
    /// (prose, free to be reworded) and from `disp` (which IS the thing being
    /// rebound), so a config file written today keeps working when the help text
    /// is rephrased tomorrow.
    pub action: &'static str,
}

/// The authoritative prefix-chord table AS SHIPPED, before `config.mux.keys` is
/// applied. Call [`key_bindings`] instead unless you specifically want the
/// defaults (the rebind resolver does, to know which actions exist).
fn default_bindings() -> Vec<KeyBinding> {
    use Command as C;
    use Event::*;
    use KeySection::*;
    let b = |key: u8, action, event, section, label| KeyBinding {
        key,
        disp: key_disp(key),
        event,
        section,
        label,
        action,
    };
    vec![
        // panes
        b(b'%', "split-h", Cmd(C::SplitH), Panes, "split horizontal"),
        b(b'"', "split-v", Cmd(C::SplitV), Panes, "split vertical"),
        b(
            b'h',
            "focus-left",
            Cmd(C::FocusDir(Dir::Left)),
            Panes,
            "focus left",
        ),
        b(
            b'j',
            "focus-down",
            Cmd(C::FocusDir(Dir::Down)),
            Panes,
            "focus down",
        ),
        b(
            b'k',
            "focus-up",
            Cmd(C::FocusDir(Dir::Up)),
            Panes,
            "focus up",
        ),
        b(
            b'l',
            "focus-right",
            Cmd(C::FocusDir(Dir::Right)),
            Panes,
            "focus right",
        ),
        b(
            b'H',
            "resize-left",
            Cmd(C::ResizeDir(Dir::Left)),
            Panes,
            "resize left",
        ),
        b(
            b'J',
            "resize-down",
            Cmd(C::ResizeDir(Dir::Down)),
            Panes,
            "resize down",
        ),
        b(
            b'K',
            "resize-up",
            Cmd(C::ResizeDir(Dir::Up)),
            Panes,
            "resize up",
        ),
        b(
            b'L',
            "resize-right",
            Cmd(C::ResizeDir(Dir::Right)),
            Panes,
            "resize right",
        ),
        b(b'x', "close-pane", Cmd(C::ClosePane), Panes, "close pane"),
        b(
            b'D',
            "diff-pane",
            Cmd(C::ToggleDiffPane {
                agent: None,
                pane: None,
            }),
            Panes,
            "toggle git diff pane",
        ),
        // workspaces & tabs
        b(b'c', "new-tab", Cmd(C::NewTab), WorkspacesTabs, "new tab"),
        b(
            b'n',
            "next-tab",
            Cmd(C::NextTab),
            WorkspacesTabs,
            "next tab",
        ),
        b(
            b'p',
            "prev-tab",
            Cmd(C::PrevTab),
            WorkspacesTabs,
            "prev tab",
        ),
        b(
            b'&',
            "close-tab",
            Cmd(C::CloseTab),
            WorkspacesTabs,
            "close tab",
        ),
        b(b',', "rename-tab", OpenRename, WorkspacesTabs, "rename tab"),
        b(
            b'z',
            "cycle-section",
            CycleSection,
            WorkspacesTabs,
            "cycle active section",
        ),
        b(
            b'<',
            "move-tab-left",
            ReorderTab(-1),
            WorkspacesTabs,
            "move tab left",
        ),
        b(
            b'>',
            "move-tab-right",
            ReorderTab(1),
            WorkspacesTabs,
            "move tab right",
        ),
        // navigation (scrollback blocks + goto/search)
        b(
            b'[',
            "prev-block",
            BlockJump(BlockDir::Prev),
            Navigation,
            "jump prev block",
        ),
        b(
            b']',
            "next-block",
            BlockJump(BlockDir::Next),
            Navigation,
            "jump next block",
        ),
        b(
            b'v',
            "select-block",
            BlockSelect(BlockDir::Prev),
            Navigation,
            "select block",
        ),
        b(
            b'y',
            "copy-selection",
            Cmd(C::CopySelection),
            Navigation,
            "copy selection",
        ),
        b(b'r', "rerun-block", BlockRerun, Navigation, "rerun block"),
        b(b'/', "search", SearchOpen, Navigation, "search scrollback"),
        // The label names every row class `nav_rows()` actually emits. The old
        // `goto pane/agent` undersold the one selector in the mux with no
        // nine-item ceiling, so the capped digit chords read as the way to
        // reach a squad or a tab and this read as a lesser tool.
        b(
            b'f',
            "find",
            OpenNav,
            Navigation,
            "find: goto squad/tab/pane/agent",
        ),
        // global
        b(
            b'w',
            "selector",
            OpenSelector,
            Global,
            "sideline row selector",
        ),
        b(b'a', "answers", OpenAnswers, Global, "answer queue"),
        b(
            b'm',
            "yard",
            OpenYard,
            Global,
            "the yard (fleet as a menagerie)",
        ),
        b(
            b'b',
            "toggle-sideline",
            TogglePanel,
            Global,
            "toggle sideline",
        ),
        b(
            b'B',
            "cycle-density",
            CycleDensity,
            Global,
            "cycle sideline density",
        ),
        b(
            b'o',
            "sort-agents",
            ToggleAgentSort,
            Global,
            "sort table columns",
        ),
        b(b's', "toggle-status", ToggleStatus, Global, "toggle status"),
        b(
            b'\\',
            "show-pane-ids",
            ShowPaneIds,
            Panes,
            "hold to show pane ids",
        ),
        b(b'?', "show-keys", ShowKeys, Global, "this key table"),
        b(
            b'g',
            "grab-work",
            DispatchNext,
            Global,
            "grab work (dispatch next ready)",
        ),
        b(b'd', "detach", Detach, Global, "detach"),
    ]
}

/// The prefix-chord table IN FORCE: [`default_bindings`] with the installed
/// keymap's rebinds applied. `chord()` looks a byte up here and the which-key
/// modal renders these rows, so a rebound key dispatches and documents itself
/// from the same place - the help cannot advertise a chord the scanner does not
/// run. The `1-9` (select tab) and prefix-prefix (literal) chords are structural
/// specials handled in `chord()` and shown by [`meta_rows`], so they are
/// deliberately absent here and refused as rebind targets.
pub fn key_bindings() -> Vec<KeyBinding> {
    bindings_for(keymap())
}

fn bindings_for(map: &Keymap) -> Vec<KeyBinding> {
    let mut rows = default_bindings();
    for (action, byte) in &map.rebinds {
        if let Some(kb) = rows.iter_mut().find(|kb| kb.action == action) {
            kb.key = *byte;
            kb.disp = key_disp(*byte);
        }
    }
    rows
}

/// The display glyph for `action_id` resolved from the LIVE keymap (the shipped
/// defaults plus any `config.mux.keys` rebinds), so a menu hint advertises the
/// chord the dispatcher actually runs - never a literal that a rebind has
/// silently moved. `None` when no binding carries that action id; the caller
/// renders no hint.
pub fn key_for(action_id: &str) -> Option<String> {
    disp_for(action_id, &key_bindings())
}

/// `key_for` over a supplied table. Split out so a test can assert a rebind
/// moves the glyph without touching the process-global keymap (`install` is
/// first-call-wins), the same reason the prefix_hint rebind test resolves
/// locally rather than installing.
fn disp_for(action_id: &str, bindings: &[KeyBinding]) -> Option<String> {
    bindings
        .iter()
        .find(|kb| kb.action == action_id)
        .map(|kb| kb.disp.clone())
}

/// One in-menu accelerator: the byte that selects an entry while a context
/// menu is OPEN, under the same stable action-id vocabulary the prefix table
/// uses. A SEPARATE scope, not a rebind: prefix chords read a byte only after
/// the prefix, menu keys read it bare inside a popup, so the two layers never
/// consult each other's tables (ruling d-a5e7569d - `x` stays kill-pane after
/// a prefix and becomes the destructive verb inside a menu).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuKeyBinding {
    pub action: &'static str,
    pub key: u8,
}

/// The shipped in-menu accelerator table: app vocabulary inside the open menu
/// (`r` renames, `x` is the destructive verb, `n` and the angle brackets drive
/// the tab verbs). One byte MAY serve two action ids here (`close-tab` and
/// `remove-row` never share a popup), so collision rules live with the caller,
/// which knows which menu is open. A static slice: lookups run per popup row
/// at build time and per offered action on every in-menu keypress.
pub const MENU_BINDINGS: &[MenuKeyBinding] = &[
    MenuKeyBinding {
        action: "rename-tab",
        key: b'r',
    },
    MenuKeyBinding {
        action: "rename-workspace",
        key: b'r',
    },
    MenuKeyBinding {
        action: "close-tab",
        key: b'x',
    },
    MenuKeyBinding {
        action: "remove-row",
        key: b'x',
    },
    MenuKeyBinding {
        action: "new-tab",
        key: b'n',
    },
    MenuKeyBinding {
        action: "move-tab-left",
        key: b'<',
    },
    MenuKeyBinding {
        action: "move-tab-right",
        key: b'>',
    },
];

/// The one resolver both menu-scope projections read: the binding registered
/// for `action_id`, when the menu scope carries one.
fn menu_binding_for(action_id: &str) -> Option<&'static MenuKeyBinding> {
    MENU_BINDINGS.iter().find(|mb| mb.action == action_id)
}

/// The display glyph for `action_id` in menu scope: what the popup draws
/// beside the entry, i.e. the byte that RUNS it in-menu. Unlike [`key_for`]
/// (the prefix chord) this describes the input path the reader is actually
/// on. `None` when the menu scope does not bind the action - a prefix-only
/// action renders no menu key rather than advertising a chord the open menu
/// would dismiss on (the LD9 lie this node closes).
pub fn menu_key_for(action_id: &str) -> Option<String> {
    menu_binding_for(action_id).map(|mb| key_disp(mb.key))
}

/// The dispatch half of [`menu_key_for`]: the byte that executes `action_id`
/// in-menu, for the popup key handler to match a typed byte against the
/// actions the OPEN menu actually offers.
pub fn menu_byte_for(action_id: &str) -> Option<u8> {
    menu_binding_for(action_id).map(|mb| mb.key)
}

/// The one-line teaser shown while a prefix is pending, built from the LIVE
/// bindings.
///
/// This was a hardcoded string of shipped keys. `config.mux.keys` moves the
/// dispatch table underneath it, so a rebound action was advertised on a key
/// that now BELs while the key it does answer on went unmentioned. Fixing the
/// full `prefix+?` modal was not enough: this is a SECOND surface onto the same
/// table, and a rebind has to reach every one of them or the feature reads as
/// broken from whichever surface was missed.
///
/// A teaser, not the key list: it names one action per group and sends the
/// reader to `?` for the rest. An action missing from the table drops silently
/// rather than printing a gap.
pub fn prefix_hint() -> String {
    // (actions, how their keys join, the phrase that follows). No actions means
    // a literal entry: the digit range is structural, not a binding.
    const GROUPS: &[(&[&str], &str, &str)] = &[
        (&["split-h", "split-v"], " ", "split"),
        (
            &["focus-left", "focus-down", "focus-up", "focus-right"],
            "",
            "focus",
        ),
        (
            &["resize-left", "resize-down", "resize-up", "resize-right"],
            "",
            "resize",
        ),
        (&["close-pane"], "", "close"),
        (&["new-tab"], "", "tab"),
        (&["next-tab", "prev-tab"], "/", "cycle"),
        (&[], "", "1-9 tab"),
        (&["close-tab"], "", "close-tab"),
        (&["selector"], "", "select"),
        (&["toggle-sideline"], "", "sideline"),
        (&["grab-work"], "", "grab"),
        (&["find"], "", "find"),
        (&["search"], "", "search"),
        (&["toggle-status"], "", "status"),
        (&["detach"], "", "detach"),
        (&["show-keys"], "", "all keys"),
    ];
    let rows = key_bindings();
    let mut parts: Vec<String> = Vec::new();
    for (actions, sep, phrase) in GROUPS {
        if actions.is_empty() {
            parts.push(phrase.to_string());
            continue;
        }
        let keys: Vec<String> = actions
            .iter()
            .filter_map(|a| rows.iter().find(|kb| kb.action == *a))
            .map(|kb| kb.disp.clone())
            .collect();
        if !keys.is_empty() {
            parts.push(format!("{} {phrase}", keys.join(sep)));
        }
    }
    format!(" {}", parts.join(" \u{b7} "))
}

/// Display-only pseudo-bindings the modal shows but `chord()` handles as
/// structural specials (not simple byte lookups): the digit tab-select range
/// and the prefix-prefix literal. Kept beside [`key_bindings`] so the modal's
/// row set stays complete without polluting the executable table.
///
/// The literal-prefix row is built from the LIVE prefix: a frozen `C-b C-b`
/// would advertise a dead sequence the moment anyone set `config.mux.prefix`.
pub fn meta_rows() -> Vec<(String, String, KeySection)> {
    let p = key_disp(prefix());
    vec![
        // The digit range is nine by construction (one byte, nine of them), so
        // the row states its own ceiling and names the uncapped way past it
        // rather than leaving a 14-tab operator to discover `f` by accident.
        (
            "1-9".into(),
            "select tab (first 9; f goes past)".into(),
            KeySection::WorkspacesTabs,
        ),
        (
            format!("{p} {p}"),
            format!("literal {p}"),
            KeySection::Global,
        ),
        // (x-e10f) The global sideline chord: a multi-byte CSI, so it lives
        // HERE with the other display-only rows rather than in the single-byte
        // key_bindings table the modal executes from - the scanner's ChordEsc
        // branch dispatches it, not chord().
        (
            "Ctrl+Opt+Left".into(),
            "sideline (global, no prefix)".into(),
            KeySection::Global,
        ),
        // (x-f300) The dead-row removal paths. Bare sideline keys, not chords -
        // listed here so the reference names them; Enter on them BELs.
        (
            "x".into(),
            "stop a live row · remove a dead one".into(),
            KeySection::SidelineRows,
        ),
        (
            "X".into(),
            "reap all exited agents".into(),
            KeySection::SidelineRows,
        ),
        (
            "right-click".into(),
            // (x-7683) All three triggers of the same menu, so a terminal that
            // never forwards the button does not read as a dead feature. The
            // header-only clear-dead action stays named here too - it was the
            // only in-app documentation of that behavior (x-7683 review). The
            // hold duration formats from the client's one constant, so a
            // retune can never leave this label stale.
            format!(
                "context menu · or m · or hold L {}ms · on a header: clear dead",
                crate::client::MENU_LONG_PRESS.as_millis()
            ),
            KeySection::SidelineRows,
        ),
        // Workspace-row rename/reorder, only reachable inside the prefix+w
        // selector today (selector_keys) - listed here so the reference names
        // them instead of leaving an operator to discover them by accident,
        // or not at all.
        (
            format!("{p} w then r"),
            "rename the focused workspace row".into(),
            KeySection::SidelineRows,
        ),
        (
            format!("{p} w then J/K"),
            "move the focused workspace row down/up".into(),
            KeySection::SidelineRows,
        ),
        (
            format!("{p} w then x"),
            "remove the focused workspace (confirm)".into(),
            KeySection::SidelineRows,
        ),
    ]
}

/// The single-byte chord table. PREFIX (literal) and the digit range are
/// structural specials; every other byte is resolved from [`key_bindings`], the
/// same table the which-key modal renders, so dispatch and help cannot diverge.
/// Resolve a post-prefix byte to its [`Event`] as if the prefix were held - the
/// which-key modal's execution path (x-8ccf US3): a keypress in the modal runs
/// EXACTLY what `prefix+<key>` runs, because both go through this one table.
/// `Event::Bell` means the byte is unbound (the modal dismisses on it).
pub fn resolve_chord(byte: u8) -> Event {
    chord(byte)
}

fn chord(b: u8) -> Event {
    chord_for(keymap(), b)
}

fn chord_for(map: &Keymap, b: u8) -> Event {
    match b {
        // prefix-prefix = one literal prefix byte, whatever the prefix now is.
        _ if b == map.prefix => Event::Forward(vec![b]),
        b'1'..=b'9' => Event::SelectTabIdx((b - b'1') as usize),
        _ => bindings_for(map)
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

/// Arrows (`ESC [ A..D` -> focus), Ctrl-arrows (`ESC [ 1 ; 5 A..D` -> resize),
/// Shift-arrows (`ESC [ 1 ; 2 A..D` -> move the pane, x-aa95) after the
/// prefix, and the one GLOBAL rung: Ctrl+Opt-Left (`ESC [ 1 ; 7 D` -> open
/// the sideline, x-e10f), which the `ChordEsc` branch also reaches without a
/// prefix. Anything that stops matching every prefix is swallowed as one Bell.
/// (The paste-open marker is peeled off by the caller before this runs.)
///
/// Shift-arrow rather than shifted `HJKL`, which the resize binds already own:
/// the arrow forms share one modifier ladder (plain focus -> ctrl resize ->
/// shift move), so the move bind reads as one more rung rather than as a
/// letter picked because the obvious one was taken. Ctrl+Opt (modifier 7) is
/// the free rung above them, and only Left is bound on it.
fn esc_chord(seq: &[u8]) -> EscScan {
    const PLAIN: &[u8] = b"\x1b[";
    const CTRL: &[u8] = b"\x1b[1;5";
    const SHIFT: &[u8] = b"\x1b[1;2";
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
    // Complete Shift-arrow: ESC [ 1 ; 2 X. `target: None` - the server resolves
    // the destination by direction, so this rides the same `move_leaf` the drop
    // path does (Locked Decision 4: one mutation path, two gestures).
    if seq.len() == 6 && seq.starts_with(SHIFT) {
        return match arrow(seq[5]) {
            Some(dir) => EscScan::Complete(Event::Cmd(Command::MovePane {
                mover: None,
                target: None,
                dir,
            })),
            None => EscScan::Invalid,
        };
    }
    // Complete Ctrl+Opt-arrow: ESC [ 1 ; 7 X. Only Left (D) is bound - the
    // sideline chord. From the prefix this rung is Invalid on the other
    // finals (one Bell, like any unbound chord); from the global ChordEsc
    // branch the caller forwards them instead.
    if seq.len() == 6 && seq.starts_with(GLOBAL_CHORD_PREFIX) {
        return match seq[5] {
            b'D' => EscScan::Complete(Event::OpenSelector),
            _ => EscScan::Invalid,
        };
    }
    if seq.len() < 6
        && (CTRL.starts_with(seq) || SHIFT.starts_with(seq) || GLOBAL_CHORD_PREFIX.starts_with(seq))
    {
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
    fn client_keys_prefix_chords_map_and_never_leak() {
        let events = scan_all(&[b"a\x02%b"]);
        // 'a' forwards, prefix+% commands, 'b' forwards - the chord bytes
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
        // prefix+/ opens in-scrollback search (x-e780); the `/` never leaks.
        let searched = scan_all(&[b"a\x02/b"]);
        assert_eq!(
            searched,
            vec![
                Event::Forward(b"a".to_vec()),
                Event::SearchOpen,
                Event::Forward(b"b".to_vec()),
            ]
        );
        // prefix+f opens the session navigator (x-653d); the `f` never leaks,
        // and prefix+g stays "grab work" (DispatchNext, unchanged).
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
    fn sort_shortcut_and_z_section_shortcut_do_not_overlap() {
        assert_eq!(resolve_chord(b'o'), Event::ToggleAgentSort);
        assert_eq!(resolve_chord(b'z'), Event::CycleSection);
        let bindings = key_bindings();
        assert_eq!(
            bindings
                .iter()
                .find(|binding| binding.key == b'o')
                .unwrap()
                .label,
            "sort table columns"
        );
        assert_eq!(
            bindings
                .iter()
                .find(|binding| binding.key == b'z')
                .unwrap()
                .label,
            "cycle active section"
        );
    }

    #[test]
    fn client_keys_shift_arrow_moves_the_pane() {
        // x-aa95: the arrow ladder is plain=focus, ctrl=resize, shift=move.
        // Both ids ride as None - the bind knows a direction and nothing else,
        // so the server resolves the focused pane and its neighbour.
        assert_eq!(
            scan_all(&[b"\x02\x1b[1;2B"]),
            vec![Event::Cmd(Command::MovePane {
                mover: None,
                target: None,
                dir: Dir::Down
            })]
        );
        assert_eq!(
            scan_all(&[b"\x02\x1b[1;2D"]),
            vec![Event::Cmd(Command::MovePane {
                mover: None,
                target: None,
                dir: Dir::Left
            })]
        );
        // The neighbouring rungs are untouched.
        assert_eq!(
            scan_all(&[b"\x02\x1b[1;5A"]),
            vec![Event::Cmd(Command::ResizeDir(Dir::Up))]
        );
        assert_eq!(
            scan_all(&[b"\x02\x1b[A"]),
            vec![Event::Cmd(Command::FocusDir(Dir::Up))]
        );
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
    fn client_keys_prefix_pending_tracks_chord_in_flight() {
        // US4: the which-key timer arms exactly while a chord is mid-flight.
        let now = Instant::now();
        let mut s = Scanner::default();
        s.scan(b"plain", now);
        assert!(!s.prefix_pending());
        s.scan(b"\x02", now);
        assert!(s.prefix_pending(), "bare prefix held");
        s.scan(b"\x1b[", now); // partial prefix-escape still pending
        assert!(s.prefix_pending(), "split escape chord still pending");
        s.scan(b"C", now); // resolves to FocusDir(Right)
        assert!(!s.prefix_pending(), "resolution clears pending");
        // A paste never reads as a pending chord.
        s.scan(b"\x1b[200~\x02", now);
        assert!(!s.prefix_pending());
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
        // existing table tests assert for prefix bytes).
        let mut input = Vec::new();
        input.extend_from_slice(PASTE_OPEN);
        input.extend_from_slice(b"arr[0] = x\x02[\x02]");
        input.extend_from_slice(PASTE_CLOSE);
        let events = scan_all(&[&input]);
        assert_eq!(forwarded_only(&events), input);
    }

    #[test]
    fn client_keys_prefix_prefix_sends_one_literal_prefix() {
        assert_eq!(
            scan_all(&[b"\x02\x02"]),
            vec![Event::Forward(vec![DEFAULT_PREFIX])]
        );
    }

    #[test]
    fn parse_key_reads_the_spec_forms_and_refuses_the_rest() {
        assert_eq!(parse_key("C-a"), Some(0x01));
        assert_eq!(parse_key("Ctrl-a"), Some(0x01));
        assert_eq!(parse_key("^a"), Some(0x01));
        assert_eq!(parse_key("c-B"), Some(0x02), "case-insensitive");
        assert_eq!(parse_key(" C-b "), Some(0x02), "trimmed");
        assert_eq!(parse_key("q"), Some(b'q'));
        assert_eq!(parse_key("?"), Some(b'?'));
        // Nothing the scanner could dispatch on: it reads ONE post-prefix byte,
        // so a spec it cannot reduce to one byte must be refused, not guessed.
        assert_eq!(parse_key("C-"), None);
        assert_eq!(parse_key("C-ab"), None);
        assert_eq!(parse_key("alt-x"), None);
        assert_eq!(parse_key("F5"), None);
        assert_eq!(parse_key(""), None);
        assert_eq!(parse_key("C-1"), None, "control of a non-letter");
    }

    #[test]
    fn resolve_keymap_applies_a_prefix_and_a_rebind() {
        let (map, warn) = resolve_keymap(
            Some("C-a"),
            &[("detach".into(), "Q".into()), ("SEARCH".into(), "?".into())],
        );
        // The id is case-folded, so `SEARCH` resolves - and then collides with
        // `?`, which is already the key table.
        assert!(
            warn.iter().any(|w| w.0.contains("would also be show-keys")),
            "{warn:?}"
        );
        assert_eq!(map.prefix, 0x01);
        assert_eq!(map.rebinds, vec![("detach".to_string(), b'Q')]);
    }

    #[test]
    fn resolve_keymap_refuses_rather_than_breaking_the_keyboard() {
        // Each refusal keeps a keyboard that WORKS. Applying any of these would
        // shadow a binding or bind a chord that can never fire.
        let cases: [(&str, &str, &str); 4] = [
            ("detach", "F5", "cannot read"),
            ("teleport", "T", "no such action"),
            ("detach", "3", "select tabs"),
            ("detach", "c", "would also be new-tab"),
        ];
        for (action, spec, expect) in cases {
            let (map, warn) = resolve_keymap(None, &[(action.into(), spec.into())]);
            assert!(map.rebinds.is_empty(), "{action}={spec} should not apply");
            assert!(
                warn.iter().any(|w| w.0.contains(expect)),
                "{action}={spec} should say {expect:?}, said {warn:?}"
            );
        }
        // A bad prefix keeps Ctrl-b rather than leaving the session prefixless.
        let (map, warn) = resolve_keymap(Some("meta-q"), &[]);
        assert_eq!(map.prefix, DEFAULT_PREFIX);
        assert!(warn[0].0.contains("config.mux.prefix"));
    }

    #[test]
    fn every_surface_that_shows_a_key_reads_the_same_table() {
        // The pending-prefix hint was a hardcoded string, so a rebind reached
        // the dispatch table and the `?` modal but not this line. Both surfaces
        // are generated now; this asserts they agree rather than that either
        // one contains a particular character.
        let rows = key_bindings();
        let hint = prefix_hint();
        // Byte-identical to the string this generator replaced. Generating it
        // was meant to change nothing an operator sees until they rebind
        // something, so the shipped rendering is worth pinning.
        assert_eq!(
            hint,
            " % \" split · hjkl focus · HJKL resize · x close · c tab · n/p cycle \
             · 1-9 tab · & close-tab · w select · b sideline · g grab · f find \
             · / search · s status · d detach · ? all keys"
        );
        let disp = |action: &str| {
            rows.iter()
                .find(|kb| kb.action == action)
                .map(|kb| kb.disp.clone())
                .expect("action is in the shipped table")
        };
        for action in ["detach", "find", "search", "show-keys", "new-tab"] {
            assert!(
                hint.contains(&disp(action)),
                "the hint must name {action} on the key it actually answers on \
                 ({}), hint was {hint:?}",
                disp(action)
            );
        }
        // A rebind moves the hint with it. Resolved locally rather than
        // installed, since `install` is a process-global OnceLock.
        let (map, warn) = resolve_keymap(None, &[("detach".into(), "Q".into())]);
        assert!(warn.is_empty(), "Q is free: {warn:?}");
        assert_eq!(map.rebinds, vec![("detach".to_string(), b'Q')]);
        let rebound: Vec<KeyBinding> = {
            let mut rows = default_bindings();
            for (action, byte) in &map.rebinds {
                if let Some(kb) = rows.iter_mut().find(|kb| kb.action == action) {
                    kb.key = *byte;
                    kb.disp = key_disp(*byte);
                }
            }
            rows
        };
        assert_eq!(
            rebound
                .iter()
                .find(|kb| kb.action == "detach")
                .map(|kb| kb.disp.as_str()),
            Some("Q"),
            "the table moved, so the hint built from it moves too"
        );
    }

    #[test]
    fn key_for_resolves_the_live_glyph_and_moves_on_a_rebind() {
        // Default table: detach answers on `d`; an unknown action has no hint.
        assert_eq!(
            disp_for("detach", &default_bindings()).as_deref(),
            Some("d")
        );
        assert_eq!(disp_for("no-such-action", &default_bindings()), None);
        // `key_for` reads the live (global) table; an unknown action is None
        // under any installed keymap.
        assert_eq!(key_for("no-such-action"), None);
        // A rebind moves the resolved glyph a menu hint would show. Resolved
        // locally, not via `install` (process-global, first-call-wins), mirroring
        // the prefix_hint rebind test above.
        let (map, warn) = resolve_keymap(None, &[("detach".into(), "Q".into())]);
        assert!(warn.is_empty(), "Q is free: {warn:?}");
        let mut rows = default_bindings();
        for (action, byte) in &map.rebinds {
            if let Some(kb) = rows.iter_mut().find(|kb| kb.action == action) {
                kb.key = *byte;
                kb.disp = key_disp(*byte);
            }
        }
        assert_eq!(
            disp_for("detach", &rows).as_deref(),
            Some("Q"),
            "the rebind moved the glyph a menu hint resolves"
        );
    }

    #[test]
    fn a_digit_prefix_is_refused_like_a_digit_rebind() {
        // The final-map collision check only sees NAMED bindings, so `3` looked
        // free. It is not: `chord()` resolves the prefix before the structural
        // `1-9` branch, so `prefix+3` would forward a literal `3` while the key
        // modal went on advertising the whole range. A quietly missing tab is
        // worse than a refusal, which at least says why.
        for spec in ["1", "3", "9"] {
            let (map, warn) = resolve_keymap(Some(spec), &[]);
            assert_eq!(map.prefix, DEFAULT_PREFIX, "prefix={spec} must not apply");
            assert!(
                warn.iter().any(|w| w.0.contains("1-9 select tabs")),
                "prefix={spec} should say why, said {warn:?}"
            );
        }
        // Asserted on the resolver rather than through `chord()`, because
        // `install` is process-global and first-call-wins: a test that installs
        // a keymap decides the keyboard for whichever tests run after it.
        //
        // `0` is not in the tab range, so it stays a legal prefix.
        assert_eq!(resolve_keymap(Some("0"), &[]).0.prefix, b'0');
    }

    #[test]
    fn resolve_keymap_swaps_two_bound_keys() {
        // A REAL exchange: `n` and `p` trade places, neither moving to a free
        // key first. Checking entries one at a time against a half-applied map
        // refuses both (whichever comes first sees the other still parked on its
        // target), so this is the case that pins judging the FINAL assignment.
        for order in [
            vec![("next-tab", "p"), ("prev-tab", "n")],
            vec![("prev-tab", "n"), ("next-tab", "p")],
        ] {
            let entries: Vec<(String, String)> = order
                .iter()
                .map(|(a, k)| ((*a).to_string(), (*k).to_string()))
                .collect();
            let (map, warn) = resolve_keymap(None, &entries);
            assert!(warn.is_empty(), "a swap is legal: {warn:?}");
            assert_eq!(map.rebinds.len(), 2, "both halves of the swap apply");
            let key = |a: &str| map.rebinds.iter().find(|(x, _)| x == a).map(|(_, k)| *k);
            assert_eq!((key("next-tab"), key("prev-tab")), (Some(b'p'), Some(b'n')));
        }
    }

    #[test]
    fn resolve_keymap_keeps_a_three_way_cycle() {
        // The same rule at one more step: no ordering of a cycle has a free key
        // to start from, so any sequential check rejects all three.
        let (map, warn) = resolve_keymap(
            None,
            &[
                ("focus-left".into(), "j".into()),
                ("focus-down".into(), "k".into()),
                ("focus-up".into(), "h".into()),
            ],
        );
        assert!(warn.is_empty(), "a cycle is legal: {warn:?}");
        assert_eq!(map.rebinds.len(), 3);
    }

    #[test]
    fn resolve_keymap_refuses_a_prefix_that_shadows_a_chord() {
        // `chord()` matches the prefix byte BEFORE the table, so an action left
        // on the prefix is unreachable while the key table still advertises it.
        // The prefix loses, because refusing it keeps every chord while the
        // alternative silently costs one.
        let (map, warn) = resolve_keymap(Some("d"), &[]);
        assert_eq!(map.prefix, DEFAULT_PREFIX);
        assert!(
            warn.iter().any(|w| w.0.contains("is already detach")),
            "{warn:?}"
        );
        // Moving the action out of the way first makes the same prefix legal.
        let (map, warn) = resolve_keymap(Some("d"), &[("detach".into(), "Q".into())]);
        assert!(warn.is_empty(), "{warn:?}");
        assert_eq!(map.prefix, b'd');
        assert_eq!(map.rebinds, vec![("detach".to_string(), b'Q')]);
        // And a REBIND onto the prefix loses instead, since the prefix is the
        // more global choice.
        let (map, warn) = resolve_keymap(Some("C-a"), &[("detach".into(), "C-a".into())]);
        assert_eq!(map.prefix, 0x01);
        assert!(map.rebinds.is_empty());
        assert!(
            warn.iter().any(|w| w.0.contains("is the prefix")),
            "{warn:?}"
        );
    }

    #[test]
    fn meta_rows_name_the_live_prefix() {
        // The literal-prefix row is built from `prefix()`, so it cannot keep
        // advertising `C-b C-b` after the prefix moves. Asserted against the
        // default here (`install` is process-global and one-shot, so a test must
        // not take it); the construction is what stops the drift.
        let rows = meta_rows();
        let literal = rows
            .iter()
            .find(|(_, label, _)| label.starts_with("literal"))
            .expect("the literal-prefix row");
        let p = key_disp(prefix());
        assert_eq!(literal.0, format!("{p} {p}"));
        assert_eq!(literal.1, format!("literal {p}"));
    }

    #[test]
    fn rebinding_moves_the_chord_and_its_help_together() {
        // The key-table parity rule, carried onto rebinds: whatever the modal
        // prints is what the dispatcher runs. Applied to a LOCAL table copy -
        // `install` is process-global and one-shot, so a test must not take it.
        let (map, _) = resolve_keymap(None, &[("detach".into(), "C-q".into())]);
        let mut rows = default_bindings();
        for (action, byte) in &map.rebinds {
            if let Some(kb) = rows.iter_mut().find(|kb| kb.action == action) {
                kb.key = *byte;
                kb.disp = key_disp(*byte);
            }
        }
        let detach = rows.iter().find(|kb| kb.action == "detach").unwrap();
        assert_eq!(detach.key, 0x11);
        assert_eq!(detach.disp, "C-q", "the key table prints the NEW key");
        assert_eq!(detach.event, Event::Detach);
    }

    #[test]
    fn every_binding_has_a_unique_stable_action_id() {
        // The config surface: `config.mux.keys.<action>`. A duplicate id would
        // make one of the two unrebindable, and a stray uppercase or space would
        // make the id undiscoverable from the help text.
        let mut seen = std::collections::HashSet::new();
        for kb in default_bindings() {
            assert!(
                seen.insert(kb.action),
                "duplicate action id {:?}",
                kb.action
            );
            assert!(
                !kb.action.is_empty()
                    && kb
                        .action
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c == '-'),
                "action id {:?} is not kebab-case",
                kb.action
            );
        }
    }

    #[test]
    fn client_keys_prefix_unmapped_swallows_with_bell() {
        // The 'q' must NOT be forwarded - swallow + BEL.
        assert_eq!(scan_all(&[b"\x02q"]), vec![Event::Bell]);

        // (x-3e17, AC2-INV) The never-leak guarantee, swept over the whole byte
        // space rather than one specimen. This design deliberately adds NO
        // scanner chord and NO held-byte state - the discoverability fix is
        // four label strings - and this is what pins that: if a later change
        // starts accumulating bytes after the prefix, some byte in this sweep
        // stops being a lone Bell and the assertion names it.
        //
        // ESC is excluded because it legitimately opens a multi-byte arrow scan
        // (Partial, not Bell); the digits and the prefix are structural
        // specials with their own rows.
        let bound: std::collections::HashSet<u8> =
            key_bindings().into_iter().map(|kb| kb.key).collect();
        for b in 0u8..=255 {
            if b == prefix() || b == 0x1b || (b'1'..=b'9').contains(&b) || bound.contains(&b) {
                continue;
            }
            assert_eq!(
                scan_all(&[&[prefix(), b]]),
                vec![Event::Bell],
                "prefix + unmapped {b:#04x} must swallow with one BEL and leak nothing"
            );
        }
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
            // The digit range and the prefix are structural specials, never rows.
            assert!(
                !(b'1'..=b'9').contains(&kb.key) && kb.key != prefix(),
                "structural special {:?} must not appear in key_bindings()",
                kb.key as char
            );
        }
    }

    #[test]
    fn menu_accelerators_round_trip_and_stay_out_of_the_prefix_scope() {
        // x-91a1 AC1: every menu binding resolves to a glyph and back to the
        // same byte/action pair, the prefix table keeps its own meanings
        // untouched (menu `x` did not rebind prefix+x), and a prefix-only
        // action has no menu key to advertise.
        for mb in MENU_BINDINGS {
            assert_eq!(
                menu_key_for(mb.action),
                Some(key_disp(mb.key)),
                "{}",
                mb.action
            );
            assert_eq!(menu_byte_for(mb.action), Some(mb.key), "{}", mb.action);
        }
        // The tab verbs read as this app in both scopes (menu n and angle
        // brackets beside the prefix chords); Diff stays prefix-only - a
        // row-menu action with no settled menu meaning renders no key.
        assert!(
            menu_key_for("diff-pane").is_none(),
            "prefix-only: no menu key"
        );
        let rows = key_bindings();
        let byte_of = |action: &str| {
            rows.iter()
                .find(|kb| kb.action == action)
                .map(|kb| kb.key)
                .unwrap_or_else(|| panic!("prefix table lost {action}"))
        };
        assert_eq!(byte_of("close-pane"), b'x', "prefix+x still kills a pane");
        assert_eq!(byte_of("new-tab"), b'c', "prefix+c still opens a tab");
        assert_eq!(byte_of("rename-tab"), b',');
        assert_eq!(byte_of("close-tab"), b'&');
        assert_eq!(byte_of("move-tab-left"), b'<');
        assert_eq!(byte_of("move-tab-right"), b'>');
        // The two scopes reuse `x` on purpose: menu close/remove can never
        // co-occur with the prefix layer, which reads a byte only after the
        // prefix byte.
        assert!(MENU_BINDINGS.iter().any(|mb| mb.key == b'x'));
    }

    #[test]
    fn client_keys_arrows_and_ctrl_arrows_split_across_reads() {
        // A prefix+arrow chord arriving one byte per read still lands.
        assert_eq!(
            scan_all(&[b"\x02", b"\x1b", b"[", b"C"]),
            vec![Event::Cmd(Command::FocusDir(Dir::Right))]
        );
        // Ctrl-Up = resize up, split at an awkward boundary.
        assert_eq!(
            scan_all(&[b"\x02\x1b[1;", b"5A"]),
            vec![Event::Cmd(Command::ResizeDir(Dir::Up))]
        );
        // A non-arrow escape after prefix is swallowed as one Bell.
        assert_eq!(scan_all(&[b"\x02\x1b[Z"]), vec![Event::Bell]);
    }

    #[test]
    fn esc_released_by_the_prefix_byte_still_enters_prefix() {
        // F2 on PR 1194: Esc followed by the prefix key must open the selector,
        // not forward a literal prefix byte whose argument leaks into the pane.
        // The released candidate's LAST byte re-dispatches through the Normal
        // arms, so the pre-chord behavior is exact.
        assert_eq!(
            scan_all(&[b"\x1b", b"\x02", b"w"]),
            vec![Event::Forward(b"\x1b".to_vec()), Event::OpenSelector]
        );
        assert_eq!(
            scan_all(&[b"\x1b\x02%"]),
            vec![
                Event::Forward(b"\x1b".to_vec()),
                Event::Cmd(Command::SplitH)
            ]
        );
        // A near-miss CSI ahead of the prefix: same re-dispatch of the tail.
        assert_eq!(
            scan_all(&[b"\x1b[1;3\x02%"]),
            vec![
                Event::Forward(b"\x1b[1;3".to_vec()),
                Event::Cmd(Command::SplitH)
            ]
        );
    }

    #[test]
    fn flush_chord_releases_a_quiet_candidate_and_keeps_scanning() {
        // F1 on PR 1194: a lone Esc held for the global chord must not wait
        // for the next keypress forever (vim/fzf Esc-to-cancel stalled). After
        // the client's quiet window, flush_chord forwards the held bytes and
        // the scanner stays whole.
        let now = Instant::now();
        let mut s = Scanner::default();
        assert_eq!(
            s.scan(b"\x1b", now),
            Vec::<Event>::new(),
            "held, not forwarded yet"
        );
        assert!(s.chord_pending());
        assert_eq!(s.flush_chord(), Some(Event::Forward(b"\x1b".to_vec())));
        assert!(!s.chord_pending());
        assert_eq!(s.scan(b"\x02%", now), vec![Event::Cmd(Command::SplitH)]);
        assert_eq!(s.flush_chord(), None, "nothing held: a no-op");
        // A partial multi-byte candidate flushes the same way.
        let mut s = Scanner::default();
        assert_eq!(s.scan(b"\x1b[1;", now), Vec::<Event>::new());
        assert_eq!(s.flush_chord(), Some(Event::Forward(b"\x1b[1;".to_vec())));
    }

    #[test]
    fn client_keys_global_ctrl_opt_left_opens_selector_consumed() {
        // x-e10f AC12: ESC[1;7D with NO prefix fires OpenSelector and the
        // bytes are consumed, never forwarded - the accepted cost of a global
        // grab (a pane program binding Ctrl+Opt+Arrow loses it). Surrounding
        // typing still forwards, in order, on both sides of the chord.
        assert_eq!(scan_all(&[b"\x1b[1;7D"]), vec![Event::OpenSelector]);
        assert_eq!(
            scan_all(&[b"ls\r\x1b[1;7Dmore"]),
            vec![
                Event::Forward(b"ls\r".to_vec()),
                Event::OpenSelector,
                Event::Forward(b"more".to_vec()),
            ]
        );
        // Split across reads at an awkward boundary, like every other chord.
        assert_eq!(scan_all(&[b"\x1b[1;", b"7D"]), vec![Event::OpenSelector]);
        // The prefix path reaches the same rung: prefix + Ctrl+Opt+Left is
        // the same event through the same parse.
        assert_eq!(scan_all(&[b"\x02\x1b[1;7D"]), vec![Event::OpenSelector]);
        // The UNBOUND Ctrl+Opt arrows are not ours: they forward, as before.
        assert_eq!(
            forwarded_only(&scan_all(&[b"\x1b[1;7C"])),
            b"\x1b[1;7C".to_vec()
        );
    }

    #[test]
    fn client_keys_global_chord_hold_releases_non_chord_escapes() {
        // x-e10f: the hold is invisible to everything that is not the chord -
        // bare arrows, Opt+arrow word motion, Alt+x, a lone Esc answered by a
        // later key, and whole pastes all forward byte-exact, paste mode
        // still engages under the hold, and the chord fires after a
        // released near-miss.
        assert_eq!(forwarded_only(&scan_all(&[b"\x1b[C"])), b"\x1b[C".to_vec());
        assert_eq!(
            forwarded_only(&scan_all(&[b"\x1b[1;3D"])),
            b"\x1b[1;3D".to_vec(),
            "Opt+Left word motion is modifier 3, not 7"
        );
        assert_eq!(forwarded_only(&scan_all(&[b"\x1bx"])), b"\x1bx".to_vec());
        assert_eq!(
            forwarded_only(&scan_all(&[b"\x1b", b"j"])),
            b"\x1bj".to_vec(),
            "a lone Esc releases when the next byte says not-the-chord"
        );
        let mut paste = Vec::new();
        paste.extend_from_slice(PASTE_OPEN);
        paste.extend_from_slice(b"pasted");
        paste.extend_from_slice(PASTE_CLOSE);
        let events = scan_all(&[&paste]);
        assert_eq!(forwarded_only(&events), paste);
        assert_eq!(
            scan_all(&[b"\x1b[1;3D\x1b[1;7D"]),
            vec![Event::Forward(b"\x1b[1;3D".to_vec()), Event::OpenSelector,]
        );
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
    fn client_keys_paste_passes_prefix_and_ctrl_backslash_verbatim() {
        // AC5-HP: everything between the markers - prefix bytes, 0x1C -
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
        input.extend_from_slice(b"\x02"); // prefix inside the paste
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
    fn client_keys_paste_open_during_pending_prefix_bells_then_pastes() {
        // AC5-EDGE: prefix pressed, then a paste-open arrives - the dangling
        // chord dies with one BEL, the marker forwards, paste mode engages
        // (the prefix byte inside the paste is inert).
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
    fn client_keys_unterminated_paste_keeps_forwarding_prefix_inert() {
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
            "prefix stays inert until 201~ or reconnect"
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
    fn repeat_window_holds_resize_without_prefix() {
        // AC1-HP: prefix+L arms the window; bare L keeps resizing, each repeat
        // extending it. One prefix chord + N bare keys -> N+1 Resize events.
        let mut s = Scanner::default();
        let t0 = Instant::now();
        assert_eq!(
            s.scan(b"\x02L", t0),
            vec![RESIZE_R],
            "prefix+L resizes + arms"
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
        // AC5-FR: Esc is the explicit hatch - it disarms the window and no
        // resize fires from it. Since x-e10f a lone ESC is also the first
        // byte of the global chord, so it is HELD until the next byte says
        // it is not the chord (ChordEsc); it then forwards with that byte,
        // byte-exact. The disarm itself is unchanged and immediate.
        let mut s = Scanner::default();
        let t0 = Instant::now();
        s.scan(b"\x02K", t0);
        assert_eq!(
            s.scan(b"\x1b", t0 + Duration::from_millis(50)),
            Vec::<Event>::new(),
            "the lone ESC is held for chord disambiguation, not swallowed"
        );
        assert_eq!(
            s.scan(b"K", t0 + Duration::from_millis(80)),
            vec![Event::Forward(b"\x1bK".to_vec())],
            "disarmed by Esc: the released ESC and the bare K forward, no resize"
        );
    }

    #[test]
    fn repeat_window_prefix_disarms_then_chords_normally() {
        // Invariant: prefix inside the window disarms first, then the chord runs
        // as usual - a prefix+resize re-arms; a prefix+other does not.
        let mut s = Scanner::default();
        let t0 = Instant::now();
        s.scan(b"\x02L", t0); // arm
        assert_eq!(
            s.scan(b"\x02%", t0 + Duration::from_millis(100)),
            vec![Event::Cmd(Command::SplitH)],
            "prefix+% still splits inside the window"
        );
        // prefix+% is not a resize, so the window is now closed: bare L forwards.
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
            "prefix+Ctrl-Right resizes right"
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
        // A focus chord (prefix+l) must NOT arm a resize window.
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

    #[test]
    fn pane_id_chord_is_rebindable_and_repeats_without_prefix() {
        assert_eq!(format!("{:?}", resolve_chord(b'\\')), "ShowPaneIds");

        let (map, warnings) = resolve_keymap(None, &[("show-pane-ids".into(), ";".into())]);
        assert!(
            warnings.is_empty(),
            "rebind should be accepted: {warnings:?}"
        );
        let t0 = Instant::now();
        let mut scanner = Scanner::with_keymap(map);
        assert_eq!(format!("{:?}", scanner.scan(b"\x02\\", t0)), "[Bell]");
        assert_eq!(format!("{:?}", scanner.scan(b"\x02;", t0)), "[ShowPaneIds]");
        assert_eq!(
            format!("{:?}", scanner.scan(b";", t0 + Duration::from_millis(40))),
            "[ShowPaneIds]"
        );
        assert_eq!(
            format!("{:?}", scanner.scan(b";", t0 + Duration::from_millis(791))),
            "[Forward([59])]"
        );
    }

    #[test]
    fn pane_id_chord_does_not_fire_inside_bracketed_paste() {
        let mut input = PASTE_OPEN.to_vec();
        input.extend_from_slice(b"\\");
        input.extend_from_slice(PASTE_CLOSE);
        let mut scanner = Scanner::default();
        assert_eq!(forwarded_only(&scanner.scan(&input, Instant::now())), input);
    }
}
