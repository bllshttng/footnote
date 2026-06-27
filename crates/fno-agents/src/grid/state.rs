//! Compositor state machines (ab-3c063856, Waves 3.1 + 3.2 + 4.1).
//!
//! Two interacting machines, both pure and unit-testable so the run loop
//! (Wave 5.1) can wire them to tokio + tokio-tungstenite + crossterm
//! without re-deriving any rules:
//!
//! 1. **Per-pane connection state** ([`ConnState`], Wave 3.1) -
//!    `connecting → watching → {driving, busy_elsewhere, disconnected,
//!    exited}`. Each [`ConnEvent`] yields a [`ConnAction`] the run loop
//!    must execute (open a watcher socket, send a drive RPC, send a
//!    detach control frame, log a denial flash, …). The "distinct
//!    `connecting` vs blank `watching`" silent-failure invariant
//!    (AC1-FR) and the "claim released on every exit path" invariant
//!    (Domain Pitfall + AC4-FR) live here.
//!
//! 2. **Global mode + focus** ([`Compositor`], Waves 3.2 + 4.1) -
//!    `Mode = WATCH | DRIVE`, focused pane index. Routes keystrokes:
//!    in WATCH, the compositor consumes them (focus / take-over /
//!    quit); in DRIVE, they forward to the focused pane's driver
//!    socket. Promotion serializes a single `agent.drive` RPC ahead
//!    of any input routing (Concurrency invariant; AC3-DOUBLE).
//!    Demotion fires on Esc, WS-drop, agent-exit, and quit.
//!
//! Both machines hold no I/O handles - those live in the run loop's
//! struct that wraps a `Compositor` and an `HashMap<PaneId, JoinHandle>`
//! or similar. The FSM API is `fn step(&mut self, event) -> Action`
//! so the run loop can write a clean `match` dispatch.

/// Per-pane connection state.
///
/// The placeholders the renderer paints (`connecting…`,
/// `tailing (busy elsewhere)`, `disconnected - r to retry`,
/// `exited (code N)`) are derived from this state, so the renderer can
/// show "connecting" vs "watching" distinctly - closing the silent
/// "never-arriving stream" failure mode (AC1-FR).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    /// Watcher socket open, no bytes received yet. Distinct from
    /// [`Watching`] so a never-arriving stream is visible. An idle agent
    /// that has connected but emitted nothing remains here and is still
    /// drivable on Enter - split from [`PromotePending`] to address the
    /// "quiet agent can't be promoted" review finding (PR #370 Codex P2).
    Connecting,
    /// Operator pressed Enter; the run loop has issued
    /// `agent.drive(mode: "interactive")` and is awaiting the ack. No
    /// further `SendDriveRpc` action fires on a re-press (AC3-DOUBLE).
    PromotePending,
    /// Live watcher receiving PTY bytes. Read-only.
    Watching,
    /// Driver claim held by this pane; input forwarded to its socket.
    Driving,
    /// Take-over denied - an external driver holds the agent. The pane
    /// renders the agent's tail-clipped output with a `busy` label.
    BusyElsewhere { holder: Option<String> },
    /// WS dropped or never established. The renderer offers `r` to
    /// retry. A pane that was [`Driving`] when the WS dropped re-enters
    /// here, NEVER stays in [`Driving`] with a dead socket (AC2-FR).
    Disconnected { reason: String },
    /// Agent process exited. Terminal for this run; the renderer paints
    /// the last received frame plus `exited (code N)`.
    Exited { code: i32 },
}

impl ConnState {
    /// Whether keystrokes from a DRIVE-mode focused pane can reach this
    /// agent. Only `Driving` ever lets bytes through; anything else
    /// (including `Watching`) eats them - the global mode is the second
    /// guard, but a per-pane sanity check closes the door if the global
    /// state were ever inconsistent.
    pub fn can_route_input(&self) -> bool {
        matches!(self, ConnState::Driving)
    }

    /// Whether the operator can attempt to promote this pane via Enter.
    /// A pane that is `Exited`, `Disconnected`, already `Driving`, or
    /// has an in-flight promote (`PromotePending`) cannot be promoted by
    /// a fresh Enter. Critically, [`Connecting`] IS drivable - an idle
    /// agent that has connected but emitted no bytes must still accept
    /// take-over (PR #370 Codex P2).
    pub fn is_drivable(&self) -> bool {
        matches!(
            self,
            ConnState::Connecting | ConnState::Watching | ConnState::BusyElsewhere { .. }
        )
    }

    /// Whether the attention scanner should run a readiness check on this
    /// pane (fu-grid-pagination, task 3.1). Only LIVE panes (`Watching` /
    /// `Driving`) count: an `Exited` / `Disconnected` pane holds a frozen
    /// last frame whose tail may end in a prompt glyph, which would
    /// false-positive a "waiting" badge (Domain Pitfall / Invariant: count
    /// only live panes). `Connecting`/`PromotePending`/`BusyElsewhere` are
    /// also excluded - a waiting agent has drawn its prompt and so has
    /// emitted bytes, landing it in `Watching`.
    pub fn is_scannable(&self) -> bool {
        matches!(self, ConnState::Watching | ConnState::Driving)
    }

    /// One-line label for the renderer's pane chrome.
    pub fn label(&self) -> String {
        match self {
            ConnState::Connecting => "connecting…".to_string(),
            ConnState::PromotePending => "acquiring driver claim…".to_string(),
            ConnState::Watching => "watching".to_string(),
            ConnState::Driving => "driving".to_string(),
            ConnState::BusyElsewhere { holder } => match holder {
                Some(h) => format!("tailing (busy: driven by {h})"),
                None => "tailing (busy elsewhere)".to_string(),
            },
            ConnState::Disconnected { reason } => format!("disconnected - {reason} (r to retry)"),
            ConnState::Exited { code } => format!("exited (code {code})"),
        }
    }
}

/// Events the run loop feeds into a pane's connection FSM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnEvent {
    /// The watcher WS finished its initial handshake successfully.
    WatcherConnected,
    /// PTY bytes arrived from the watcher WS.
    BytesReceived(Vec<u8>),
    /// `agent.drive` RPC with `mode: "interactive"` was acked OK.
    DriveClaimAcquired,
    /// `agent.drive` RPC was rejected because another driver holds the
    /// agent (an `ErrorCode::Busy` from `protocol.rs`).
    DriveClaimDenied { holder: Option<String> },
    /// The operator's release request landed: detach control frame sent
    /// and ack'd.
    DriveClaimReleased,
    /// The WebSocket dropped - either the operator quit, the daemon
    /// closed the connection, or the network went away.
    WsClosed { reason: String },
    /// The agent's child process exited.
    AgentExited { code: i32 },
    /// The operator pressed Enter on this focused pane.
    PromoteRequested,
    /// The operator pressed Esc while in DRIVE mode (only meaningful
    /// for the currently-driving pane).
    ReleaseRequested,
    /// The operator pressed `r` on a disconnected pane.
    RetryRequested,
}

/// Side-effect the run loop must perform after a transition. The FSM
/// itself is pure; actions describe the network / WS / log work the run
/// loop owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnAction {
    /// Open the watcher WS for this pane.
    OpenWatcher,
    /// Feed these bytes into the pane's render parser.
    FeedRenderer(Vec<u8>),
    /// Send `agent.drive` with `mode: "interactive"` (the take-over RPC).
    SendDriveRpc,
    /// Send the `{"t":"detach","reason":"..."}` control frame.
    SendDetach,
    /// Flash a denial in the pane chrome (Enter pressed on undrivable
    /// or busy-elsewhere pane; AC2-ERR).
    FlashDenial(String),
    /// Re-attempt the watcher connection.
    Reconnect,
    /// No external work required.
    NoOp,
}

impl ConnState {
    /// Step the FSM. Returns the action the run loop must perform.
    ///
    /// **Rules pinned by the plan:**
    /// - `WsClosed` while `Driving` → `Disconnected` AND a `SendDetach`
    ///   is NOT issued (the WS is already gone; the daemon's per-
    ///   session watchdog will release the claim on heartbeat timeout
    ///   per `drive.rs`'s `HEARTBEAT_TIMEOUT`). The release-on-every-
    ///   exit-path invariant (AC4-FR / Concurrency) means the FSM
    ///   never lingers in `Driving` on a dead socket.
    /// - `AgentExited` while `Driving` → `Exited`. The claim is gone
    ///   with the agent.
    /// - `PromoteRequested` on `Watching` → `Connecting` (the request
    ///   is async; the run loop will fire `SendDriveRpc` and we wait
    ///   for `DriveClaimAcquired` or `DriveClaimDenied`). Double-
    ///   Enter inside this window is a no-op (AC3-DOUBLE).
    /// - `DriveClaimDenied` while `Connecting` (post-promote) →
    ///   `BusyElsewhere` (NOT back to `Watching` - the operator
    ///   needs to see a denial flash; the renderer reads
    ///   `BusyElsewhere` for the label).
    pub fn step(&mut self, event: ConnEvent) -> ConnAction {
        use ConnEvent::*;
        use ConnState::*;

        // ---- Fast path: BytesReceived ----
        //
        // BytesReceived is the high-frequency event (one per PTY chunk
        // on the watcher WS). Handling it here avoids the heap-alloc-
        // prone `self.clone()` of the catch-all match below: `ConnState`
        // carries `String`s in `BusyElsewhere` and `Disconnected`, so a
        // clone-per-byte would copy a String per PTY chunk on every
        // pane. Caught by gemini-code-assist on PR #370.
        if let BytesReceived(bytes) = event {
            return match self {
                Connecting => {
                    *self = Watching;
                    ConnAction::FeedRenderer(bytes)
                }
                Watching | Driving | BusyElsewhere { .. } | PromotePending => {
                    ConnAction::FeedRenderer(bytes)
                }
                // Bytes after exit / disconnect are dropped (cannot
                // arrive on a closed socket but covers the buffered-
                // frame race).
                Exited { .. } | Disconnected { .. } => ConnAction::NoOp,
            };
        }

        // ---- Low-frequency events ----
        //
        // These fire on operator actions (promote / release / quit) and
        // network transitions (ws-closed, agent-exited). The cloned
        // pattern is fine here because the events are infrequent and
        // the match shape is easier to audit than a state-machine table.
        let next: ConnState;
        let action: ConnAction;

        match (self.clone(), event) {
            // BytesReceived already handled in the fast path above.
            (_, BytesReceived(_)) => unreachable!(),

            // ---- watcher establishment ----
            (Connecting, WatcherConnected) => {
                next = Connecting;
                action = ConnAction::NoOp;
            }

            // ---- promote ----
            (Watching, PromoteRequested) | (Connecting, PromoteRequested) => {
                // C3 fix: an idle Connecting pane (no bytes yet, but
                // socket open) IS drivable. The transition mirrors the
                // Watching path: fire the drive RPC, await ack in
                // PromotePending.
                next = PromotePending;
                action = ConnAction::SendDriveRpc;
            }
            (BusyElsewhere { .. }, PromoteRequested) => {
                // Operator may still try - daemon's stale takeover may
                // evict the other driver (drive.rs::STALE_DRIVER_IDLE).
                next = PromotePending;
                action = ConnAction::SendDriveRpc;
            }
            (PromotePending, PromoteRequested) => {
                // AC3-DOUBLE: a second Enter inside the inflight window
                // is a no-op; the first RPC serializes.
                next = PromotePending;
                action = ConnAction::NoOp;
            }
            (Driving, PromoteRequested) => {
                // Already driving; redundant Enter is a no-op.
                next = Driving;
                action = ConnAction::NoOp;
            }
            (Exited { code }, PromoteRequested) => {
                next = Exited { code };
                action = ConnAction::FlashDenial("agent has exited".to_string());
            }
            (Disconnected { reason }, PromoteRequested) => {
                next = Disconnected { reason };
                action = ConnAction::FlashDenial("disconnected".to_string());
            }

            // ---- claim outcomes ----
            (PromotePending, DriveClaimAcquired) => {
                next = Driving;
                action = ConnAction::NoOp;
            }
            (_, DriveClaimAcquired) => {
                // Acquired without a corresponding PromotePending is a
                // logic bug (stale ack); keep the current state.
                next = self.clone();
                action = ConnAction::NoOp;
            }
            (PromotePending, DriveClaimDenied { holder }) => {
                next = BusyElsewhere {
                    holder: holder.clone(),
                };
                action = ConnAction::FlashDenial(match holder {
                    Some(h) => format!("busy: driven by {h}"),
                    None => "busy: another driver holds this agent".to_string(),
                });
            }
            (_, DriveClaimDenied { .. }) => {
                next = self.clone();
                action = ConnAction::NoOp;
            }

            // ---- release ----
            (Driving, ReleaseRequested) => {
                next = Watching;
                action = ConnAction::SendDetach;
            }
            (PromotePending, ReleaseRequested) => {
                // C2 fix: Esc pressed after Enter but before the claim
                // ack arrives. Abort the in-flight promote: snap back
                // to Watching so the compositor mode can sync back to
                // WATCH (see Compositor::observe_pane_states). The run
                // loop is responsible for either cancelling the WS
                // upgrade or, if the ack races and lands after, immediately
                // sending detach when it sees the per-pane state is no
                // longer PromotePending.
                next = Watching;
                action = ConnAction::NoOp;
            }
            (_, ReleaseRequested) => {
                next = self.clone();
                action = ConnAction::NoOp;
            }
            (Driving, DriveClaimReleased) => {
                next = Watching;
                action = ConnAction::NoOp;
            }
            (_, DriveClaimReleased) => {
                next = self.clone();
                action = ConnAction::NoOp;
            }

            // ---- WS drop ----
            (Driving, WsClosed { reason }) | (PromotePending, WsClosed { reason }) => {
                // AC2-FR: never linger with a dead socket. The daemon's
                // heartbeat watchdog releases the claim on its side
                // (drive.rs::HEARTBEAT_TIMEOUT); the FSM demotes here
                // without issuing SendDetach (the socket is already
                // gone). PromotePending behaves the same way - the
                // in-flight RPC failed at the transport layer.
                next = Disconnected { reason };
                action = ConnAction::NoOp;
            }
            (_, WsClosed { reason }) => {
                next = Disconnected { reason };
                action = ConnAction::NoOp;
            }

            // ---- agent exit ----
            (_, AgentExited { code }) => {
                // AC4-HP / AC4-FR: an exit from any state lands us in
                // Exited; if we were driving (or promoting), the claim
                // is gone with the agent.
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

            // ---- watcher connected when not in Connecting ----
            (_, WatcherConnected) => {
                // A late-arriving handshake event from a previous
                // connection cycle; treat as no-op.
                next = self.clone();
                action = ConnAction::NoOp;
            }
        }

        *self = next;
        action
    }
}

/// Global compositor mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Default: panes render read-only; stdin is the compositor's
    /// command surface (arrows / tab focus, Enter take-over, q quit).
    Watch,
    /// One pane (the focused one) is driver-attached; stdin forwards
    /// to its socket.
    Drive,
    /// The focused pane is frozen and the operator is scrolling its
    /// captured history (modal "scrollback mode"). Entered from WATCH via
    /// `Space`; exited with `Esc` (which snaps the pane back to its live
    /// tail). Paging / focus / promote keys are inert while scrolling - the
    /// operator is pinned to the entry pane (Locked Decision 5).
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// Move focus to the next drivable pane.
    FocusNext,
    /// Move focus to the previous drivable pane.
    FocusPrev,
    /// Promote the focused pane to DRIVE (claim attempt).
    Promote,
    /// Release the current driver and return to WATCH.
    Release,
    /// Quit the compositor.
    Quit,
    /// Flip to the next page (`]` / PgDn in WATCH). Clamps at the last page
    /// (no wrap); relocates focus to the new page's first pane so the focused
    /// index stays in the visible slice (fu-grid-pagination, Invariant).
    PageNext,
    /// Flip to the previous page (`[` / PgUp in WATCH). Clamps at page 0.
    PagePrev,
    /// A keystroke from the operator - bytes destined for either the
    /// focused pane (DRIVE) or eaten by the compositor (WATCH).
    Keystroke(Vec<u8>),
    /// Enter scrollback mode on the focused pane (`Space` in WATCH). The
    /// no-history guard lives in the run loop (it owns the panes); the
    /// compositor flips the mode and scrolls up one line to freeze the view.
    EnterScrollback,
    /// Leave scrollback and return to WATCH (`Esc`); the run loop snaps the
    /// pane to its live tail.
    ExitScrollback,
    /// Scroll the focused pane within scrollback mode.
    ScrollLineUp,
    ScrollLineDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,
}

/// Action the run loop must perform after a compositor-level event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositorAction {
    /// Nothing to do.
    NoOp,
    /// Forward bytes to pane `pane_idx`'s driver socket (DRIVE only).
    ForwardKeystrokes { pane_idx: usize, bytes: Vec<u8> },
    /// Tell pane `pane_idx`'s connection FSM that the operator wants to
    /// promote it (the FSM will yield the actual `SendDriveRpc`).
    AttemptPromote { pane_idx: usize },
    /// Tell pane `pane_idx`'s connection FSM that the operator wants to
    /// release the driver (the FSM will yield the actual `SendDetach`).
    AttemptRelease { pane_idx: usize },
    /// Scroll pane `pane_idx`'s captured history (scrollback mode). The run
    /// loop translates `cmd` to an `alacritty_terminal` scroll and applies it
    /// to the pane's terminal.
    Scroll { pane_idx: usize, cmd: ScrollCmd },
    /// Quit the run loop; release any held driver claim first.
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
            mode: Mode::Watch,
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

    /// Re-synchronize the global mode against the per-pane states.
    ///
    /// **Call this from the run loop after every per-pane FSM transition.**
    /// The compositor flips to [`Mode::Drive`] eagerly on Enter (before
    /// the RPC ack lands) so that subsequent keystrokes route correctly
    /// once the per-pane FSM enters [`ConnState::Driving`]. But several
    /// outcomes leave the focused pane non-Driving even though the
    /// compositor still believes it is in DRIVE:
    ///
    /// - Claim denied → pane enters `BusyElsewhere`
    /// - WS dropped → pane enters `Disconnected`
    /// - Agent exited → pane enters `Exited`
    /// - In-flight promote aborted via Esc → pane enters `Watching`
    /// - PromotePending raced with the ack (we already snapped back)
    ///
    /// In each case the compositor mode must return to WATCH so the
    /// next keystroke is consumed as a compositor command, not silently
    /// dropped by the per-pane gate. Without this sync the global mode
    /// would stay DRIVE forever while keystrokes vanish. Caught by
    /// chatgpt-codex-connector on PR #370.
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
        if self.mode == Mode::Drive {
            let idx = self.focus;
            let still_driving =
                idx < pane_states.len() && matches!(pane_states[idx], ConnState::Driving);
            if !still_driving {
                // Focused pane is no longer the active driver. Snap
                // the global mode back so subsequent keystrokes route
                // as compositor commands again.
                self.mode = Mode::Watch;
            }
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
            self.mode = Mode::Watch;
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

        match (self.mode, event) {
            // ---- WATCH: focus + take-over + quit; bytes eaten ----
            (Mode::Watch, InputEvent::FocusNext) => {
                self.focus = next_focus(self.focus, pane_states, /*forward=*/ true);
                self.follow_focus_page(); // task 2.2: focus-follow auto-advance
                CompositorAction::NoOp
            }
            (Mode::Watch, InputEvent::FocusPrev) => {
                self.focus = next_focus(self.focus, pane_states, /*forward=*/ false);
                self.follow_focus_page();
                CompositorAction::NoOp
            }
            // ---- WATCH: page navigation (task 2.1) ----
            (Mode::Watch, InputEvent::PageNext) => {
                self.page_next();
                CompositorAction::NoOp
            }
            (Mode::Watch, InputEvent::PagePrev) => {
                self.page_prev();
                CompositorAction::NoOp
            }
            (Mode::Watch, InputEvent::Promote) => {
                let idx = self.focus;
                if idx < pane_states.len() && pane_states[idx].is_drivable() {
                    // Pre-flip the mode so a follow-up Keystroke in the
                    // same tick routes to the soon-to-be driving pane.
                    // The run loop fires SendDriveRpc and only the
                    // DriveClaimAcquired event yields actual byte
                    // routing through the per-pane FSM.
                    //
                    // NOTE: the global mode flip BEFORE claim ack is
                    // safe because the per-pane FSM still refuses byte
                    // routing via can_route_input() = matches Driving
                    // only. So a stray pre-ack keystroke is dropped at
                    // the second guard (per-pane). The "claim acquired
                    // BEFORE flipping input routing" invariant
                    // (Concurrency) is honored by the per-pane gate,
                    // which is the load-bearing check.
                    self.mode = Mode::Drive;
                    CompositorAction::AttemptPromote { pane_idx: idx }
                } else {
                    // AC3-HP: WATCH consumes Enter and emits no bytes.
                    CompositorAction::NoOp
                }
            }
            (Mode::Watch, InputEvent::Release) => {
                // Esc in WATCH is a no-op (no driver to release).
                CompositorAction::NoOp
            }
            (Mode::Watch, InputEvent::Keystroke(_)) => {
                // AC3-HP: no keystroke reaches any agent in WATCH.
                CompositorAction::NoOp
            }
            (Mode::Watch, InputEvent::Quit) => CompositorAction::Quit,
            // ---- WATCH: enter scrollback on the focused pane ----
            // The no-history guard lives in the run loop (it owns the panes);
            // here we flip the mode AND scroll up one line. That scroll is
            // load-bearing: alacritty only freezes the viewport when the
            // display offset is non-zero, so entering at offset 0 would keep
            // following the live tail on a busy agent (chatgpt-codex, PR #387).
            // One line up moves the offset to >=1, freezing immediately; the
            // run loop has already verified history exists. Scroll/exit events
            // are WATCH-only by construction (key_to_input never emits them in
            // WATCH), so they fall through to the catch-all as no-ops.
            (Mode::Watch, InputEvent::EnterScrollback) => {
                self.mode = Mode::Scrollback;
                CompositorAction::Scroll {
                    pane_idx: self.focus,
                    cmd: ScrollCmd::LineUp,
                }
            }
            (Mode::Watch, _) => CompositorAction::NoOp,

            // ---- DRIVE: keystrokes forward; Release demotes ----
            (Mode::Drive, InputEvent::Keystroke(bytes)) => {
                let idx = self.focus;
                if idx < pane_states.len() && pane_states[idx].can_route_input() {
                    CompositorAction::ForwardKeystrokes {
                        pane_idx: idx,
                        bytes,
                    }
                } else {
                    // Per-pane gate refused. The DriveClaimAcquired
                    // event for this pane has not landed yet (or the
                    // claim dropped). Eat the bytes rather than queue
                    // them - the operator sees no echo and presses
                    // again, which is the right UX for a half-second
                    // claim-acquisition window.
                    CompositorAction::NoOp
                }
            }
            (Mode::Drive, InputEvent::Release) => {
                let idx = self.focus;
                // AC2-UI: release demotes to WATCH.
                self.mode = Mode::Watch;
                CompositorAction::AttemptRelease { pane_idx: idx }
            }
            (Mode::Drive, InputEvent::FocusNext)
            | (Mode::Drive, InputEvent::FocusPrev)
            | (Mode::Drive, InputEvent::Promote)
            | (Mode::Drive, InputEvent::PageNext)
            | (Mode::Drive, InputEvent::PagePrev) => {
                // In DRIVE mode focus-change, Enter, and page keys forward to
                // the agent as keystrokes (Tab is meaningful inside shells,
                // Enter inside REPLs, `[`/`]` are real characters). The run
                // loop maps the original raw key bytes into a Keystroke event
                // (key_to_input never classifies these as page/focus events in
                // DRIVE), so reaching here means the input handler classified
                // them - in DRIVE it should not, but be defensive. Paging is
                // WATCH-only (Locked Decision 2): a page key never flips a page
                // while a driver claim is held.
                CompositorAction::NoOp
            }
            (Mode::Drive, InputEvent::Quit) => {
                // q in DRIVE forwards as a keystroke too (q is a real
                // character agents may want to receive); but Quit as a
                // distinct event only arrives in WATCH per the input
                // handler. Defensive: release first, then quit.
                self.mode = Mode::Watch;
                CompositorAction::Quit
            }
            // Scroll / scrollback-entry events are WATCH-only (Locked Decision
            // 4: scrollback is not reachable from DRIVE - the agent's own TUI
            // scrollback handles in-drive review). Inert if they arrive.
            (Mode::Drive, _) => CompositorAction::NoOp,

            // ---- SCROLLBACK: scroll the frozen focused pane; Esc exits ----
            // The focused pane is pinned for the duration; focus / paging /
            // promote / keystroke / re-entry are inert (Locked Decision 5).
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
                self.mode = Mode::Watch;
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

    /// AC1-HP / AC1-FR: a watcher that has not yet received bytes shows
    /// "connecting…", distinct from a blank "watching".
    #[test]
    fn watch_connect_then_first_bytes_promote_to_watching() {
        let mut s = ConnState::Connecting;
        // Connection finished its handshake but bytes have not arrived.
        let a = s.step(ConnEvent::WatcherConnected);
        assert_eq!(a, ConnAction::NoOp);
        assert!(matches!(s, ConnState::Connecting));
        // First bytes flip to Watching.
        let a = s.step(ConnEvent::BytesReceived(b"hello".to_vec()));
        assert_eq!(a, ConnAction::FeedRenderer(b"hello".to_vec()));
        assert_eq!(s, ConnState::Watching);
    }

    /// AC4-HP: agent exit anywhere lands in Exited; bytes from a
    /// buffered frame after exit are dropped.
    #[test]
    fn agent_exit_from_watching_lands_in_exited_and_drops_bytes() {
        let mut s = ConnState::Watching;
        s.step(ConnEvent::AgentExited { code: 42 });
        assert_eq!(s, ConnState::Exited { code: 42 });
        let a = s.step(ConnEvent::BytesReceived(vec![1, 2, 3]));
        assert_eq!(a, ConnAction::NoOp);
        assert_eq!(s, ConnState::Exited { code: 42 });
    }

    /// AC4-FR: agent exit while driving releases the claim (the daemon
    /// drops it with the agent) and lands in Exited.
    #[test]
    fn agent_exit_during_drive_lands_in_exited() {
        let mut s = ConnState::Driving;
        let a = s.step(ConnEvent::AgentExited { code: -1 });
        assert_eq!(a, ConnAction::NoOp);
        assert_eq!(s, ConnState::Exited { code: -1 });
    }

    /// AC2-FR: WS drop during driving demotes to Disconnected - never
    /// linger in Driving on a dead socket. SendDetach is NOT issued
    /// because the socket is already gone (the daemon's heartbeat
    /// watchdog releases the claim on its side).
    #[test]
    fn ws_drop_during_drive_demotes_to_disconnected() {
        let mut s = ConnState::Driving;
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

    /// AC2-HP: promote from Watching → SendDriveRpc, then DriveClaimAcquired
    /// → Driving.
    #[test]
    fn promote_happy_path_acquires_claim() {
        let mut s = ConnState::Watching;
        let a = s.step(ConnEvent::PromoteRequested);
        assert_eq!(a, ConnAction::SendDriveRpc);
        assert!(matches!(s, ConnState::PromotePending));
        let a = s.step(ConnEvent::DriveClaimAcquired);
        assert_eq!(a, ConnAction::NoOp);
        assert_eq!(s, ConnState::Driving);
    }

    /// C3 fix: an idle agent in Connecting (handshake done, no bytes
    /// yet) is drivable; Enter fires the drive RPC and transitions to
    /// PromotePending. Mirrors the Watching path.
    #[test]
    fn promote_from_connecting_fires_drive_rpc() {
        let mut s = ConnState::Connecting;
        assert!(s.is_drivable(), "idle Connecting must be drivable");
        let a = s.step(ConnEvent::PromoteRequested);
        assert_eq!(a, ConnAction::SendDriveRpc);
        assert_eq!(s, ConnState::PromotePending);
    }

    /// AC2-ERR: promote denial lands in BusyElsewhere with a flash, not
    /// in Driving.
    #[test]
    fn promote_denied_lands_in_busy_elsewhere() {
        let mut s = ConnState::Watching;
        s.step(ConnEvent::PromoteRequested);
        assert_eq!(s, ConnState::PromotePending);
        let a = s.step(ConnEvent::DriveClaimDenied {
            holder: Some("op-jane".to_string()),
        });
        assert!(matches!(a, ConnAction::FlashDenial(_)));
        assert!(
            matches!(s, ConnState::BusyElsewhere { holder } if holder.as_deref() == Some("op-jane"))
        );
    }

    /// C2 fix: Esc pressed during PromotePending (after Enter, before
    /// ack) aborts the in-flight promote - state returns to Watching.
    /// The run loop is responsible for either cancelling the in-flight
    /// WS upgrade or immediately sending detach when the late ack lands.
    #[test]
    fn release_during_promote_pending_aborts_to_watching() {
        let mut s = ConnState::Watching;
        s.step(ConnEvent::PromoteRequested);
        assert_eq!(s, ConnState::PromotePending);
        let a = s.step(ConnEvent::ReleaseRequested);
        assert_eq!(a, ConnAction::NoOp);
        assert_eq!(s, ConnState::Watching);
    }

    /// Once aborted via Release-during-PromotePending, a late
    /// DriveClaimAcquired must NOT reactivate Driving - the catch-all
    /// stale-ack rule applies.
    #[test]
    fn late_ack_after_abort_does_not_reactivate_drive() {
        let mut s = ConnState::Watching;
        s.step(ConnEvent::PromoteRequested);
        s.step(ConnEvent::ReleaseRequested);
        assert_eq!(s, ConnState::Watching);
        let a = s.step(ConnEvent::DriveClaimAcquired);
        assert_eq!(a, ConnAction::NoOp);
        assert_eq!(s, ConnState::Watching, "stale ack must not stick");
    }

    /// AC3-DOUBLE: a second PromoteRequested inside the inflight window
    /// is a no-op - claim acquisition serializes through the first RPC.
    #[test]
    fn double_promote_serialized_to_single_claim() {
        let mut s = ConnState::Watching;
        let a1 = s.step(ConnEvent::PromoteRequested);
        assert_eq!(a1, ConnAction::SendDriveRpc);
        let a2 = s.step(ConnEvent::PromoteRequested);
        assert_eq!(
            a2,
            ConnAction::NoOp,
            "second Enter must not fire a second RPC"
        );
    }

    /// AC2-UI: release from Driving sends a detach and returns to Watching.
    #[test]
    fn release_from_drive_returns_to_watching() {
        let mut s = ConnState::Driving;
        let a = s.step(ConnEvent::ReleaseRequested);
        assert_eq!(a, ConnAction::SendDetach);
        assert_eq!(s, ConnState::Watching);
    }

    /// Retry from Disconnected re-attempts the watcher connection.
    #[test]
    fn retry_from_disconnected_reopens_watcher() {
        let mut s = ConnState::Disconnected {
            reason: "lost".to_string(),
        };
        let a = s.step(ConnEvent::RetryRequested);
        assert_eq!(a, ConnAction::Reconnect);
        assert!(matches!(s, ConnState::Connecting));
    }

    /// Promote on an exited pane flashes a denial and stays Exited.
    #[test]
    fn promote_on_exited_flashes_denial() {
        let mut s = ConnState::Exited { code: 0 };
        let a = s.step(ConnEvent::PromoteRequested);
        assert!(matches!(a, ConnAction::FlashDenial(_)));
        assert_eq!(s, ConnState::Exited { code: 0 });
    }

    /// can_route_input is true ONLY in Driving - the per-pane gate that
    /// closes the "stray keystroke during claim-acquire window" loophole.
    #[test]
    fn can_route_input_only_in_driving() {
        assert!(!ConnState::Connecting.can_route_input());
        assert!(!ConnState::PromotePending.can_route_input());
        assert!(!ConnState::Watching.can_route_input());
        assert!(ConnState::Driving.can_route_input());
        assert!(!ConnState::BusyElsewhere { holder: None }.can_route_input());
        assert!(!ConnState::Disconnected { reason: "x".into() }.can_route_input());
        assert!(!ConnState::Exited { code: 0 }.can_route_input());
    }

    /// PromotePending is NOT drivable - a second Enter inside the
    /// inflight window must be a no-op (AC3-DOUBLE), and other UI that
    /// asks "should I let Enter fire?" must say no.
    #[test]
    fn promote_pending_is_not_drivable() {
        assert!(!ConnState::PromotePending.is_drivable());
    }

    /// C1 fix: when the focused pane is no longer Driving, the
    /// compositor mode snaps back to WATCH on the next observe call.
    /// Covers denial, WS-drop, exit, and abort cases.
    #[test]
    fn observe_snaps_mode_back_when_focused_pane_leaves_driving() {
        let mut comp = Compositor::new(2);
        let mut states = watching_panes(2);
        comp.step(InputEvent::FocusNext, &states);
        comp.step(InputEvent::Promote, &states);
        assert_eq!(comp.mode(), Mode::Drive);
        // Simulate the per-pane FSM walking PromotePending → Driving →
        // some demoted state.
        states[1] = ConnState::PromotePending;
        states[1].step(ConnEvent::DriveClaimAcquired);
        assert_eq!(states[1], ConnState::Driving);
        comp.observe_pane_states(&states); // still Driving → mode unchanged
        assert_eq!(comp.mode(), Mode::Drive);

        // Denial path: pane goes to BusyElsewhere; mode must snap back.
        states[1] = ConnState::BusyElsewhere { holder: None };
        comp.observe_pane_states(&states);
        assert_eq!(
            comp.mode(),
            Mode::Watch,
            "denied pane should snap mode to WATCH"
        );
    }

    /// C1 fix corollary: WS drop on the driving pane also snaps mode
    /// back. The per-pane FSM has already demoted to Disconnected.
    #[test]
    fn observe_snaps_mode_on_ws_drop() {
        let mut comp = Compositor::new(1);
        let mut states = vec![ConnState::Watching];
        comp.step(InputEvent::Promote, &states);
        states[0] = ConnState::PromotePending;
        states[0].step(ConnEvent::DriveClaimAcquired);
        comp.observe_pane_states(&states);
        assert_eq!(comp.mode(), Mode::Drive);
        states[0].step(ConnEvent::WsClosed {
            reason: "lost".to_string(),
        });
        comp.observe_pane_states(&states);
        assert_eq!(comp.mode(), Mode::Watch);
    }

    /// C1 fix: agent exit during DRIVE snaps mode back to WATCH.
    #[test]
    fn observe_snaps_mode_on_agent_exit() {
        let mut comp = Compositor::new(1);
        let mut states = vec![ConnState::Watching];
        comp.step(InputEvent::Promote, &states);
        states[0] = ConnState::PromotePending;
        states[0].step(ConnEvent::DriveClaimAcquired);
        comp.observe_pane_states(&states);
        assert_eq!(comp.mode(), Mode::Drive);
        states[0].step(ConnEvent::AgentExited { code: 1 });
        comp.observe_pane_states(&states);
        assert_eq!(comp.mode(), Mode::Watch);
    }

    // ---- Compositor transition tests (Wave 3.2 + 4.1) ----

    fn watching_panes(n: usize) -> Vec<ConnState> {
        (0..n).map(|_| ConnState::Watching).collect()
    }

    /// AC3-HP: WATCH cycles focus on FocusNext / FocusPrev without
    /// emitting any bytes to any agent.
    #[test]
    fn watch_focus_cycles_without_keystrokes() {
        let panes = watching_panes(3);
        let mut c = Compositor::new(3);
        assert_eq!(c.mode(), Mode::Watch);
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

    /// AC3-HP again: stdin keystrokes in WATCH are eaten.
    #[test]
    fn watch_keystrokes_dont_reach_panes() {
        let panes = watching_panes(2);
        let mut c = Compositor::new(2);
        let a = c.step(InputEvent::Keystroke(b"abc".to_vec()), &panes);
        assert_eq!(a, CompositorAction::NoOp);
    }

    /// AC2-HP step: Promote on a drivable focused pane flips to DRIVE
    /// and emits AttemptPromote.
    #[test]
    fn promote_flips_mode_and_emits_attempt() {
        let panes = watching_panes(2);
        let mut c = Compositor::new(2);
        c.step(InputEvent::FocusNext, &panes);
        let a = c.step(InputEvent::Promote, &panes);
        assert_eq!(a, CompositorAction::AttemptPromote { pane_idx: 1 });
        assert_eq!(c.mode(), Mode::Drive);
    }

    /// AC2-ERR: Promote on an undrivable focused pane (Exited /
    /// Disconnected) is a no-op and stays in WATCH.
    #[test]
    fn promote_on_exited_pane_is_noop() {
        let panes = vec![ConnState::Exited { code: 0 }, ConnState::Watching];
        let mut c = Compositor::new(2);
        let a = c.step(InputEvent::Promote, &panes);
        assert_eq!(a, CompositorAction::NoOp);
        assert_eq!(c.mode(), Mode::Watch);
    }

    /// AC3-HP: focus skips undrivable panes when cycling.
    #[test]
    fn focus_skips_undrivable_panes() {
        let panes = vec![
            ConnState::Watching,
            ConnState::Exited { code: 0 },
            ConnState::Watching,
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
        c.step(InputEvent::FocusNext, &watching_panes(3));
        c.step(InputEvent::FocusNext, &watching_panes(3));
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

    /// AC2-UI: Release from DRIVE returns to WATCH and emits
    /// AttemptRelease for the focused pane.
    #[test]
    fn release_demotes_drive_to_watch() {
        // Both panes Watching at first so FocusNext can reach pane 1
        // (Driving is NOT drivable, so a Driving seed would have
        // trapped focus at 0).
        let mut panes = watching_panes(2);
        let mut c = Compositor::new(2);
        c.step(InputEvent::FocusNext, &panes);
        c.step(InputEvent::Promote, &panes);
        // The claim is then acquired by the run loop; the per-pane
        // FSM transitions PromotePending -> Driving (the action the
        // run loop owns).
        panes[1] = ConnState::PromotePending; // Promote moved it here
        panes[1].step(ConnEvent::DriveClaimAcquired);
        assert_eq!(panes[1], ConnState::Driving);
        let a = c.step(InputEvent::Release, &panes);
        assert_eq!(a, CompositorAction::AttemptRelease { pane_idx: 1 });
        assert_eq!(c.mode(), Mode::Watch);
    }

    /// Concurrency invariant: bytes do NOT route to a pane whose per-
    /// pane FSM is not in Driving, even while the global mode is DRIVE.
    /// This is the load-bearing guard that closes the "claim-acquire
    /// window" hole.
    #[test]
    fn keystrokes_during_claim_acquire_are_dropped() {
        let mut c = Compositor::new(2);
        let panes = vec![
            ConnState::Watching,
            ConnState::PromotePending, // RPC inflight, claim not yet acquired
        ];
        c.step(InputEvent::FocusNext, &panes);
        c.step(InputEvent::Promote, &panes); // flips mode to Drive
        assert_eq!(c.mode(), Mode::Drive);
        // Pane 1's per-pane state is PromotePending (mid-RPC) so a
        // keystroke must NOT reach it.
        let a = c.step(InputEvent::Keystroke(b"x".to_vec()), &panes);
        assert_eq!(a, CompositorAction::NoOp);
    }

    /// Once the claim is acquired, keystrokes route to the focused
    /// pane.
    #[test]
    fn keystrokes_route_to_driving_pane() {
        let mut c = Compositor::new(2);
        // Start with both Watching so FocusNext can land on pane 1.
        let mut panes = watching_panes(2);
        c.step(InputEvent::FocusNext, &panes);
        c.step(InputEvent::Promote, &panes); // global mode -> Drive
                                             // Now the per-pane FSM is updated by the run loop after the
                                             // claim acquisition lands.
        panes[1] = ConnState::PromotePending;
        panes[1].step(ConnEvent::DriveClaimAcquired);
        assert_eq!(panes[1], ConnState::Driving);
        let a = c.step(InputEvent::Keystroke(b"hello".to_vec()), &panes);
        assert_eq!(
            a,
            CompositorAction::ForwardKeystrokes {
                pane_idx: 1,
                bytes: b"hello".to_vec()
            }
        );
    }

    /// Quit in WATCH yields Quit; the run loop releases any held claim
    /// before exit.
    #[test]
    fn quit_in_watch_yields_quit_action() {
        let mut c = Compositor::new(2);
        let panes = watching_panes(2);
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
        let panes = watching_panes(3);
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
        let panes = watching_panes(10);
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
        let panes = watching_panes(10);
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
                let panes = watching_panes(12);
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
        let panes = watching_panes(10);
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
        let panes = watching_panes(10);
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
        let panes = watching_panes(10);
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
        let panes = watching_panes(10);
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
        let panes = watching_panes(3);
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
        let panes = watching_panes(10);
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
        let panes = watching_panes(6);
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
        let panes = watching_panes(10);
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
            ConnState::Watching,
            ConnState::Driving,
            ConnState::BusyElsewhere { holder: None },
            ConnState::BusyElsewhere {
                holder: Some("op".into()),
            },
            ConnState::Disconnected { reason: "x".into() },
            ConnState::Exited { code: 0 },
        ] {
            assert!(!s.label().is_empty(), "state {s:?} has empty label");
        }
    }

    // ── Scrollback mode FSM ──────────────────────────────────────────────

    /// AC1-HP / AC4-HP: Space enters scrollback (mode flip, no action), Esc
    /// exits back to WATCH and snaps the focused pane to its live tail.
    #[test]
    fn enter_scrollback_flips_mode_and_exit_returns_and_snaps() {
        let panes = watching_panes(2);
        let mut c = Compositor::new(2);
        // Entry flips to Scrollback AND scrolls up one line to freeze the
        // viewport immediately (chatgpt-codex, PR #387).
        let a = c.step(InputEvent::EnterScrollback, &panes);
        assert_eq!(
            a,
            CompositorAction::Scroll {
                pane_idx: 0,
                cmd: ScrollCmd::LineUp
            },
            "entry freezes the pane by scrolling up one line"
        );
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
        assert_eq!(c.mode(), Mode::Watch);
    }

    /// AC1-HP: scroll keys in scrollback emit Scroll actions for the focused pane.
    #[test]
    fn scroll_events_emit_scroll_actions_for_focused_pane() {
        let panes = watching_panes(3);
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

    /// Locked Decision 5: focus / paging / promote / keystroke are inert while
    /// scrolling (the operator is pinned to the entry pane).
    #[test]
    fn inert_keys_in_scrollback_are_noops() {
        let panes = watching_panes(3);
        let mut c = Compositor::new(3);
        c.step(InputEvent::EnterScrollback, &panes);
        for ev in [
            InputEvent::FocusNext,
            InputEvent::FocusPrev,
            InputEvent::PageNext,
            InputEvent::PagePrev,
            InputEvent::Promote,
            InputEvent::Keystroke(b"x".to_vec()),
            InputEvent::EnterScrollback, // re-entry is idempotent
        ] {
            assert_eq!(c.step(ev, &panes), CompositorAction::NoOp);
            assert_eq!(c.focus(), 0, "focus is pinned in scrollback");
            assert_eq!(c.mode(), Mode::Scrollback);
        }
    }

    /// Locked Decision 4: scrollback is not reachable from DRIVE.
    #[test]
    fn enter_scrollback_inert_in_drive() {
        let panes = watching_panes(2);
        let mut c = Compositor::new(2);
        c.step(InputEvent::Promote, &panes); // -> Drive
        assert_eq!(c.mode(), Mode::Drive);
        let a = c.step(InputEvent::EnterScrollback, &panes);
        assert_eq!(a, CompositorAction::NoOp);
        assert_eq!(c.mode(), Mode::Drive, "scrollback never engages from DRIVE");
    }
}
