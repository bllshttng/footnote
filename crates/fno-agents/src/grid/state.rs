//! Compositor state machines (ab-3c063856, Waves 3.1 + 3.2 + 4.1).
//!
//! Two interacting machines, both pure and unit-testable so the run loop
//! can wire them to tokio + tokio-tungstenite + crossterm without re-deriving
//! any rules. Owned-PTY model (x-1356): footnote owns every panel's PTY, so
//! there is no watch/drive split and no exclusive-driver claim - focus IS the
//! keystroke target.
//!
//! 1. **Per-pane connection state** ([`ConnState`]) - `connecting → live →
//!    {disconnected, exited}`. Each [`ConnEvent`] yields a [`ConnAction`] the
//!    run loop must execute (open the owned-PTY stream, feed the renderer,
//!    reconnect). The "distinct `connecting` vs blank `live`" silent-failure
//!    invariant (AC1-FR) lives here; there is nothing to detach on exit.
//!
//! 2. **Global mode + focus** ([`Compositor`]) - `Mode = DRIVE | SCROLLBACK`,
//!    focused pane index. The resting mode is `Drive`: a bare keystroke
//!    forwards to the focused pane's owned PTY, and mux commands (focus / page
//!    / quit / scrollback entry) arrive through the leader key. `Scrollback`
//!    is the one surviving modal island (the pane freezes and the nav keys
//!    page its history).
//!
//! Both machines hold no I/O handles - those live in the run loop's struct.
//! The FSM API is `fn step(&mut self, event) -> Action` so the run loop can
//! write a clean `match` dispatch.

/// Per-pane connection state.
///
/// footnote OWNS every panel's PTY (the herdr model), so there is no
/// "watch vs drive" split and no exclusive-driver claim: a live pane is
/// always drivable because the multiplexer is its sole writer. The
/// pre-owned-PTY states (`PromotePending` / `Watching` / `Driving` /
/// `BusyElsewhere`) collapsed into a single [`Live`] when watch mode was
/// removed (x-1356). The renderer's placeholders (`connecting…`,
/// `disconnected - r to retry`, `exited (code N)`) are still derived from
/// this state, so a never-arriving stream stays visible (AC1-FR).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    /// Stream open, no bytes received yet. Distinct from [`Live`] so a
    /// never-arriving stream is visible. An idle owned pane that has
    /// connected but emitted nothing stays here and is still drivable -
    /// the moment you type, the bytes reach its PTY.
    Connecting,
    /// Live: receiving PTY bytes and drivable. The multiplexer reads the
    /// owned PTY master to render the tile and writes raw bytes to it to
    /// drive - one connection, no take-over, no claim.
    Live,
    /// Stream dropped or never established. The renderer offers `r` to
    /// retry.
    Disconnected { reason: String },
    /// Agent process exited. Terminal for this run; the renderer paints
    /// the last received frame plus `exited (code N)`.
    Exited { code: i32 },
}

impl ConnState {
    /// Whether keystrokes from the focused pane reach this agent. Every
    /// live or still-connecting owned pane routes input - footnote is the
    /// sole writer, so there is no read-only state; only a dead or
    /// disconnected pane eats keystrokes. (The run loop's per-pane sink
    /// presence is the real transport guard.)
    pub fn can_route_input(&self) -> bool {
        matches!(self, ConnState::Live | ConnState::Connecting)
    }

    /// Whether the operator can focus-and-drive this pane. Identical to
    /// [`can_route_input`](Self::can_route_input) now that focus == drive
    /// (no separate promote/take-over step): a connected owned pane is
    /// always drivable, a dead one is not.
    pub fn is_drivable(&self) -> bool {
        matches!(self, ConnState::Live | ConnState::Connecting)
    }

    /// Whether the attention scanner should run a readiness check on this
    /// pane. Only `Live` panes count: an `Exited` / `Disconnected` pane
    /// holds a frozen last frame whose tail may end in a prompt glyph,
    /// which would false-positive a "waiting" badge (Invariant: count only
    /// live panes). `Connecting` is excluded - a waiting agent has drawn
    /// its prompt and so has emitted bytes, landing it in `Live`.
    pub fn is_scannable(&self) -> bool {
        matches!(self, ConnState::Live)
    }

    /// One-line label for the renderer's pane chrome.
    pub fn label(&self) -> String {
        match self {
            ConnState::Connecting => "connecting…".to_string(),
            ConnState::Live => "live".to_string(),
            ConnState::Disconnected { reason } => format!("disconnected - {reason} (r to retry)"),
            ConnState::Exited { code } => format!("exited (code {code})"),
        }
    }
}

/// Events the run loop feeds into a pane's connection FSM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnEvent {
    /// The pane's owned-PTY stream finished its initial handshake.
    WatcherConnected,
    /// PTY bytes arrived from the owned-PTY stream.
    BytesReceived(Vec<u8>),
    /// The stream dropped - the operator quit, the daemon closed the
    /// connection, or the network went away.
    WsClosed { reason: String },
    /// The agent's child process exited.
    AgentExited { code: i32 },
    /// The operator pressed `r` on a disconnected pane.
    RetryRequested,
}

/// Side-effect the run loop must perform after a transition. The FSM
/// itself is pure; actions describe the stream / render work the run loop
/// owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnAction {
    /// Open the owned-PTY stream for this pane.
    OpenWatcher,
    /// Feed these bytes into the pane's render parser.
    FeedRenderer(Vec<u8>),
    /// Re-attempt the stream connection.
    Reconnect,
    /// No external work required.
    NoOp,
}

impl ConnState {
    /// Step the FSM. Returns the action the run loop must perform.
    ///
    /// Owned-PTY model (x-1356): there is no promote/claim/release
    /// lifecycle - footnote owns the PTY, so a pane is `Connecting` until
    /// its first bytes flip it `Live`, and any exit path (`WsClosed` /
    /// `AgentExited`) lands in `Disconnected` / `Exited` with nothing to
    /// detach. The renderer keeps the last frame on a dead pane.
    pub fn step(&mut self, event: ConnEvent) -> ConnAction {
        use ConnEvent::*;
        use ConnState::*;

        // ---- Fast path: BytesReceived ----
        //
        // The high-frequency event (one per PTY chunk). Handled first to
        // avoid the `self.clone()` of the catch-all match below
        // (`Disconnected` carries a `String`, so a clone-per-byte would
        // copy it on every chunk).
        if let BytesReceived(bytes) = event {
            return match self {
                Connecting => {
                    *self = Live;
                    ConnAction::FeedRenderer(bytes)
                }
                Live => ConnAction::FeedRenderer(bytes),
                // Bytes after exit / disconnect are dropped (buffered-frame race).
                Exited { .. } | Disconnected { .. } => ConnAction::NoOp,
            };
        }

        // ---- Low-frequency events (stream transitions, retry) ----
        let next: ConnState;
        let action: ConnAction;

        match (self.clone(), event) {
            // BytesReceived already handled in the fast path above.
            (_, BytesReceived(_)) => unreachable!(),

            // Handshake completing before any bytes: stay Connecting until
            // the first byte flips us Live (keeps a never-arriving stream
            // visible). A late handshake from a previous cycle is a no-op.
            (Connecting, WatcherConnected) => {
                next = Connecting;
                action = ConnAction::NoOp;
            }
            (_, WatcherConnected) => {
                next = self.clone();
                action = ConnAction::NoOp;
            }

            // ---- stream drop ----
            (_, WsClosed { reason }) => {
                next = Disconnected { reason };
                action = ConnAction::NoOp;
            }

            // ---- agent exit ----
            (_, AgentExited { code }) => {
                next = Exited { code };
                action = ConnAction::NoOp;
            }

            // ---- retry ----
            (Disconnected { .. }, RetryRequested) => {
                next = Connecting;
                action = ConnAction::Reconnect;
            }
            (_, RetryRequested) => {
                next = self.clone();
                action = ConnAction::NoOp;
            }
        }

        *self = next;
        action
    }
}

/// Global compositor mode.
///
/// Owned-PTY model (x-1356): there is only one mode. footnote owns every
/// panel's PTY, so the focused pane is ALWAYS live-driven - bare keystrokes
/// forward straight to it, exactly like a terminal multiplexer. Multiplexer
/// commands (focus / page / quit) arrive through the leader key
/// ([`super::leader`]), never by stealing a bare key. The former
/// `Watch` (read-only, keystroke-eating) variant was removed with watch mode
/// (x-1356); `Scrollback` survives as the one remaining modal state (entered
/// via a leader combo, not a bare key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The focused pane is live-driven; stdin forwards to its owned PTY.
    Drive,
    /// The focused pane is frozen and the operator is paging its captured
    /// history (modal "scrollback mode"). Entered from `Drive` via a leader
    /// combo; exited with `Esc`, which snaps the pane back to its live tail.
    /// Paging / focus / keystrokes are inert while scrolling - the operator is
    /// pinned to the entry pane (Locked Decision 5).
    Scrollback,
}

/// A scroll command the run loop applies to the focused pane's terminal.
/// Kept renderer-agnostic so [`state`](crate::grid::state) does not depend on
/// `alacritty_terminal::grid::Scroll`; the run loop translates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollCmd {
    /// Up one line (older).
    LineUp,
    /// Down one line (newer).
    LineDown,
    /// Up one page.
    PageUp,
    /// Down one page.
    PageDown,
    /// Jump to the oldest retained line.
    Top,
    /// Jump to the live tail (display offset 0).
    Bottom,
}

/// Compositor-level events fed by the input handler.
///
/// Owned-PTY model (x-1356): a bare keystroke is always a [`Keystroke`] for
/// the focused pane. The focus/page/quit events are emitted ONLY for keys
/// the input handler resolved behind the leader; they never come from a bare
/// key (that would steal it from the agent). The former promote / release
/// events were removed with watch mode; the scrollback events survive, now
/// reached through the leader rather than a bare `Space`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// Move focus to the next pane (leader command).
    FocusNext,
    /// Move focus to the previous pane (leader command).
    FocusPrev,
    /// Quit the compositor (leader command).
    Quit,
    /// Flip to the next page (leader command). Clamps at the last page (no
    /// wrap); relocates focus to the new page's first pane so the focused
    /// index stays in the visible slice (fu-grid-pagination, Invariant).
    PageNext,
    /// Flip to the previous page (leader command). Clamps at page 0.
    PagePrev,
    /// A keystroke from the operator, forwarded to the focused pane's owned
    /// PTY (the default for every bare key).
    Keystroke(Vec<u8>),
    /// Enter scrollback on the focused pane (leader command). Freezes the
    /// pane and pins focus until exit.
    EnterScrollback,
    /// Leave scrollback; snaps the pane back to its live tail.
    ExitScrollback,
    /// Scroll the frozen pane up one line (older).
    ScrollLineUp,
    /// Scroll the frozen pane down one line (newer).
    ScrollLineDown,
    /// Scroll the frozen pane up one page.
    ScrollPageUp,
    /// Scroll the frozen pane down one page.
    ScrollPageDown,
    /// Jump to the oldest retained line.
    ScrollTop,
    /// Jump to the live tail (display offset 0).
    ScrollBottom,
}

/// Action the run loop must perform after a compositor-level event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositorAction {
    /// Nothing to do.
    NoOp,
    /// Forward bytes to pane `pane_idx`'s owned-PTY sink.
    ForwardKeystrokes { pane_idx: usize, bytes: Vec<u8> },
    /// Scroll pane `pane_idx`'s captured history (scrollback mode).
    Scroll { pane_idx: usize, cmd: ScrollCmd },
    /// Quit the run loop.
    Quit,
}

/// The compositor-level state machine. Holds the global mode and the
/// focused pane index; reads per-pane drivability through the borrowed
/// connection states the run loop passes in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compositor {
    mode: Mode,
    focus: usize,
    /// Pane count snapshot. The run loop updates this when a pane is
    /// added or removed; `step` clamps focus on shrink (AC3-FR).
    pane_count: usize,
    /// Page capacity `C` (max panes per page at min tile size), fed from the
    /// layout on every [`recompute_pagination`]. Defaults to `pane_count`
    /// (single page) until the first layout lands (fu-grid-pagination).
    capacity: usize,
    /// 0-indexed current page. Load-bearing invariant:
    /// `0 <= current_page < page_count()` after EVERY mutation path. All
    /// clamping is centralized in [`recompute_pagination`] /
    /// [`set_pane_count`] (Domain Pitfall: do not scatter clamps).
    current_page: usize,
}

impl Compositor {
    /// `pane_count == 0` is legal: the zero-config front door (E5b) starts the
    /// grid with no panes and tiles them live as goals spawn workers. Every
    /// count-sensitive path (`recompute_pagination` / `set_pane_count` /
    /// `set_focus` / `page_count`) already guards the empty case, so the only
    /// thing `new` must do is not assume a first pane exists.
    pub fn new(pane_count: usize) -> Self {
        Compositor {
            mode: Mode::Drive,
            focus: 0,
            pane_count,
            // Single page until the run loop's first recompute_pagination.
            // max(1) keeps capacity a valid divisor even with zero panes.
            capacity: pane_count.max(1),
            current_page: 0,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn focus(&self) -> usize {
        self.focus
    }

    pub fn pane_count(&self) -> usize {
        self.pane_count
    }

    /// Page capacity `C` currently in effect.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 0-indexed page currently in view.
    pub fn current_page(&self) -> usize {
        self.current_page
    }

    /// Total page count for the current `(pane_count, capacity)`. Single-
    /// sourced through [`layout::page_count_for`](crate::grid::layout::page_count_for)
    /// so it can never disagree with the renderer's page math.
    pub fn page_count(&self) -> usize {
        crate::grid::layout::page_count_for(self.pane_count, self.capacity)
    }

    /// Recompute pagination after any change to capacity or pane count
    /// (resize, pane add/remove). THE centralized clamp + anchor path
    /// (Domain Pitfall: centralize, do not scatter clamps). Sets the new
    /// capacity, clamps focus into `[0, pane_count - 1]`, then anchors
    /// `current_page` on the focused agent (`focus / capacity`) so a re-tile
    /// keeps the focused agent visible (AC4-HP / AC1-FR). Guarantees
    /// `0 <= current_page < page_count()`.
    pub fn recompute_pagination(&mut self, capacity: usize) {
        self.capacity = capacity.max(1);
        if self.pane_count == 0 {
            self.focus = 0;
            self.current_page = 0;
            return;
        }
        if self.focus >= self.pane_count {
            self.focus = self.pane_count - 1;
        }
        // Anchor: show whichever page now holds the focused agent.
        self.current_page = self.focus / self.capacity;
        // Defensive clamp; `focus < pane_count` already implies this.
        // saturating_sub: page_count() guarantees >= 1, but stay underflow-safe.
        let last = self.page_count().saturating_sub(1);
        if self.current_page > last {
            self.current_page = last;
        }
    }

    /// Re-sync the compositor against the per-pane states after a per-pane
    /// FSM transition (pane added / removed / exited).
    ///
    /// Owned-PTY model (x-1356): there is no mode to re-sync - the focused
    /// pane is always live-driven and a bare keystroke always forwards to it.
    /// This now only keeps `pane_count`/focus in range and signals quit when
    /// the fleet empties. (Pre-rip-out this also snapped a stale DRIVE back to
    /// WATCH when a claim dropped; with no claim and no WATCH that is moot.)
    ///
    /// Returns `CompositorAction::NoOp` in the common case;
    /// `CompositorAction::Quit` if pane_count has dropped to zero.
    pub fn observe_pane_states(&mut self, pane_states: &[ConnState]) -> CompositorAction {
        if pane_states.len() != self.pane_count {
            self.set_pane_count(pane_states.len());
        }
        if self.pane_count == 0 {
            return CompositorAction::Quit;
        }
        CompositorAction::NoOp
    }

    /// Update the pane count (called by the run loop when a pane is
    /// added or removed). Clamps focus to a valid index when the count
    /// shrinks below it (AC3-FR).
    pub fn set_pane_count(&mut self, n: usize) {
        self.pane_count = n;
        if n == 0 {
            // The run loop should exit when this hits zero; clamp so
            // any inflight access doesn't panic.
            self.focus = 0;
            self.current_page = 0;
            return;
        }
        if self.focus >= n {
            self.focus = n - 1;
        }
        // Keep current_page in range against the new pane_count (capacity is
        // unchanged here; a resize that changes capacity routes through
        // recompute_pagination). Same invariant: 0 <= current_page < page_count.
        // saturating_sub: page_count() guarantees >= 1, but stay underflow-safe.
        let last = self.page_count().saturating_sub(1);
        if self.current_page > last {
            self.current_page = last;
        }
    }

    /// Point compositor focus at a specific pane index, aligning the
    /// compositor's focus to the rail's selected agent (ab-ecf48467, US2).
    /// The rail tracks its selection as `RailState::selected_agent_idx`, but
    /// the drive machinery (`Promote` / `ForwardKeystrokes`) acts on
    /// `self.focus`; without this setter the rail would promote and forward to
    /// whatever pane the compositor last focused, not the one the operator
    /// selected. Clamps into `[0, pane_count-1]` and anchors `current_page` on
    /// the focused pane so the centralized invariant
    /// (`0 <= current_page < page_count()`) still holds. No-op on an empty fleet.
    pub fn set_focus(&mut self, idx: usize) {
        if self.pane_count == 0 {
            return;
        }
        self.focus = idx.min(self.pane_count - 1);
        self.current_page = self.focus / self.capacity.max(1);
        let last = self.page_count().saturating_sub(1);
        if self.current_page > last {
            self.current_page = last;
        }
    }

    /// Flip to the next page in WATCH. Clamps at the last page (no wrap; only
    /// focus-follow Tab wraps, and only deliberately, per Locked Decision 1
    /// and AC1-ERR). Relocates focus to the new page's first pane so the
    /// focused index points into the visible slice (Invariant). A no-op when
    /// already on the last page.
    fn page_next(&mut self) {
        let last = self.page_count().saturating_sub(1);
        if self.current_page < last {
            self.current_page += 1;
            self.focus = (self.current_page * self.capacity).min(self.pane_count.saturating_sub(1));
        }
    }

    /// Flip to the previous page in WATCH. Clamps at page 0.
    fn page_prev(&mut self) {
        if self.current_page > 0 {
            self.current_page -= 1;
            self.focus = self.current_page * self.capacity;
        }
    }

    /// After a focus move, anchor `current_page` on the now-focused pane
    /// (focus-follow, task 2.2). Because focus cycles through ALL panes
    /// across pages and wraps, tabbing off the last pane wraps focus to the
    /// first drivable pane and `current_page` follows to its page (wrap-to-
    /// page-0 in the common case; Claude's Discretion 3). Page KEYS clamp
    /// instead (page_next/page_prev); only Tab wraps.
    fn follow_focus_page(&mut self) {
        self.current_page = self.focus / self.capacity.max(1);
    }

    /// Step the global FSM. `pane_states` is a borrowed view of the
    /// per-pane FSMs the run loop owns; the compositor consults it to
    /// honor "focus cycles only through drivable panes" (AC3-HP /
    /// AC3-FR) and to refuse a Promote on a non-drivable pane.
    pub fn step(&mut self, event: InputEvent, pane_states: &[ConnState]) -> CompositorAction {
        // Defensive: pane_count must match the slice the run loop hands
        // us. Mismatch means a missed set_pane_count and the renderer
        // is about to over-read; clamp pane_count rather than panic so
        // the run loop merely renders one frame stale.
        if pane_states.len() != self.pane_count {
            self.set_pane_count(pane_states.len());
        }
        if self.pane_count == 0 {
            return CompositorAction::Quit;
        }

        // Owned-PTY model (x-1356): `Drive` is the sole live mode - a bare
        // keystroke always forwards to the focused pane's owned PTY; the
        // focus/page/quit events only ever arrive resolved behind the leader
        // key, so they are mux commands, never stolen bare keys. `Scrollback`
        // is the one surviving modal state (pane frozen, leader-entered).
        match (self.mode, event) {
            (Mode::Drive, InputEvent::FocusNext) => {
                self.focus = next_focus(self.focus, pane_states, /*forward=*/ true);
                self.follow_focus_page();
                CompositorAction::NoOp
            }
            (Mode::Drive, InputEvent::FocusPrev) => {
                self.focus = next_focus(self.focus, pane_states, /*forward=*/ false);
                self.follow_focus_page();
                CompositorAction::NoOp
            }
            (Mode::Drive, InputEvent::PageNext) => {
                self.page_next();
                CompositorAction::NoOp
            }
            (Mode::Drive, InputEvent::PagePrev) => {
                self.page_prev();
                CompositorAction::NoOp
            }
            (Mode::Drive, InputEvent::Quit) => CompositorAction::Quit,
            // Scrollback entry (rebound from bare `Space` to a leader combo, x-1356).
            (Mode::Drive, InputEvent::EnterScrollback) => {
                self.mode = Mode::Scrollback;
                CompositorAction::NoOp
            }
            (Mode::Drive, InputEvent::Keystroke(bytes)) => {
                let idx = self.focus;
                // Every connected owned pane routes input (footnote is the
                // sole writer). A dead/disconnected pane eats the bytes; the
                // run loop's sink-presence check is the real transport guard.
                if idx < pane_states.len() && pane_states[idx].can_route_input() {
                    CompositorAction::ForwardKeystrokes {
                        pane_idx: idx,
                        bytes,
                    }
                } else {
                    CompositorAction::NoOp
                }
            }
            (Mode::Drive, _) => CompositorAction::NoOp,

            // Scrollback: the focused pane is frozen and pinned; focus / paging
            // / keystrokes / re-entry are inert (Locked Decision 5).
            (Mode::Scrollback, InputEvent::ScrollLineUp) => CompositorAction::Scroll {
                pane_idx: self.focus,
                cmd: ScrollCmd::LineUp,
            },
            (Mode::Scrollback, InputEvent::ScrollLineDown) => CompositorAction::Scroll {
                pane_idx: self.focus,
                cmd: ScrollCmd::LineDown,
            },
            (Mode::Scrollback, InputEvent::ScrollPageUp) => CompositorAction::Scroll {
                pane_idx: self.focus,
                cmd: ScrollCmd::PageUp,
            },
            (Mode::Scrollback, InputEvent::ScrollPageDown) => CompositorAction::Scroll {
                pane_idx: self.focus,
                cmd: ScrollCmd::PageDown,
            },
            (Mode::Scrollback, InputEvent::ScrollTop) => CompositorAction::Scroll {
                pane_idx: self.focus,
                cmd: ScrollCmd::Top,
            },
            (Mode::Scrollback, InputEvent::ScrollBottom) => CompositorAction::Scroll {
                pane_idx: self.focus,
                cmd: ScrollCmd::Bottom,
            },
            (Mode::Scrollback, InputEvent::ExitScrollback) => {
                // Exit always snaps the pane to its live tail (Locked Decision
                // 7) so the operator can never be stranded in a frozen view.
                self.mode = Mode::Drive;
                CompositorAction::Scroll {
                    pane_idx: self.focus,
                    cmd: ScrollCmd::Bottom,
                }
            }
            (Mode::Scrollback, InputEvent::Quit) => CompositorAction::Quit,
            (Mode::Scrollback, _) => CompositorAction::NoOp,
        }
    }
}

/// Aggregate per-pane waiting flags into per-page counts for OFF-screen pages
/// (fu-grid-pagination, task 3.2).
///
/// `waiting[i]` is whether global pane `i` is waiting for input (the run loop
/// computes it each tick via [`Pane::is_waiting`](crate::grid::pane::Pane::is_waiting),
/// which already excludes dead panes). Returns `(page, count)` pairs - in
/// ascending page order - for pages OTHER than `current_page` that hold at
/// least one waiting agent. The current page is excluded because its waiting
/// agents are directly visible, so flipping to a flagged page clears its
/// badge (AC2-UI). Recomputed every tick, so a count decrements within one
/// tick of an agent resuming output and the badge clears at zero (AC2-FR).
pub fn off_screen_waiting_by_page(
    waiting: &[bool],
    capacity: usize,
    current_page: usize,
) -> Vec<(usize, usize)> {
    let cap = capacity.max(1);
    let mut counts: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for (i, &w) in waiting.iter().enumerate() {
        if !w {
            continue;
        }
        let page = i / cap;
        if page == current_page {
            continue;
        }
        *counts.entry(page).or_insert(0) += 1;
    }
    counts.into_iter().collect()
}

/// Pick the next drivable focus index in cyclic order. If no pane is
/// drivable, focus stays put (the operator sees no change but no
/// keystroke leaks either).
fn next_focus(current: usize, pane_states: &[ConnState], forward: bool) -> usize {
    let n = pane_states.len();
    if n == 0 {
        return 0;
    }
    let mut idx = current;
    for _ in 0..n {
        idx = if forward {
            (idx + 1) % n
        } else {
            (idx + n - 1) % n
        };
        if pane_states[idx].is_drivable() {
            return idx;
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ConnState transition tests (Wave 3.1) ----

    /// AC1-HP / AC1-FR: a pane that has not yet received bytes shows
    /// "connecting…"; the first byte flips it Live.
    #[test]
    fn connect_then_first_bytes_flip_to_live() {
        let mut s = ConnState::Connecting;
        let a = s.step(ConnEvent::WatcherConnected);
        assert_eq!(a, ConnAction::NoOp);
        assert!(matches!(s, ConnState::Connecting));
        let a = s.step(ConnEvent::BytesReceived(b"hello".to_vec()));
        assert_eq!(a, ConnAction::FeedRenderer(b"hello".to_vec()));
        assert_eq!(s, ConnState::Live);
    }

    /// AC4-HP: agent exit anywhere lands in Exited; bytes from a buffered
    /// frame after exit are dropped.
    #[test]
    fn agent_exit_lands_in_exited_and_drops_bytes() {
        let mut s = ConnState::Live;
        s.step(ConnEvent::AgentExited { code: 42 });
        assert_eq!(s, ConnState::Exited { code: 42 });
        let a = s.step(ConnEvent::BytesReceived(vec![1, 2, 3]));
        assert_eq!(a, ConnAction::NoOp);
        assert_eq!(s, ConnState::Exited { code: 42 });
    }

    /// AC2-FR: a stream drop on a live pane lands in Disconnected - never
    /// linger Live on a dead socket. Owned-PTY: nothing to detach.
    #[test]
    fn ws_drop_on_live_lands_in_disconnected() {
        let mut s = ConnState::Live;
        let a = s.step(ConnEvent::WsClosed {
            reason: "broken pipe".to_string(),
        });
        assert_eq!(a, ConnAction::NoOp);
        assert_eq!(
            s,
            ConnState::Disconnected {
                reason: "broken pipe".to_string()
            }
        );
    }

    /// Retry from Disconnected re-attempts the owned-PTY connection.
    #[test]
    fn retry_from_disconnected_reopens_watcher() {
        let mut s = ConnState::Disconnected {
            reason: "lost".to_string(),
        };
        let a = s.step(ConnEvent::RetryRequested);
        assert_eq!(a, ConnAction::Reconnect);
        assert!(matches!(s, ConnState::Connecting));
    }

    /// Owned-PTY: every live or still-connecting pane routes input (footnote
    /// is the sole writer); a dead / disconnected pane does not.
    #[test]
    fn can_route_input_only_when_live_or_connecting() {
        assert!(ConnState::Connecting.can_route_input());
        assert!(ConnState::Live.can_route_input());
        assert!(!ConnState::Disconnected { reason: "x".into() }.can_route_input());
        assert!(!ConnState::Exited { code: 0 }.can_route_input());
    }

    /// is_drivable mirrors can_route_input now that focus == drive (no
    /// separate promote/claim step).
    #[test]
    fn is_drivable_only_when_live_or_connecting() {
        assert!(ConnState::Connecting.is_drivable());
        assert!(ConnState::Live.is_drivable());
        assert!(!ConnState::Disconnected { reason: "x".into() }.is_drivable());
        assert!(!ConnState::Exited { code: 0 }.is_drivable());
    }

    // ---- Compositor transition tests (Wave 3.2 + 4.1) ----

    fn live_panes(n: usize) -> Vec<ConnState> {
        (0..n).map(|_| ConnState::Live).collect()
    }

    /// AC3-HP: WATCH cycles focus on FocusNext / FocusPrev without
    /// emitting any bytes to any agent.
    #[test]
    fn focus_cycles_without_emitting_bytes() {
        let panes = live_panes(3);
        let mut c = Compositor::new(3);
        assert_eq!(c.mode(), Mode::Drive);
        assert_eq!(c.focus(), 0);
        let a = c.step(InputEvent::FocusNext, &panes);
        assert_eq!(a, CompositorAction::NoOp);
        assert_eq!(c.focus(), 1);
        c.step(InputEvent::FocusNext, &panes);
        assert_eq!(c.focus(), 2);
        c.step(InputEvent::FocusNext, &panes);
        assert_eq!(c.focus(), 0, "focus should wrap forward");
        c.step(InputEvent::FocusPrev, &panes);
        assert_eq!(c.focus(), 2, "focus should wrap backward");
    }

    /// AC3-HP: focus skips undrivable panes when cycling.
    #[test]
    fn focus_skips_undrivable_panes() {
        let panes = vec![
            ConnState::Live,
            ConnState::Exited { code: 0 },
            ConnState::Live,
        ];
        let mut c = Compositor::new(3);
        c.step(InputEvent::FocusNext, &panes);
        assert_eq!(c.focus(), 2, "should skip the exited pane in the middle");
        c.step(InputEvent::FocusNext, &panes);
        assert_eq!(c.focus(), 0, "should wrap to the live first pane");
    }

    /// AC3-HP corner: when no pane is drivable, focus stays put.
    #[test]
    fn no_drivable_panes_focus_stays() {
        let panes = vec![ConnState::Exited { code: 0 }, ConnState::Exited { code: 1 }];
        let mut c = Compositor::new(2);
        c.step(InputEvent::FocusNext, &panes);
        assert_eq!(c.focus(), 0);
    }

    /// AC3-FR: pane removal clamps focus to a valid index.
    #[test]
    fn pane_removal_clamps_focus() {
        let mut c = Compositor::new(3);
        c.step(InputEvent::FocusNext, &live_panes(3));
        c.step(InputEvent::FocusNext, &live_panes(3));
        assert_eq!(c.focus(), 2);
        c.set_pane_count(2);
        assert!(c.focus() < 2, "focus must clamp on shrink");
    }

    /// US2: set_focus aligns the compositor focus to the rail's selected
    /// agent, clamps out-of-range indices, and is a no-op on an empty fleet.
    #[test]
    fn set_focus_aligns_and_clamps() {
        let mut c = Compositor::new(4);
        c.set_focus(2);
        assert_eq!(c.focus(), 2, "focus moves to the selected index");
        c.set_focus(99);
        assert_eq!(c.focus(), 3, "out-of-range index clamps to last pane");
        // Empty fleet: no panic, no change.
        c.set_pane_count(0);
        c.set_focus(1);
        assert_eq!(c.focus(), 0, "set_focus is a no-op on an empty fleet");
    }

    /// Owned-PTY: a bare keystroke forwards to the focused live pane - focus
    /// IS drive, with no promote/claim gate to pass first.
    #[test]
    fn keystrokes_forward_to_focused_live_pane() {
        let mut c = Compositor::new(2);
        let panes = live_panes(2);
        c.step(InputEvent::FocusNext, &panes);
        let a = c.step(InputEvent::Keystroke(b"hello".to_vec()), &panes);
        assert_eq!(
            a,
            CompositorAction::ForwardKeystrokes {
                pane_idx: 1,
                bytes: b"hello".to_vec()
            }
        );
    }

    /// A keystroke to a dead (Exited) focused pane is dropped - the per-pane
    /// gate (`can_route_input`) refuses it.
    #[test]
    fn keystrokes_to_dead_pane_are_dropped() {
        let mut c = Compositor::new(1);
        let panes = vec![ConnState::Exited { code: 0 }];
        let a = c.step(InputEvent::Keystroke(b"x".to_vec()), &panes);
        assert_eq!(a, CompositorAction::NoOp);
    }

    /// Quit yields Quit so the run loop exits cleanly.
    #[test]
    fn quit_yields_quit_action() {
        let mut c = Compositor::new(2);
        let panes = live_panes(2);
        let a = c.step(InputEvent::Quit, &panes);
        assert_eq!(a, CompositorAction::Quit);
    }

    /// Pane count to zero forces Quit so the run loop exits cleanly.
    #[test]
    fn zero_panes_forces_quit() {
        let mut c = Compositor::new(1);
        c.set_pane_count(0);
        let a = c.step(InputEvent::FocusNext, &[]);
        assert_eq!(a, CompositorAction::Quit);
    }

    // ── Pagination state tests (fu-grid-pagination, task 1.2) ────────────

    /// A fresh compositor is single-page (capacity defaults to pane_count).
    #[test]
    fn new_compositor_is_single_page() {
        let c = Compositor::new(5);
        assert_eq!(c.capacity(), 5);
        assert_eq!(c.page_count(), 1);
        assert_eq!(c.current_page(), 0);
    }

    /// E5b zero-config front door: the grid starts with no panes, so the
    /// compositor must construct cleanly at count 0 and keep the
    /// `page_count >= 1` invariant (capacity is divided by `.max(1)`).
    #[test]
    fn new_compositor_allows_zero_panes() {
        let c = Compositor::new(0);
        assert_eq!(c.pane_count(), 0);
        assert_eq!(c.page_count(), 1);
        assert_eq!(c.current_page(), 0);
        assert_eq!(c.focus(), 0);
    }

    /// E5b one-tap orchestration: a goal spawns a worker and the run loop
    /// grows the count live. The compositor must adopt the new panes without a
    /// panic, then accept focus on the freshly added one.
    #[test]
    fn grow_from_zero_focuses_new_pane() {
        let mut c = Compositor::new(0);
        c.set_pane_count(2);
        c.recompute_pagination(2);
        assert_eq!(c.pane_count(), 2);
        assert_eq!(c.page_count(), 1);
        // The run loop focuses the just-added pane (global index 1).
        c.set_focus(1);
        assert_eq!(c.focus(), 1);
        // step() also self-syncs pane_count against the live states slice.
        let panes = live_panes(3);
        c.step(InputEvent::FocusNext, &panes);
        assert_eq!(c.pane_count(), 3);
    }

    /// recompute_pagination derives page_count from (pane_count, capacity).
    #[test]
    fn recompute_sets_capacity_and_page_count() {
        let mut c = Compositor::new(10);
        c.recompute_pagination(4); // C=4, 10 panes → 3 pages
        assert_eq!(c.capacity(), 4);
        assert_eq!(c.page_count(), 3);
    }

    /// AC4-HP / AC1-FR anchor: after a re-tile, current_page shows whichever
    /// page now holds the focused agent. Focus on global pane 7, C=4 → page 1.
    #[test]
    fn recompute_anchors_current_page_on_focused_agent() {
        let mut c = Compositor::new(10);
        // Walk focus to pane 7 (all drivable so FocusNext steps by one).
        let panes = live_panes(10);
        for _ in 0..7 {
            c.step(InputEvent::FocusNext, &panes);
        }
        assert_eq!(c.focus(), 7);
        c.recompute_pagination(4); // C=4 → pane 7 lives on page 7/4 = 1
        assert_eq!(c.current_page(), 1, "anchored on focused agent's page");
        // Re-tile larger so C=6: pane 7 now on page 7/6 = 1 still.
        c.recompute_pagination(6);
        assert_eq!(c.current_page(), 1);
        // Re-tile so C=10 (single page): pane 7 on page 0.
        c.recompute_pagination(10);
        assert_eq!(c.current_page(), 0);
        assert_eq!(c.page_count(), 1);
    }

    /// AC1-FR: a resize that GROWS capacity (page_count shrinks) re-clamps
    /// current_page to a valid page; the focused agent stays visible.
    #[test]
    fn recompute_clamps_current_page_when_capacity_grows() {
        let mut c = Compositor::new(10);
        c.recompute_pagination(4); // 3 pages
                                   // Force-view the last page by focusing a late pane then recomputing.
        let panes = live_panes(10);
        for _ in 0..9 {
            c.step(InputEvent::FocusNext, &panes);
        }
        c.recompute_pagination(4);
        assert_eq!(c.current_page(), 2); // pane 9 / 4
                                         // Grow so C=10 → 1 page; current_page must clamp to 0.
        c.recompute_pagination(10);
        assert!(c.current_page() < c.page_count());
        assert_eq!(c.current_page(), 0);
    }

    /// Load-bearing invariant: 0 <= current_page < page_count() after every
    /// recompute across a sweep of capacities and a sweep of focus positions.
    #[test]
    fn pagination_invariant_holds_across_mutations() {
        for focus_target in 0..12 {
            for cap in 1..=12 {
                let mut c = Compositor::new(12);
                let panes = live_panes(12);
                for _ in 0..focus_target {
                    c.step(InputEvent::FocusNext, &panes);
                }
                c.recompute_pagination(cap);
                assert!(
                    c.current_page() < c.page_count(),
                    "invariant violated: page {} >= count {} (focus={focus_target}, cap={cap})",
                    c.current_page(),
                    c.page_count()
                );
            }
        }
    }

    /// AC5-EDGE seam: set_pane_count keeps current_page valid when the count
    /// shrinks (placeholders are normally retained, but the FSM must stay
    /// coherent if a removal ever lands).
    #[test]
    fn set_pane_count_keeps_current_page_valid() {
        let mut c = Compositor::new(10);
        c.recompute_pagination(4); // 3 pages
        let panes = live_panes(10);
        for _ in 0..9 {
            c.step(InputEvent::FocusNext, &panes);
        }
        c.recompute_pagination(4);
        assert_eq!(c.current_page(), 2);
        // Shrink to 5 panes → 2 pages; current_page must clamp to <= 1.
        c.set_pane_count(5);
        assert!(c.current_page() < c.page_count());
    }

    // ── Navigation tests (fu-grid-pagination, tasks 2.1 + 2.2) ───────────

    /// AC1-HP: `]` (PageNext) in WATCH advances the page and relocates focus
    /// to the new page's first pane (focused index stays in the visible
    /// slice; Invariant).
    #[test]
    fn page_next_advances_and_relocates_focus() {
        let mut c = Compositor::new(10);
        c.recompute_pagination(4); // 3 pages, C=4, focus 0, page 0
        let panes = live_panes(10);
        c.step(InputEvent::PageNext, &panes);
        assert_eq!(c.current_page(), 1);
        assert_eq!(c.focus(), 4, "focus → first pane of page 1");
        c.step(InputEvent::PageNext, &panes);
        assert_eq!(c.current_page(), 2);
        assert_eq!(c.focus(), 8, "focus → first pane of page 2");
    }

    /// AC1-ERR: PageNext at the last page is a clamped no-op (no wrap), and
    /// focus does not move.
    #[test]
    fn page_next_clamps_at_last_page_no_wrap() {
        let mut c = Compositor::new(10);
        c.recompute_pagination(4);
        let panes = live_panes(10);
        c.step(InputEvent::PageNext, &panes); // → page 1
        c.step(InputEvent::PageNext, &panes); // → page 2 (last)
        let focus_before = c.focus();
        c.step(InputEvent::PageNext, &panes); // clamp
        assert_eq!(c.current_page(), 2, "stays on last page");
        assert_eq!(c.focus(), focus_before, "focus unchanged at bound");
    }

    /// PagePrev clamps at page 0 and relocates focus to the new page's first
    /// pane on a real flip.
    #[test]
    fn page_prev_clamps_at_zero_and_relocates() {
        let mut c = Compositor::new(10);
        c.recompute_pagination(4);
        let panes = live_panes(10);
        c.step(InputEvent::PageNext, &panes); // page 1
        c.step(InputEvent::PageNext, &panes); // page 2
        c.step(InputEvent::PagePrev, &panes); // page 1
        assert_eq!(c.current_page(), 1);
        assert_eq!(c.focus(), 4);
        c.step(InputEvent::PagePrev, &panes); // page 0
        assert_eq!(c.current_page(), 0);
        c.step(InputEvent::PagePrev, &panes); // clamp at 0
        assert_eq!(c.current_page(), 0);
    }

    /// AC1-UI: page keys are inert in the single-page case (no pages to flip).
    #[test]
    fn page_keys_inert_single_page() {
        let mut c = Compositor::new(3); // capacity 3, 1 page
        let panes = live_panes(3);
        c.step(InputEvent::PageNext, &panes);
        assert_eq!(c.current_page(), 0);
        c.step(InputEvent::PagePrev, &panes);
        assert_eq!(c.current_page(), 0);
    }

    /// Task 2.2 / AC1-HP focus-follow: tab focus that crosses the current
    /// page boundary auto-advances current_page so the newly-focused pane is
    /// visible.
    #[test]
    fn focus_follow_advances_page_on_boundary_crossing() {
        let mut c = Compositor::new(10);
        c.recompute_pagination(4); // C=4, page 0
        let panes = live_panes(10);
        // Tab within page 0 (focus 1,2,3) stays on page 0.
        c.step(InputEvent::FocusNext, &panes); // focus 1
        assert_eq!(c.current_page(), 0);
        c.step(InputEvent::FocusNext, &panes); // focus 2
        c.step(InputEvent::FocusNext, &panes); // focus 3
        assert_eq!(c.current_page(), 0);
        // Crossing to focus 4 advances to page 1.
        c.step(InputEvent::FocusNext, &panes); // focus 4
        assert_eq!(c.focus(), 4);
        assert_eq!(c.current_page(), 1, "focus-follow crossed the boundary");
    }

    /// Wrap policy (Claude's Discretion 3): tabbing off the last pane wraps
    /// focus to the first drivable pane, and current_page follows to page 0.
    /// Page KEYS clamp; only Tab wraps.
    #[test]
    fn focus_follow_wraps_to_page_zero() {
        let mut c = Compositor::new(6);
        c.recompute_pagination(2); // C=2 → 3 pages
        let panes = live_panes(6);
        for _ in 0..5 {
            c.step(InputEvent::FocusNext, &panes);
        }
        assert_eq!(c.focus(), 5);
        assert_eq!(c.current_page(), 2); // last pane on last page
        c.step(InputEvent::FocusNext, &panes); // wrap
        assert_eq!(c.focus(), 0, "focus wraps to first pane");
        assert_eq!(c.current_page(), 0, "current_page follows to page 0");
    }

    /// FocusPrev follows the page backward across a boundary too.
    #[test]
    fn focus_prev_follows_page_backward() {
        let mut c = Compositor::new(10);
        c.recompute_pagination(4);
        let panes = live_panes(10);
        // Jump to page 2 via page key (focus 8).
        c.step(InputEvent::PageNext, &panes);
        c.step(InputEvent::PageNext, &panes);
        assert_eq!(c.focus(), 8);
        // FocusPrev → focus 7 → page 1.
        c.step(InputEvent::FocusPrev, &panes);
        assert_eq!(c.focus(), 7);
        assert_eq!(c.current_page(), 1);
    }

    // ── Attention aggregation tests (fu-grid-pagination, task 3.2) ───────

    /// AC2-HP: an off-screen waiting agent produces a badge for its page.
    #[test]
    fn off_screen_waiting_flags_the_page() {
        // C=4. Pane 5 (page 1) waiting; viewing page 0.
        let mut waiting = vec![false; 10];
        waiting[5] = true;
        let badges = off_screen_waiting_by_page(&waiting, 4, 0);
        assert_eq!(badges, vec![(1, 1)]);
    }

    /// AC2-UI: the current page is excluded - its waiting agents are visible,
    /// so no badge (flipping to a flagged page clears it).
    #[test]
    fn current_page_waiting_is_not_badged() {
        // C=4. Pane 1 (page 0) waiting; viewing page 0 → no badge.
        let mut waiting = vec![false; 8];
        waiting[1] = true;
        assert!(off_screen_waiting_by_page(&waiting, 4, 0).is_empty());
        // Pane 5 (page 1) also waiting; viewing page 1 → still no badge for
        // page 1, and page 0's pane 1 now badges.
        waiting[5] = true;
        assert_eq!(off_screen_waiting_by_page(&waiting, 4, 1), vec![(0, 1)]);
    }

    /// AC2-EDGE: multiple off-screen pages waiting → per-page counts, ordered.
    #[test]
    fn multiple_off_screen_pages_waiting() {
        // C=4, 12 panes (3 pages), viewing page 0.
        // page 1: panes 4,5 waiting (2). page 2: pane 9 waiting (1).
        let mut waiting = vec![false; 12];
        waiting[4] = true;
        waiting[5] = true;
        waiting[9] = true;
        let badges = off_screen_waiting_by_page(&waiting, 4, 0);
        assert_eq!(badges, vec![(1, 2), (2, 1)]);
    }

    /// AC2-FR: when an agent stops waiting, the recomputed badge set drops it;
    /// at zero the page's badge clears entirely.
    #[test]
    fn waiting_count_decrements_and_clears() {
        let mut waiting = vec![false; 8];
        waiting[4] = true;
        waiting[5] = true; // page 1 has 2 waiting
        assert_eq!(off_screen_waiting_by_page(&waiting, 4, 0), vec![(1, 2)]);
        waiting[5] = false; // one resumes
        assert_eq!(off_screen_waiting_by_page(&waiting, 4, 0), vec![(1, 1)]);
        waiting[4] = false; // both resumed → badge clears
        assert!(off_screen_waiting_by_page(&waiting, 4, 0).is_empty());
    }

    /// No waiting agents anywhere → no badges.
    #[test]
    fn no_waiting_no_badges() {
        let waiting = vec![false; 9];
        assert!(off_screen_waiting_by_page(&waiting, 4, 1).is_empty());
    }

    /// Zero panes collapses pagination state cleanly (quit path).
    #[test]
    fn zero_panes_resets_pagination() {
        let mut c = Compositor::new(4);
        c.recompute_pagination(1); // 4 pages
        c.set_pane_count(0);
        assert_eq!(c.current_page(), 0);
        assert_eq!(c.page_count(), 1);
    }

    /// ConnState labels are non-empty for the renderer chrome.
    #[test]
    fn conn_state_labels_are_nonempty() {
        for s in [
            ConnState::Connecting,
            ConnState::Live,
            ConnState::Disconnected { reason: "x".into() },
            ConnState::Exited { code: 0 },
        ] {
            assert!(!s.label().is_empty(), "state {s:?} has empty label");
        }
    }

    // ── Scrollback mode FSM ──────────────────────────────────────────────

    /// Leader+Space enters scrollback (mode flip, no action); Esc exits back
    /// to driving and snaps the focused pane to its live tail.
    #[test]
    fn enter_scrollback_flips_mode_and_exit_returns_and_snaps() {
        let panes = live_panes(2);
        let mut c = Compositor::new(2);
        // Entry flips Drive -> Scrollback; the pane freezes where it is (the
        // operator scrolls with the nav keys).
        let a = c.step(InputEvent::EnterScrollback, &panes);
        assert_eq!(a, CompositorAction::NoOp);
        assert_eq!(c.mode(), Mode::Scrollback);

        let a = c.step(InputEvent::ExitScrollback, &panes);
        assert_eq!(
            a,
            CompositorAction::Scroll {
                pane_idx: 0,
                cmd: ScrollCmd::Bottom
            },
            "exit snaps to the live tail (Locked Decision 7)"
        );
        assert_eq!(c.mode(), Mode::Drive);
    }

    /// AC1-HP: scroll keys in scrollback emit Scroll actions for the focused pane.
    #[test]
    fn scroll_events_emit_scroll_actions_for_focused_pane() {
        let panes = live_panes(3);
        let mut c = Compositor::new(3);
        c.step(InputEvent::FocusNext, &panes); // focus 1
        c.step(InputEvent::EnterScrollback, &panes);
        for (ev, cmd) in [
            (InputEvent::ScrollLineUp, ScrollCmd::LineUp),
            (InputEvent::ScrollLineDown, ScrollCmd::LineDown),
            (InputEvent::ScrollPageUp, ScrollCmd::PageUp),
            (InputEvent::ScrollPageDown, ScrollCmd::PageDown),
            (InputEvent::ScrollTop, ScrollCmd::Top),
            (InputEvent::ScrollBottom, ScrollCmd::Bottom),
        ] {
            assert_eq!(
                c.step(ev, &panes),
                CompositorAction::Scroll { pane_idx: 1, cmd },
            );
            assert_eq!(
                c.mode(),
                Mode::Scrollback,
                "scroll keys do not leave the mode"
            );
        }
    }

    /// Locked Decision 5: focus / paging / keystroke are inert while scrolling
    /// (the operator is pinned to the entry pane).
    #[test]
    fn inert_keys_in_scrollback_are_noops() {
        let panes = live_panes(3);
        let mut c = Compositor::new(3);
        c.step(InputEvent::EnterScrollback, &panes);
        for ev in [
            InputEvent::FocusNext,
            InputEvent::FocusPrev,
            InputEvent::PageNext,
            InputEvent::PagePrev,
            InputEvent::Keystroke(b"x".to_vec()),
            InputEvent::EnterScrollback, // re-entry is idempotent
        ] {
            assert_eq!(c.step(ev, &panes), CompositorAction::NoOp);
            assert_eq!(c.focus(), 0, "focus is pinned in scrollback");
            assert_eq!(c.mode(), Mode::Scrollback);
        }
    }
}
