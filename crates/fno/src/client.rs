//! The mux client: chrome + a dumb compositor over the server's pane frames.
//!
//! Takes over the terminal (crossterm raw mode + alternate screen), attaches
//! to the session server (spawning one if absent), and renders three things:
//! a top tab bar, a left sideline (squads with caret dropdowns), and the
//! content area where per-pane `Frame`s are blitted into the rects the last
//! `Layout` assigned. The client never runs the layout algorithm and never
//! emulates VT (Locked Decision 3): rects and grids both come from the
//! server, which is what makes reattach exact.
//!
//! Input goes through the prefix-key scanner (`keys.rs`): bare bytes forward
//! verbatim on the reliable channel (AC2-UI), chords become `Command`s.
//! Caret expansion, sideline visibility, and the selector are CLIENT-LOCAL
//! view state - never on the wire (Locked Decision 15).
//!
//! Every error surface while the compositor owns the terminal goes through
//! the rendered UI (tab-bar notice + BEL), never stderr (x-0175 pitfall).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::style::Color as CtColor;
use crossterm::{cursor, queue, style, terminal};
use tokio::sync::mpsc;

use crate::agents_view::lineage_layout;
use crate::chrome;

mod rename_overlay;

use self::rename_overlay::RenameTarget;

// The placement pickers (attach `p`, portal `P`) live in their own module;
// client.rs is shrink-only under the file-budget gate.
mod placement_pickers;

use self::placement_pickers::{attach_place_keys, portal_pick_keys, AttachPlace, PortalPick};
use crate::keys::{
    key_bindings, meta_rows, resolve_chord, Event, KeySection, Scanner, PANE_IDS_REPEAT_WINDOW,
};
use crate::lane_colors_panel::LaneColorsUi;
use crate::popup::{self, Anchor, GridCell, NavDir, Popup, PopupRow};
use crate::proto::{
    self, cell_flags, is_mission_squad, read_msg, write_msg, AgentBadge, AgentNoPaneReason,
    AgentRow, AnswerablePrompt, BacklogCard, BacklogVerb, BlockDir, CardState, Cell, ClientMsg,
    Color, Command, Frame, MouseButton, MouseEvent, MouseKind, PanePlacement, PaneTarget,
    PlacementFallback, ProtoError, ServerMsg, SquadMeta, TabMeta, BUILD_VERSION, MAX_MAIL_TEXT,
    MAX_SQUAD_NAME, MAX_TAB_NAME, PROTO_VERSION,
};
use crate::sideline_color;
use crate::theme::Theme;
use crate::tree::{Axis, Dir, Rect, TabId};
use crate::view_store::{
    self, next_view, AgentSort, AgentSortColumn, Density, SectionKey, SectionView, SortDirection,
};
use crate::vt::ShellActivity;

mod row_stamp;
use self::row_stamp::{no_pane_notice, paint_notice_overlay, paint_row_stamp, RowArm, RowStamp};

/// How long to wait for a just-spawned server to accept.
const SPAWN_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Connect bound for the attach path. Longer than the scriptable verbs'
/// probe (a human is willing to wait a beat) but never infinite: a wedged
/// server must produce a clear line, not a hang.
const ATTACH_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Sideline width in columns at [`Density::Regular`], divider column included.
/// Client-local chrome: the server sees only the content-area viewport.
const PANEL_W: u16 = 28;
/// (x-b186) The [`Density::Slim`] rail width. FIXED rather than fitted to the
/// widest header: a rail that resized itself as squads came and went would
/// shift the content area on unrelated events, and `header_band_text` already
/// degrades a too-long header gracefully (rollup pairs drop from the
/// least-severe end, then the label truncates). Wide enough for a short
/// workspace name plus a rollup pair, which is what makes slim legible rather
/// than blind.
const SLIM_PANEL_W: u16 = 16;
/// (x-b186) The narrowest slim rail. Below this the sideline finally hides, but
/// between here and [`SLIM_PANEL_W`] it clamps - a rail that disappeared on a
/// narrow terminal would contradict the one thing slim promises.
const MIN_SLIM_PANEL_W: u16 = 8;
/// Below this many content columns the sideline auto-hides (AC6-EDGE).
const MIN_CONTENT_COLS: u16 = 40;

/// Extended-table column widths in display columns, render order: status glyph,
/// agent, last message, PR, and relative last-update age. The first and last
/// three cells are fixed; the agent and message cells share the remainder.
const COL_STATUS: u16 = 4;
const COL_PR: u16 = 7;
const COL_TIME: u16 = 6;
const COL_MIN_NAME: u16 = 12;
const COL_MAX_NAME: u16 = 24;
const COL_MIN_TAIL: u16 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ColumnSpan {
    start: u16,
    width: u16,
}

impl ColumnSpan {
    fn contains(self, col: u16) -> bool {
        col >= self.start && col < self.start + self.width
    }
}

/// The one geometry authority for the extended table. Header text, row text,
/// and header hit testing all consume these spans, so age stays right-anchored
/// when the panel width changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TableLayout {
    text_w: u16,
    status: ColumnSpan,
    agent: ColumnSpan,
    tail: Option<ColumnSpan>,
    pr: ColumnSpan,
    age: ColumnSpan,
}

impl TableLayout {
    fn fitting(text_w: u16) -> Option<Self> {
        let fixed = COL_STATUS + COL_PR + COL_TIME;
        let flexible = text_w.checked_sub(fixed)?;
        if flexible < COL_MIN_NAME {
            return None;
        }

        let requested_name = (flexible / 3).clamp(COL_MIN_NAME, COL_MAX_NAME);
        let (name_w, tail_w) = if flexible.saturating_sub(requested_name) >= COL_MIN_TAIL {
            (requested_name, Some(flexible - requested_name))
        } else {
            (flexible, None)
        };
        let status = ColumnSpan {
            start: 0,
            width: COL_STATUS,
        };
        let agent = ColumnSpan {
            start: status.start + status.width,
            width: name_w,
        };
        let tail = tail_w.map(|width| ColumnSpan {
            start: agent.start + agent.width,
            width,
        });
        let pr_start = tail
            .map(|span| span.start + span.width)
            .unwrap_or(agent.start + agent.width);
        let pr = ColumnSpan {
            start: pr_start,
            width: COL_PR,
        };
        let age = ColumnSpan {
            start: text_w - COL_TIME,
            width: COL_TIME,
        };
        debug_assert_eq!(age.start, pr.start + pr.width);
        debug_assert_eq!(age.start + age.width, text_w);
        Some(Self {
            text_w,
            status,
            agent,
            tail,
            pr,
            age,
        })
    }
}

/// (x-b186) The full extended-table panel width (every column plus the divider),
/// what entering `Extended` widens to before any clamp.
const EXTENDED_PANEL_W: u16 = COL_STATUS + COL_PR + COL_TIME + 54 + 1;
/// The narrowest useful extended panel: fixed status/PR/age cells, a readable
/// agent cell, and the divider. The message cell is omitted only below its
/// eight-column floor; age is never dropped from an admitted table.
const MIN_EXTENDED_PANEL_W: u16 = COL_STATUS + COL_MIN_NAME + COL_PR + COL_TIME + 1;

/// (x-b186) Columns the top-right density button reserves on the sideline's
/// first row: the state glyph plus a trailing pad (x-2e86) so it does not sit
/// flush against the divider.
const DENSITY_BTN_W: usize = 2;

/// (x-2e86) The width a density jumps to when picked as a preset (the density
/// key or button): each mode's canonical size. Free-standing so the preset
/// path can price a mode without being in it.
fn canonical_width(d: Density) -> u16 {
    match d {
        Density::Slim => SLIM_PANEL_W,
        Density::Regular => PANEL_W,
        Density::Extended => EXTENDED_PANEL_W,
    }
}

/// (x-2e86) The narrowest width at which a density still renders its structure.
/// A drag below this demotes the density (Locked 5). Only `Extended` has a
/// floor above [`MIN_SLIM_PANEL_W`]: the tree and the rail truncate gracefully
/// down to the slim floor, but the table needs room for status + agent + PR + age.
fn min_render_width(d: Density) -> u16 {
    match d {
        Density::Slim | Density::Regular => MIN_SLIM_PANEL_W,
        Density::Extended => MIN_EXTENDED_PANEL_W,
    }
}

/// (x-2e86) The smallest terminal room ([`sideline_max_width`]) at which a
/// density is shown at all; below it the rail AUTO-HIDES so the panes keep the
/// screen, rather than rendering a rail too cramped to be worth its columns.
///
/// This is the pre-x-2e86 per-density floor, and it is deliberately NOT
/// [`min_render_width`]: a `Slim` rail stays useful squished to
/// [`MIN_SLIM_PANEL_W`], but a `Regular` tree below [`PANEL_W`] or an `Extended`
/// table below [`MIN_EXTENDED_PANEL_W`] is too tight to read, so on a narrow
/// terminal those hide (giving content the room) exactly as they did before free
/// width. It gates on terminal CAPACITY, not on the stored width, so a rail the
/// terminal CAN admit still renders at a small DRAGGED width (a drag-to-8 Regular
/// shows, because the terminal that fits 28 also fits 8).
fn min_admit_width(d: Density) -> u16 {
    match d {
        Density::Slim => MIN_SLIM_PANEL_W,
        Density::Regular => PANEL_W,
        Density::Extended => MIN_EXTENDED_PANEL_W,
    }
}

/// (x-2e86) The largest sideline width this terminal allows: 60% of the columns,
/// but never so wide that content drops below [`MIN_CONTENT_COLS`] - the tighter
/// bound wins. Saturating throughout (a u32 intermediate for the 60%) so a
/// degenerate terminal underflows to 0 rather than panicking; `panel_w` reads
/// that 0 as "too narrow, hide the rail".
fn sideline_max_width(term_cols: u16) -> u16 {
    let sixty = ((term_cols as u32) * 3 / 5) as u16;
    sixty.min(term_cols.saturating_sub(MIN_CONTENT_COLS))
}

fn density_glyph(d: Density) -> char {
    // (x-2e86) A fill ramp - rail, tree, table - reading as increasing density.
    // All three are East-Asian-width 1 (U+2581/2584/2588), which the button's
    // column math and the header-band composition require.
    match d {
        Density::Slim => '▁',
        Density::Regular => '▄',
        Density::Extended => '█',
    }
}
/// The tab bar row.
const TAB_BAR_ROWS: u16 = 1;
/// The status row (US4): one always-on bottom line of client-local chrome.
const STATUS_ROWS: u16 = 1;
/// Below this many terminal rows the bottom chrome (status row + which-key
/// hint) auto-hides and the content area recovers the line (AC4-ERR).
const MIN_ROWS_FOR_STATUS: u16 = TAB_BAR_ROWS + STATUS_ROWS + 5;
/// The sideline footer's `+ new` and `☰ menu` labels (x-8ccf US4). The menu
/// button rides the existing new-workspace footer row's right edge when the
/// panel is wide enough (see [`View::footer_menu_range`]).
const FOOTER_NEW_LABEL: &str = "+ new workspace";
const FOOTER_MENU: &str = "☰ menu";
/// How long a pending prefix chord waits before the which-key hint paints
/// (US4, AC4-HP). `prefix+?` shows the full table instantly instead.
const HINT_DELAY: Duration = Duration::from_millis(400);
/// (x-e10f fix) How long a held global-chord candidate (a lone Esc so far)
/// waits for the chord's remaining bytes before flushing to the pane - the
/// tmux escape-time analog. A terminal delivers a whole CSI in one read, so
/// anything still pending after this window is a bare Esc (or a torn write
/// older than the window), and Esc-to-cancel inside a pane must not stall
/// until the next keystroke. 40ms: far above intra-CSI byte gaps, far below
/// the latency a typist feels.
const CHORD_FLUSH_AFTER: Duration = Duration::from_millis(40);
/// (x-cf97) How long a held tab number waits for the next digit before
/// resolving - the tmux escape-time analog for a typed number. Far above an
/// inter-keystroke gap (so `3` then `4` lands tab 34, never 3 then 4), short
/// enough that a one-digit jump feels immediate. Enter always resolves now.
const DIGIT_FLUSH_AFTER: Duration = Duration::from_millis(400);

/// The client-side pane identity overlay follows the scanner's repeat grace.
pub const PANE_ID_REVEAL_WINDOW: Duration = PANE_IDS_REPEAT_WINDOW;

/// (x-c5ee) The top-K live-row cap per rendered squad: attention rows
/// (Blocked/Working/DoneUnseen) always render, then idle rows fill up to this
/// many LIVE rows total; the idle overflow folds into one `+N more` row. Sized
/// a little above a typical attention set so the common squad emits no fold.
/// Dead rows sit outside this budget under the section view's control.
const SQUAD_ROW_CAP: usize = 8;

/// How long the pointer must settle on one new pane before focus follows it
/// (x-a496). 1003 reports every crossed cell, so a fast sweep produces a burst;
/// only a pane that stays under the pointer this long steals focus, coalescing
/// the burst to one `FocusPane` for the pane the pointer lands on.
const HOVER_DEBOUNCE: Duration = Duration::from_millis(50);

/// (hover affordance) How long the pointer must rest on ONE cell before the
/// client asks the server whether that cell belongs to a link. A separate
/// clock from [`HOVER_DEBOUNCE`]: focus-follows-mouse debounces the PANE
/// (keeping the first landing time through in-pane motion), link detection
/// debounces the exact CELL (every crossed cell restarts it), so the two
/// share a duration but never a deadline. Same 50ms so the affordance feels
/// like the focus it travels with.
const LINK_HOVER_DEBOUNCE: Duration = HOVER_DEBOUNCE;

/// (hover affordance) Client-local hover-link state. Detection is
/// server-side (the grid, its scrollback and OSC 8 anchors live there); this
/// holds the client's pointer target, the debounce clock, and the accepted
/// underline span. Nothing here is shared: the underline is painted into the
/// composed frame only, never the cached server `Frame`, and the server
/// reply carries coordinates only, so no pane text crosses back.
#[derive(Debug, Default)]
struct LinkHoverState {
    /// The cell the pointer last rested on, and when its quiet period ends.
    /// `None` when the pointer is over chrome, a divider, an overlay, or out
    /// of every live pane - those targets carry no probe and clear the
    /// underline immediately, without a request.
    pending: Option<LinkTarget>,
    /// The next probe's seq, bumped per probe (and per frame-restart) so a
    /// reply for a target the pointer has already left, or for a span a new
    /// frame invalidated, is dropped on arrival.
    next_seq: u64,
    /// The accepted underline: which pane and which of its visible cells. A
    /// miss (empty reply) clears it. Cleared on every new frame for that pane
    /// and re-debounced, so streaming output never paints a stale span and
    /// never scans at frame cadence.
    accepted: Option<(u64, Vec<(u16, u16)>)>,
}

/// One debounced hover probe target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LinkTarget {
    pane: u64,
    row: u16,
    col: u16,
    /// Identifies THIS probe on the wire and its reply.
    seq: u64,
    /// When the quiet period ends and the probe fires.
    deadline: Instant,
    /// Whether the probe already fired for this target. Kept so the deadline
    /// arm does not re-send while the pointer rests motionless (a still
    /// pointer emits no further events to re-arm on).
    fired: bool,
}

impl LinkHoverState {
    /// Fold one pointer position into the probe state. A new cell restarts
    /// the quiet period and clears the old underline immediately (an
    /// underline trailing the pointer reads as stuck, and the cell may not be
    /// a link at all); the same cell changes nothing, so in-cell jitter does
    /// not starve the probe. `None` (chrome/divider/overlay/outside) drops
    /// the target and the underline with no request.
    fn retarget(&mut self, hit: Option<(u64, u16, u16)>, now: Instant) {
        match hit {
            Some((pane, row, col)) => {
                if !matches!(
                    self.pending,
                    Some(t) if (t.pane, t.row, t.col) == (pane, row, col)
                ) {
                    let seq = self.next_seq;
                    self.next_seq += 1;
                    self.pending = Some(LinkTarget {
                        pane,
                        row,
                        col,
                        seq,
                        deadline: now + LINK_HOVER_DEBOUNCE,
                        fired: false,
                    });
                    self.accepted = None;
                }
            }
            None => {
                self.pending = None;
                self.accepted = None;
            }
        }
    }

    /// A new frame for `pane` invalidated whatever was accepted: clear the
    /// span and restart the quiet period from this frame, so the probe waits
    /// for the pane to go quiet again and repeated streaming frames keep
    /// postponing the lookup instead of scanning at frame cadence.
    fn on_frame(&mut self, pane: u64, now: Instant) {
        if let Some(t) = &mut self.pending {
            if t.pane == pane {
                t.seq = self.next_seq;
                self.next_seq += 1;
                t.deadline = now + LINK_HOVER_DEBOUNCE;
                t.fired = false;
                self.accepted = None;
            }
        }
    }

    /// Take the probe that is due: the pending target whose quiet period
    /// ended and has not fired. Marks it fired so a resting pointer never
    /// re-sends.
    fn take_due_probe(&mut self, now: Instant) -> Option<LinkTarget> {
        let t = self.pending.as_mut()?;
        if t.fired || now < t.deadline {
            return None;
        }
        t.fired = true;
        Some(*t)
    }

    /// Fold a reply in: accept it only when its pane and seq still identify
    /// the current target. A stale reply (pointer moved, or a frame
    /// invalidated and re-sequenced the target) is dropped whole; a current
    /// reply installs the span, or clears it when the cell is no link.
    fn on_reply(&mut self, pane: u64, seq: u64, cells: Vec<(u16, u16)>) -> bool {
        let current = self.pending.is_some_and(|t| t.pane == pane && t.seq == seq);
        if !current {
            return false;
        }
        self.accepted = (!cells.is_empty()).then_some((pane, cells));
        true
    }

    /// The next wake for the probe clock, when one is pending.
    fn deadline(&self) -> Option<Instant> {
        self.pending.filter(|t| !t.fired).map(|t| t.deadline)
    }

    /// Drop the target and the underline (pointer left the panes, or a popup
    /// took the mouse). Never sends anything.
    fn clear(&mut self) {
        self.pending = None;
        self.accepted = None;
    }
}

/// How long a seam drag survives with no motion before it expires (x-d807,
/// AC7-FR). A drag whose mouse-up never arrives - the terminal lost focus
/// mid-gesture, or the release was eaten - would otherwise stay latched and
/// swallow every later mouse event. Generous compared to [`HOVER_DEBOUNCE`]
/// because a human pausing mid-drag to look at the layout is ordinary; this is
/// a stuck-state backstop, not a gesture timer.
const SEAM_DRAG_TIMEOUT: Duration = Duration::from_secs(5);

/// (x-aa95) The pane grip: a small mark at top-center saying "this pane can be
/// moved". Middle dots rather than ASCII periods so it reads as a handle and
/// not as truncated text, and three cells so it is findable without eating a
/// meaningful slice of a narrow pane's title row.
const GRIP: &str = "···";

/// (x-aa95) How long a relocation drag survives with no motion before it
/// expires. Same backstop role - and so the same duration - as
/// [`SEAM_DRAG_TIMEOUT`]: a swallowed mouse-up must not leave the client
/// latched, silently eating every later click.
const PANE_DRAG_TIMEOUT: Duration = SEAM_DRAG_TIMEOUT;

/// (x-7683) How long a Left press must hold, with no drag, before its release
/// opens the context menu instead of the click action - the no-config path for
/// terminals that never forward a right-click (Terminal.app, an unconfigured
/// iTerm2, tmux with mouse on). Long enough that an ordinary click never
/// triggers it; short enough that a deliberate hold does not feel like a wait.
/// Release-fired (not hold-fired) so no timer runs while the button is down.
/// `pub(crate)`: every user-facing mention of the hold (the keys-modal note,
/// the meta-row label in `keys.rs`) formats from this one constant, so a
/// retune can never leave the help advertising a stale duration.
pub(crate) const MENU_LONG_PRESS: Duration = Duration::from_millis(500);

/// (x-7683) The one long-press qualification rule, shared by the tab release
/// arm, the row release arm, and the dead-drag reaper: a hold that never
/// moved, past the threshold. One definition so the three surfaces can never
/// disagree about what a hold is.
fn held_long_enough(start: Instant, moved: bool) -> bool {
    !moved && Instant::now().duration_since(start) >= MENU_LONG_PRESS
}

/// Run the client for `session`. Returns the process exit code.
pub fn run(session: &str) -> i32 {
    match run_inner(session) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fno: {e}");
            1
        }
    }
}

fn run_inner(session: &str) -> Result<i32, String> {
    // Resolve + record the config warning BEFORE any early exit below (the
    // nested-session guard, an invalid session name): a pinned config whose
    // dir diverged must say so on every path, not only the happy attach. The
    // write rides the client log, never stderr - we are pre-alternate-screen,
    // and any stderr byte lands in the PTY the harness is about to read as
    // the TUI (the x-0296 NEVER-stderr rule). The mux dir is ensured first:
    // on a fresh state root nothing creates it until connect_or_spawn, and an
    // append to a missing parent silently drops the warning.
    let _ = proto::mux_dir();
    if let Some((w, _remedy)) = proto::pending_config_warning() {
        let _ = proto::ensure_mux_dir();
        client_log_append(&proto::mux_dir().join("client-warnings.log"), w);
    }
    // Nested same-session guard (AC3-UI/EDGE): BEFORE any socket, spawn, or
    // terminal mode change. `FNO_SESSION` is set in every pane the server
    // spawns, so target == env means "attaching to the session I am already
    // inside" - an instant hall of mirrors. Different-session nesting is
    // allowed (the flag already beat the env in resolution).
    if std::env::var("FNO_SESSION").ok().as_deref() == Some(session) {
        return Err(format!(
            "already inside mux session {session:?} (FNO_SESSION is set). \
             Attach to another session with `fno --session <other>`, or \
             `unset FNO_SESSION` if this shell is not really inside a pane."
        ));
    }
    let path = proto::socket_path(session)?;

    let stream = connect_or_spawn(&path)?;

    let runtime = tokio::runtime::Runtime::new().map_err(|e| format!("runtime: {e}"))?;
    runtime.block_on(attach_and_run(stream, &path))
}

/// Append one line to a log file under the mux dir, best-effort. The shared
/// write behind both the e2e breadcrumbs and the config-warning log: one
/// append mechanism, never stderr (see [`e2e_client_log`] for the rule).
fn client_log_append(path: &Path, msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{msg}");
    }
}

/// Connect to a live server, or spawn one and connect. AC3-ERR: a dead
/// server's stale socket gets a one-line notice and a fresh server - never a
/// hang on a dead socket (the spawned server's bind unlinks it). Shared with
/// `mux_cli::pane run`, which must self-spawn a server for a script-only
/// session (AC1-EDGE).
pub(crate) fn connect_or_spawn(path: &Path) -> Result<std::os::unix::net::UnixStream, String> {
    // spawn_server opens a log file in the mux dir, so the dir must exist first.
    // pane run reaches here without going through run_inner's ensure (AC1-EDGE).
    proto::ensure_mux_dir().map_err(|e| format!("cannot prepare the mux dir: {e}"))?;
    match proto::connect_unix_timeout(path, ATTACH_CONNECT_TIMEOUT) {
        Ok(s) => {
            e2e_client_log(format_args!(
                "connected to live server at {}",
                path.display()
            ));
            return Ok(s);
        }
        // A connect timeout means something holds the socket but never
        // accepted: a wedged server. Spawning over it would just lose the
        // bind race, so report instead - never hang, never clobber.
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            return Err(format!(
                "server at {} is not accepting connections (connect timed out); it is \
                 wedged. Run `fno mux kill-server` for this session: it escalates to \
                 SIGTERM/SIGKILL and unlinks the socket (the server's log is at {}), \
                 then retry.",
                path.display(),
                log_path(path).display()
            ));
        }
        Err(e) => {
            e2e_client_log(format_args!("connect failed ({e}); spawning a server"));
        }
    }
    if path.exists() {
        eprintln!("fno: previous session ended; starting a fresh one");
    }
    spawn_server(path)?;
    let deadline = Instant::now() + SPAWN_CONNECT_TIMEOUT;
    loop {
        match proto::connect_unix_timeout(path, ATTACH_CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) if Instant::now() >= deadline => {
                return Err(format!(
                    "server did not come up at {} ({e}); check {}",
                    path.display(),
                    log_path(path).display()
                ));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(30)),
        }
    }
}

fn log_path(socket: &Path) -> PathBuf {
    socket.with_extension("log")
}

/// x-0296 CI diagnostics: connect-path breadcrumbs, FNO_E2E-gated, appended
/// to `<mux_dir>/client-<pid>.log` (the e2e harness dumps every `*.log` in
/// its scratch on a timeout). NEVER stderr: pre-TUI stderr reaches the
/// client's PTY, and any byte there trips the harness's screen-not-empty
/// gates before the client has actually attached.
fn e2e_client_log(msg: std::fmt::Arguments<'_>) {
    if std::env::var_os("FNO_E2E").is_none() {
        return;
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!("[{ms} pid {}] {msg}", std::process::id());
    client_log_append(
        &proto::mux_dir().join(format!("client-{}.log", std::process::id())),
        &line,
    );
}

/// Spawn `fno --server <socket>` detached: its own session (setsid) so the
/// server never receives the terminal's SIGHUP, stderr to a per-session log.
/// Two clients racing here both spawn; the bind is the lock, the losing
/// server exits 0, and both clients attach to the winner (AC4-EDGE).
fn spawn_server(path: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find own binary: {e}"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(path))
        .map_err(|e| format!("cannot open server log: {e}"))?;
    let mut cmd = crate::process_admission::std_command(exe);
    cmd.arg("--server")
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    // Config->env bridge for the interactive path (x-6165). The pure-Rust mux
    // server reads no config.toml, so `config.mux.shell_integration: off` was
    // a silent no-op here (the Python spawn front-half already bridges
    // dispatched panes, x-b63b). Latch it at server birth: an explicit env
    // export wins (inherited naturally, never overwritten); otherwise a single
    // bounded `fno config get` decides. Only `off` needs materializing - the
    // server reads absent/anything-else as on (the default).
    if std::env::var_os("FNO_MUX_SHELL_INTEGRATION").is_none() && shell_integration_off() {
        cmd.env("FNO_MUX_SHELL_INTEGRATION", "off");
    }
    // Same bridge, same reason, for the backlog board's project scope (x-20f1).
    // Resolving it needs `fno config get`, and the SERVER must not shell out on
    // its startup path: doing so delayed shutdown past the SIGTERM grace and
    // perturbed multiclient frame ordering. The client already pays a bounded
    // config read here, so the resolution happens once, in this process, and
    // rides in on the env. An explicit export wins, inherited untouched.
    if std::env::var_os("FNO_BOARD_SCOPE").is_none() {
        let (scope, _why) = crate::backlog_view::resolve_board_scope(crate::server::config_get);
        cmd.env(
            "FNO_BOARD_SCOPE",
            crate::backlog_view::board_scope_wire(&scope),
        );
    }
    // Safety: setsid only detaches the child from our session/terminal; it is
    // async-signal-safe and touches no shared state.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    crate::process_admission::std_spawn(&mut cmd)
        .map(|_| ())
        .map_err(|e| format!("cannot spawn the mux server: {e}"))
}

/// Whether the interactive path must disable OSC 133 injection. Bounded +
/// fail-open through [`crate::server::config_get`]: any spawn/read error, a
/// non-`off` value, or a read that overruns the budget all leave injection on
/// (the default). The bound matters because this runs synchronously inside
/// `spawn_server`, *before* the client's spawn-connect wait loop exists -
/// nothing downstream would rescue an unbounded read, so a slow or wedged
/// config read would freeze `fno` startup with no notice.
fn shell_integration_off() -> bool {
    crate::server::config_get("mux.shell_integration")
        .as_deref()
        .map(config_says_off)
        .unwrap_or(false)
}

/// The one off-switch, matched exactly like the Rust pane-spawn side
/// (`pty::integration_disabled`): only a trimmed `off` disables injection.
fn config_says_off(stdout: &str) -> bool {
    stdout.trim() == "off"
}

/// Restore the terminal on every exit path, including panics.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, String> {
        terminal::enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
        let mut out = std::io::stdout();
        // Surface an alt-screen failure instead of silently painting over the
        // user's scrollback. The guard exists from here, so raw mode is
        // restored by Drop on the error path.
        let guard = TerminalGuard;
        crossterm::execute!(out, terminal::EnterAlternateScreen)
            .map_err(|e| format!("alternate screen: {e}"))?;
        // Mouse capture stays on for the client's whole life (US1/US2/US3): the
        // server routes every pane-rect event by the pane's live mode. Drop's
        // MODE_RESET (which lists 1000/1002/1006 off) turns it back off on exit.
        out.write_all(crate::mouse::ENABLE)
            .and_then(|_| out.flush())
            .map_err(|e| format!("enable mouse: {e}"))?;
        Ok(guard)
    }
}

/// Every DEC/private mode `ModeSync` can set, reset. Emitted unconditionally
/// on exit (codex P2): a focused vim's mouse reporting or bracketed paste
/// must never survive onto the user's real terminal after `fno` exits, and
/// tracking exactly-what-was-set buys nothing over resetting the fixed set
/// `vt::mode_diff` can emit. Unknown sequences (kitty CSI-u on a plain
/// terminal) are ignored by terminals by design.
const MODE_RESET: &[u8] =
    b"\x1b[?1l\x1b>\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1007l\x1b[?2004l\x1b[=0;1u";

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(MODE_RESET);
        let _ = crossterm::execute!(out, terminal::LeaveAlternateScreen, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

// ---------------------------------------------------------------------------
// View state + pure composition
// ---------------------------------------------------------------------------

/// An agent row's hosting-tab context, resolved inside-out (x-0f9d US3): a
/// chosen tab name when the tab is named, else its `·N` ordinal.
enum TabContext {
    Named(String),
    Ordinal(usize),
}

/// A draggable seam between two panes, addressed by the panes flanking it:
/// `a` is the left/top pane, `b` the right/bottom one. `axis` is the branch's
/// axis, so `Horizontal` (children side by side) means a vertical divider line.
///
/// The pair addresses one branch child pair, not one pane pair: a seam can run
/// past several panes on either side, and naming any pane from each side picks
/// out the same two branch children (a same-axis branch never nests, so every
/// descendant of a child shares that child's extent along the branch axis).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Seam {
    a: u64,
    b: u64,
    axis: Axis,
}

/// A seam drag in flight. `start_pos` is where the divider sat when the drag
/// began, kept for the Esc revert; `last_pos` suppresses duplicate commands
/// between cell-boundary crossings.
#[derive(Clone, Copy, Debug)]
struct SeamDrag {
    seam: Seam,
    start_pos: u16,
    last_pos: u16,
    last_at: Instant,
}

/// (x-2e86) A sideline right-border drag in flight. `start_width` is the width
/// at grab, so a bare Esc reverts to it (mirroring [`SeamDrag::start_pos`]);
/// `last_at` refreshes on every column of motion so a swallowed mouse-up expires
/// via the same stuck-drag timeout seam and pane drags use (Locked 9).
#[derive(Clone, Copy, Debug)]
struct SidelineDrag {
    start_width: u16,
    last_at: Instant,
}

/// (x-aa95) Where a drop would put the dragged pane: adjacent to `target` on
/// its `dir` side.
///
/// A seam and the outer edge beside it collapse to the same shape on purpose -
/// "between C and D" is "after C", and "along the left edge" is "left of the
/// leftmost pane there" - so the drop path has one vocabulary and `move_leaf`
/// has one entry point, which is what keeps drag and keyboard genuinely
/// identical rather than merely similar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct DropZone {
    target: u64,
    dir: Dir,
}

/// (x-aa95) A pane relocation drag in flight.
///
/// `zone` is the candidate under the pointer, recomputed per motion report and
/// kept here so the renderer can light it without re-hit-testing. Purely
/// client-local: nothing is sent until the release, so a drag that dies mid-air
/// leaves the server's tree untouched.
#[derive(Clone, Copy, Debug)]
struct PaneDrag {
    mover: u64,
    zone: Option<DropZone>,
    /// (v43, x-d6a8 G1) The pointer is over the tab strip: the drop breaks the
    /// pane into its own new tab (`Command::BreakPane`) instead of relocating it
    /// within the tab (`Command::MovePane`). Mutually exclusive with `zone` - a
    /// cell is either the strip or the content area, never both.
    on_strip: bool,
    last_at: Instant,
}

/// (v43, x-d6a8 G2) A tab-cell relocation drag in flight: picking a whole tab up
/// from the strip to graft it into the current tab's content as a split
/// (`Command::JoinTab`). Mirrors [`PaneDrag`]'s lifetime (ghost, timeout reaper,
/// esc-cancel); `zone` is the content-edge candidate under the pointer.
#[derive(Clone, Copy, Debug)]
struct TabDrag {
    src_tab: u64,
    zone: Option<DropZone>,
    last_at: Instant,
    /// (x-7683) When the press began - the long-press clock, unlike `last_at`
    /// which motion refreshes.
    start_at: Instant,
    /// (x-7683) Whether any drag report arrived: a long-press needs a hold
    /// with NO motion, and the clock alone cannot tell a hold from a slow
    /// drag that ends zone-less.
    moved: bool,
}

/// (x-10ec) The workspace peek body: everything the layout already holds for
/// one squad - its origin, its tabs (the active one marked), and its member
/// rows with their states. Pure and local; no wire round trip.
fn squad_peek_lines(layout: &LayoutView, sid: u64) -> Vec<String> {
    let Some(s) = layout.squads.iter().find(|s| s.id == sid) else {
        return vec!["workspace is no longer here".into()];
    };
    let mut out = vec![
        format!("origin  {}", s.canonical_cwd),
        format!("tabs    {} · panes {}", s.tabs.len(), s.panes),
    ];
    for (i, t) in s.tabs.iter().enumerate() {
        let marker = if i == s.active_tab { "*" } else { " " };
        let label = if t.name.is_empty() {
            format!("tab {}", i + 1)
        } else {
            t.name.clone()
        };
        out.push(format!("{marker} {label}"));
    }
    let members: Vec<&AgentRow> = layout
        .agents
        .iter()
        .filter(|a| a.squad == Some(sid))
        .collect();
    if members.is_empty() {
        out.push("members none".into());
        return out;
    }
    out.push("members".into());
    for a in members {
        let state = if a.exited {
            "exited"
        } else {
            // The one state vocabulary (pane_state + the nav filter words): a
            // finished-but-unseen member is `done`, not `idle`.
            match pane_state(a.badge, a.seen, a.pane_activity) {
                PaneState::Blocked => "blocked",
                PaneState::Working => "working",
                PaneState::DoneUnseen => "done",
                // NOT "unread" (x-d401): that word names output nobody has
                // looked at, which is `DoneUnseen` - a different row, filed
                // under "done". A filter typed as `unread` would then return
                // rows with no liveness reading and EXCLUDE every row that
                // actually has unseen output, which is this branch's own
                // defect: a word standing in for a fact it does not name.
                PaneState::Unmeasured => "unmeasured",
                PaneState::Idle => "idle",
                PaneState::Empty => "empty",
            }
        };
        let pane = a
            .pane_id
            .map(|p| format!(" · pane {p}"))
            .unwrap_or_default();
        out.push(format!("  {} · {state}{pane}", a.name));
    }
    out
}

/// (v43, x-d6a8 G3) The drag source of a sideline agent row.
#[derive(Clone, Debug, PartialEq, Eq)]
enum RowSource {
    /// A pane-hosted row: the drop moves its pane into the current tab's content,
    /// a cross-tab `Command::MovePane` (the pane lives in another tab).
    Pane(u64),
    /// A paneless bg row: the drop attaches it at the drop slot
    /// (`Command::AttachAgent` with a placement).
    Attach(String),
}

/// (v43, x-d6a8 G3) A sideline-row placement drag in flight. Reuses the
/// content-area zone vocabulary; `RowSource` carries whether the drop moves a
/// live pane or attaches a paneless session. Not `Copy` (the attach id is owned).
#[derive(Clone, Debug)]
struct RowDrag {
    src: RowSource,
    zone: Option<DropZone>,
    last_at: Instant,
    /// (x-7683) When the press began - the long-press clock, unlike `last_at`
    /// which motion refreshes.
    start_at: Instant,
    /// (x-7683) Whether any drag report arrived: a long-press needs a hold
    /// with NO motion, and the clock alone cannot tell a hold from a slow
    /// drag that ends zone-less.
    moved: bool,
}

/// The last `Layout` as the client holds it.
#[derive(Clone)]
struct LayoutView {
    squads: Vec<SquadMeta>,
    active_squad: u64,
    panes: Vec<(u64, Rect)>,
    focus: u64,
    /// The clamped content-area the rects were computed for; a client whose
    /// own content area is larger letterboxes (3.5).
    area: (u16, u16),
    /// Sideline agent rows (4a-G2): registry-derived, fact-badged, rendered
    /// under their squads (display-only; never selectable).
    agents: Vec<AgentRow>,
    /// (v10) The focused pane's `FNO_NODE` provenance, for the status-row
    /// `⚑ <node>` cell (x-66e8). `None` for an ad-hoc pane.
    focus_node: Option<String>,
    /// (v11, x-6f77) Board-ordered work-queue cards for the sideline backlog
    /// lane; empty when the graph is unreadable or has no ready/blocked/in-flight
    /// work (the lane then renders nothing - the agents section is unaffected).
    backlog: Vec<BacklogCard>,
    /// (v36, x-1d91) The UNCAPPED per-lane queue-card counts, feeding the
    /// section's exact `+N more` and the mini-kanban's lane headers.
    backlog_lanes: Vec<(String, usize)>,
    /// (v36, x-1d91) `backlog` is last-known rather than current - the graph read
    /// has been failing. Rendered as a header marker; the cards still show (a
    /// blank section would be worse than an honestly-labelled stale one).
    backlog_stale: bool,
}

/// One selectable sideline row: a squad, or one of its tabs when expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelRow {
    squad: u64,
    tab: Option<usize>,
}

/// A live pane badged `blocked` - the answer-queue membership test (x-c929).
/// Shared by every queue read so counting/emptiness checks never clone the rows.
fn is_blocked_row(a: &AgentRow) -> bool {
    !a.exited && a.badge == Some(AgentBadge::Blocked)
}

/// Everything the client renders from. Pure state - `compose` turns it into
/// one full-terminal `Frame` the row-diffing `Compositor` draws.
struct View {
    term: (u16, u16), // full terminal (rows, cols)
    /// The session name, for the status row. Fixed for the connection's life
    /// (sessions cannot rename), so the row can never go stale.
    session: String,
    layout: LayoutView,
    frames: HashMap<u64, Frame>,
    /// Manual sideline toggle; narrow terminals override it (auto-hide).
    /// Orthogonal to [`View::density`]: this is visibility, that is how much
    /// each visible row shows. `Slim` is a narrow rail, NOT a hidden panel.
    panel_on: bool,
    /// (x-b186) Sideline density, persisted by [`crate::view_store`]. Since
    /// x-2e86 it drives only the row set (via [`View::display_rows`]) and the
    /// preset width jump - the rendered width is [`View::sideline_width`], an
    /// independent value the border drag sets freely.
    density: Density,
    /// (x-2e86) The operator's chosen sideline width in columns (the stored
    /// intent), independent of the density. Resolved once at startup from the
    /// persisted width, or the density's canonical width when none is stored.
    /// [`View::panel_w`] clamps it to the terminal transiently; this value is
    /// never overwritten by that clamp, so a terminal shrink-then-grow restores
    /// it (AC1-FR).
    sideline_width: u16,
    /// (x-b186) Extended-table row order, persisted alongside the density.
    /// Inert in the other two densities (they render no table).
    agent_sort: AgentSort,
    /// Manual status-row toggle (prefix+s). Client-local and deliberately
    /// unpersisted: a reattach resets to on (AC4-FR).
    status_on: bool,
    /// The whole-machine resource meter, rendered in the status row. Off by
    /// default: it needs `macmon` on PATH, which fno core does not depend on.
    resource_meter_on: bool,
    /// The meter's latest one-line reading, or None before the first sample
    /// (and after a failed one - the row then says the sensor is unavailable
    /// and shows no number).
    resource_meter_text: Option<String>,
    /// Sample cadence, latched from config at startup (default 5s).
    resource_meter_refresh: u64,
    /// The sampler task's stop flag: toggling the meter off (or dropping the
    /// client) flips it and the task exits on its next wake.
    resource_meter_gate: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// One-shot: set when the toggle turns the meter on, cleared by the run
    /// loop after it spawns the sampler, so a toggle spawns exactly one task.
    resource_meter_sampling: bool,
    /// The which-key hint line is painted over the bottom row (prefix held
    /// past [`HINT_DELAY`]); any chord resolution clears it (AC4-HP).
    hint: bool,
    /// The which-key keybinds modal (prefix+?, x-8ccf US3): a centered popup
    /// built from the prefix-chord table. While open, a bound key executes
    /// through the SAME dispatch as a direct chord (which-key); arrows/pgup
    /// scroll+select, Enter runs the selected row, Esc/unbound closes. `None`
    /// when closed. Replaces the old static top-left key-table poster.
    keys_modal: Option<KeysModal>,
    /// Held prefix+backslash reveal deadline. Client-local and transient: it
    /// follows whichever pane layout the server last supplied.
    pane_ids_until: Option<Instant>,
    /// Pending escape bytes in modal mode (arrow/pgup folding), same split-arrow
    /// safety as [`View::sel_esc`].
    keys_modal_esc: Vec<u8>,
    /// (x-8ccf US2) The right-click / `m` row context menu over a sideline agent
    /// row: an anchored popup whose entries route to existing commands. `None`
    /// when closed. The target is pinned by name so a layout reshuffle can only
    /// stale-refuse an action, never redirect it.
    row_menu: Option<RowMenu>,
    /// Pending escape bytes in row-menu mode (arrow folding).
    row_menu_esc: Vec<u8>,
    /// (x-8ccf US4/US5) The sideline MENU popup or the settings modal (they share
    /// one slot; MENU chains into settings). `None` when closed.
    aux: Option<AuxPopup>,
    /// Pending escape bytes in aux-popup mode (arrow folding).
    aux_esc: Vec<u8>,
    /// (x-975a) Per-section view state - client-local, instant (AC6-UI), and
    /// persisted across restarts by [`crate::view_store`]. Keyed by squad NAME
    /// (not the ephemeral session id) so a restart restores the same sections.
    /// An absent squad key reads as [`SectionView::Collapsed`], an absent
    /// fixed section as `Expanded` - see [`View::section_view`].
    section_view: HashMap<SectionKey, SectionView>,
    /// (x-975a) The subset of [`View::section_view`] the operator EXPLICITLY
    /// chose, and the only thing that reaches disk. A seeded default is
    /// recomputed on every attach, so persisting it would let this build
    /// re-seed over a value a NEWER build wrote and this one could not parse.
    section_chosen: HashMap<SectionKey, SectionView>,
    /// (x-c5ee) Squads the operator has expanded past the top-K idle cap for
    /// THIS session - a transient "show me all the idle rows", not a durable
    /// preference (that layer is [`SectionView`]). Ephemeral by design: dropped
    /// on restart, and a dead-squad key left in it is inert, so it never needs
    /// pruning.
    idle_expanded: HashSet<SectionKey>,
    /// Selector cursor into [`View::display_rows`], when open (x-260a: one
    /// index space shared with painting, hover, and mouse hit-testing).
    selector: Option<usize>,
    /// Pending escape bytes in selector mode, carried ACROSS reads so a
    /// split arrow sequence can never half-close the selector and leak its
    /// tail into the pane (gemini medium).
    sel_esc: Vec<u8>,
    /// (x-f331) The [`View::selector`] was armed by a pointer resting in the
    /// panel, not by an explicit `prefix+w`. Pointer-in-panel arms the selector
    /// so `x`/`X`/`r`/`space` act on the pointed-at row (one regime, closes the
    /// old PTY leak where a bare `x` fell through to the focused pane). A
    /// hover-arm is motion-fresh: only the action-verb set acts on it, and the
    /// first key OUTSIDE that set disarms and forwards, so a pointer parked over
    /// the sideline never swallows typing into the focused pane (AC2-EDGE).
    sel_hover_armed: bool,
    /// (x-a621) First-visible [`View::display_rows`] index in the sideline:
    /// follow-the-cursor scroll offset so rows below the fold render and take
    /// the mouse. 0 (top-anchored) whenever the catalog fits the height.
    sideline_offset: usize,
    /// Answer-overlay cursor into [`View::blocked_queue`] (x-c929), when open;
    /// the index of the selected blocked pane in `Layout.agents` order.
    answers: Option<usize>,
    /// Pending escape bytes in answer-overlay mode (same split-arrow safety as
    /// [`View::sel_esc`]).
    ans_esc: Vec<u8>,
    /// (x-feec) The event-derived needs-me leg: the last `fno-agents needs` fold
    /// result while the overlay is open (`None` = live-only, not yet fetched
    /// this open). Merged with the live badge leg by [`View::needs_queue`].
    needs_fold: Option<Vec<crate::needs_overlay::FoldItem>>,
    /// Operator-owned priorities folded independently from the event lane.
    mine_fold: Option<Vec<crate::needs_overlay::MineItem>>,
    /// (x-feec) When `needs_fold` was last fetched, for the short re-open cache.
    needs_fold_at: Option<Instant>,
    /// (x-feec) The last fold shell-out failed/timed out: render the loud
    /// degraded notice (AC2-ERR) instead of a silent partial queue.
    needs_degraded: bool,
    /// The MINE command failed/timed out; the operator lane degrades visibly
    /// without hiding THEY NEED YOU.
    mine_degraded: bool,
    /// (x-f730 task 2.2) The MINE add-line text buffer: `Some(text)` while the
    /// `a` mini text-entry is open inside the needs overlay (appended below
    /// the MINE lane), `None` when closed. Owns the keyboard ahead of the
    /// normal overlay keys so a typed letter never triggers cycle/answer.
    mine_adding: Option<String>,
    /// (x-f730 task 2.2) A MINE mutation queued by the stdin handler for the
    /// run loop to shell out (kept out of the deep stdin handler, mirrors
    /// `conn_action`); `mine_acting` bounds it to one in flight so a second
    /// x/d/add press cannot race the first write.
    mine_action: Option<crate::needs_overlay::MineMutation>,
    mine_acting: bool,
    /// (x-f730 task 2.3) Open operator questions, folded independently
    /// (`fno inbox outstanding --json`, the richer x-7979 record - asker,
    /// options, resolved liveness) from both the MINE lane and the bare
    /// events leg. Rendered as its own row kind, ranked ahead of the rest of
    /// THEY NEED YOU.
    questions_fold: Option<Vec<crate::needs_overlay::QuestionItem>>,
    /// The questions command failed/timed out; same degrade contract as
    /// `mine_degraded`/`needs_degraded`.
    questions_degraded: bool,
    /// (x-f730 task 2.3) The free-text answer buffer for a no-options
    /// question: `Some((question_id, text))` while open, `None` when closed.
    /// Same keyboard-ownership contract as `mine_adding`.
    question_answering: Option<(String, String)>,
    /// (x-f730 task 2.3) A queued question answer, mirroring
    /// `mine_action`/`mine_acting` exactly (its own single-flight guard - a
    /// question answer and a MINE write are independent, so one in flight
    /// never blocks the other).
    question_action: Option<(String, String)>,
    question_acting: bool,
    /// (x-feec) Set by OpenAnswers when a fresh fold is wanted; the run loop
    /// spawns the shell-out and clears it, keeping the channel sender out of the
    /// deep stdin handler.
    needs_want: bool,
    /// (x-feec) A fold shell-out is running; bounds concurrent folds to one so
    /// mashing prefix+a on a stale cache cannot spawn a pile of children (P2-5).
    needs_inflight: bool,
    /// (x-feec) Generation token, bumped on every open/close so a fold result
    /// landing after the overlay closed or re-opened is discarded (AC6-FR).
    needs_gen: u64,
    /// (x-b2bf) The yard overlay, when open. The cursor indexes the
    /// roster-derived crowd; the spotlight renders that citizen's sprite,
    /// and `opened_at` drives frame cycling (a flavour channel - it carries
    /// no reading, so a timer is legal where it would not be for the eye).
    yard: Option<YardSel>,
    /// Pending escape bytes in yard-overlay mode (same split-arrow safety as
    /// [`View::ans_esc`]).
    yard_esc: Vec<u8>,
    /// (x-b2bf) The identity fold's last result while the overlay is open
    /// (`None` = not yet fetched this open). Species/rarity/crown/
    /// first-sighting only - no status field, ever: the eye is derived from
    /// the row's own badge/need values so the sprite cannot disagree.
    yard_fold: Option<Vec<crate::yard_overlay::YardItem>>,
    yard_fold_at: Option<Instant>,
    yard_degraded: bool,
    yard_want: bool,
    yard_inflight: bool,
    yard_gen: u64,
    /// (x-3cb3) The court panel. The whole thing - open flag, cached fold,
    /// generation - lives in its own module, so this is one field.
    court: crate::court_overlay::Panel,
    /// Catch-up "while you were gone" digest lines (x-4e2d), set on attach after
    /// an absence; the next keypress dismisses it (like [`View::overlay`]).
    digest: Option<Vec<String>>,
    notice: Option<(String, Instant)>,
    /// (x-f191) The row-scoped outcome stamp and its armed action, one at a
    /// time - see [`RowStamp`] / [`RowArm`].
    row_stamp: Option<RowStamp>,
    row_arm: Option<RowArm>,
    /// (x-f191 scope a+c) The display slot a row-scoped confirm was armed
    /// from, captured at arm time (the confirm clears the selector). The
    /// commit re-anchors onto the row's identity; a row the action removed
    /// falls to this slot, clamped - the neighbour, never a reset.
    row_slot: Option<usize>,
    /// (v12, x-e780) Active in-scrollback search (prefix+/), when open. While
    /// `Some`, stdin diverts to [`search_keys`] and the bottom chrome shows the
    /// input line / counter. Client-local: opening never sends a message and
    /// never reserves a row (no Resize -> no reflow -> no dropped highlight).
    search: Option<SearchView>,
    /// Pending escape bytes in search mode, carried ACROSS reads so a split
    /// arrow sequence can never half-close the search or leak its tail into the
    /// pane (same split-arrow safety as [`View::sel_esc`]).
    search_esc: Vec<u8>,
    /// (x-1d91) The one dispatched-but-unconfirmed Backlog reorder verb, if any.
    /// At most one: the marker doubles as the double-press guard, so a second
    /// dispatch on the same card cannot fire until the first resolves.
    backlog_pending: Option<BacklogPending>,
    /// (x-a496) `config.mux.hover_focus`: focus-follows-mouse over panes.
    /// Latched once at startup (default on); false disables the hover pre-pass.
    hover_focus: bool,
    /// `config.obsidian.*`, latched once at startup like the toggles above.
    /// Feeds the backlog card menu's open-plan item (`link::plan_link`); never
    /// re-read mid-session, matching every other startup-latched config value.
    obsidian: crate::digest_overlay::ObsidianCfg,
    /// `config.mux.show_missions` / `config.mux.show_backlog` (default on): drop
    /// the `~ missions` progress band or the `~ backlog` lane entirely. Latched
    /// once at startup; an operator who runs no epics hides the empty band rather
    /// than collapsing it each session.
    show_missions: bool,
    show_backlog: bool,
    /// (x-f75e) `config.mux.theme`: the chrome palette. Latched once at startup
    /// from the same config ladder `hover_focus` reads, and swapped in memory on
    /// an explicit apply from the settings modal. `terminal` (the default)
    /// inherits the emulator's own colors so every pre-theme render is
    /// byte-identical.
    theme: Theme,
    /// (x-f75e) Which settings tab is in front (general toggles / theme picker).
    settings_tab: SettingsTab,
    /// (x-a496) Focus-follows-mouse debounce: the pane the pointer is settling on
    /// and when it first landed there. `FocusPane` fires once the same pane holds
    /// for [`HOVER_DEBOUNCE`]; a different pane or chrome resets it.
    hover_pending: Option<(u64, Instant)>,
    /// (hover affordance) The link-probe clock and accepted underline span.
    /// Independent of [`View::hover_focus`]: the link affordance tracks the
    /// exact cell, and a focus-follows-mouse off-switch disables focus
    /// stealing, not hover affordances.
    link_hover: LinkHoverState,
    /// (x-a496) The `display_rows()` index the pointer is hovering in the
    /// sideline, painted with the selector's INVERSE bar. Highlight-only - never
    /// switches the viewed squad/tab. `None` off the panel.
    hover_row: Option<usize>,
    /// (x-d807) The seam under the pointer, accented so a divider reads as
    /// draggable before the press. Terminals cannot portably change the cursor
    /// shape, so the accent is the whole affordance.
    hover_seam: Option<Seam>,
    /// (x-d807) The seam drag in flight, if any. While `Some`, drag and release
    /// reports are intercepted before they reach a pane's PTY.
    seam_drag: Option<SeamDrag>,
    /// (x-d807) True while the pointer is over the sideline's right border, and
    /// the border-drag in flight. The sideline stays client-local (never on the
    /// wire); the drag sets a free width (x-2e86, reversing x-b186's snap-only).
    hover_sideline_border: bool,
    sideline_drag: Option<SidelineDrag>,
    /// (x-aa95) The pane whose grip the pointer is over, accented so the grip
    /// reads as grabbable before the press - the same "no portable cursor
    /// shape" constraint that makes [`View::hover_seam`] the whole affordance.
    hover_grip: Option<u64>,
    /// (x-aa95) The relocation drag in flight, if any. While `Some`, motion and
    /// release are intercepted before they reach a pane's PTY.
    pane_drag: Option<PaneDrag>,
    /// (v43, x-d6a8 G2) A tab-cell join drag in flight. Same interception as
    /// `pane_drag`; at most one of the three drags is ever live at a time.
    tab_drag: Option<TabDrag>,
    /// (v43, x-d6a8 G3) A sideline-row placement drag in flight.
    row_drag: Option<RowDrag>,
    /// (x-b465) A press being held on a sideline row that is NOT a drag source -
    /// a workspace name row, a section header. Those rows carry menus
    /// (`open_row_menu` builds one for a squad row), but the long-press arm that
    /// opens a menu lives inside the `row_drag` branch, and `row_drag_source_at`
    /// answers only for agent rows. So a hold on a workspace row armed no state
    /// and the gesture did nothing at all.
    ///
    /// Deliberately NOT a widening of `row_drag_source_at`: a workspace row is
    /// not a placement source, and making it one would offer a drag with nowhere
    /// to land. This is the hold clock and the pressed row, nothing else.
    ///
    /// `(display_row, identity, pressed_at)`. The identity is load-bearing: an
    /// index alone is not a row. `display_rows()` is rebuilt on every layout
    /// push, so a row dropped during the hold slides a DIFFERENT row under the
    /// same number, and the release would open that row's lifecycle menu - Stop
    /// and Remove pointed at a worker nobody pressed. Re-checked at release
    /// against [`View::row_identity`], the same discipline `RowDrag` gets from
    /// `RowSource` and the selector gets from its name re-anchor.
    press_hold: Option<(usize, String, Instant)>,
    /// (x-a496) A pending click-a-card confirm: the node to dispatch and its
    /// display label. While `Some`, keys route to the confirm (Enter dispatches,
    /// any other key cancels) and the bottom row shows the prompt.
    confirm: Option<ConfirmAction>,
    /// A left-button release paired with a click on a modal's close chip must
    /// stay swallowed after that click closes the modal.
    modal_release_swallow: bool,
    /// (x-9e5e) The pending new-workspace name buffer, `Some` while the `+`
    /// create overlay is open. Keys divert to [`create_keys`]: printable append,
    /// Backspace pops, Enter sends [`Command::NewSquad`] (empty keeps it open),
    /// Esc cancels. Client-local like `search`: opening reserves no row.
    create: Option<String>,
    /// Pending escape bytes in create-overlay mode (same split-arrow safety as
    /// [`View::search_esc`]).
    create_esc: Vec<u8>,
    /// (x-c150; widened x-96e8) The pending rename buffer: `(target captured at
    /// open, typed name)`, `Some` while the `prefix+,` (tab) or selector `r`
    /// (squad) overlay is open. Keys divert to [`rename_keys`]. Enter on an
    /// EMPTY buffer still sends (blank = clear back to the derived label),
    /// unlike `create`.
    rename: Option<(RenameTarget, String)>,
    /// (x-cf97) The pending move-to-position prompt: `(tab captured at open,
    /// typed destination)`, `Some` while the tab menu's `Move to…` entry (or
    /// `prefix+#`) has it open. Keys divert to [`move_to_keys`], the rename
    /// overlay's shape with a numeric grammar: digits/backspace edit, Enter
    /// computes the delta and sends ONE `Command::ReorderTab`, Esc cancels,
    /// and an out-of-range ordinal keeps the prompt open with a notice - it
    /// never sends a clamped guess.
    move_to: Option<(TabId, String)>,
    /// (x-e4f1) The lane-colors drill + text-entry state for the settings
    /// Colors tab. Client-local ephemera like `create`/`rename`; dormant while
    /// another tab or no popup is front, reset on tab switch away from Colors.
    lane: LaneColorsUi,
    /// Pending escape bytes in rename-overlay mode (same split-arrow safety
    /// as [`View::create_esc`]).
    rename_esc: Vec<u8>,
    /// (x-0f9d US1) Armed when a bare NewTab (`c` / the strip `+`) is
    /// dispatched: `Some(baseline)` where `baseline` is the greatest tab id in
    /// the active squad at send time, or `None` when the squad had no tabs. The
    /// layout that materializes a tab beyond that baseline opens the rename
    /// overlay on it, so a create-time name prompt reuses the x-c150 rename
    /// machinery with no new command or overlay. Guarding on the baseline (not a
    /// bare bool) keeps a routine scrape-tick layout - which can arrive before
    /// the server has processed NewTab - from arming on the wrong (old) tab. The
    /// nested Option distinguishes "no tabs before" from "max id 0", so the
    /// first tab (id 0) still triggers (gemini review).
    pending_new_tab: Option<Option<u64>>,
    /// (x-8f11) Multi-select marks for bulk recruit: the `attach_id`s toggled
    /// with `space` in the sideline selector. Client-local ephemera keyed by id,
    /// so a marked row surviving a filter/scroll keeps its mark and a vanished
    /// row simply drops it (never a stale index). Cleared on a recruit submit.
    marks: std::collections::HashSet<String>,
    /// (x-8f11) The pending recruit workspace-name buffer, `Some` while the `R`
    /// recruit overlay is open. Enter sends [`Command::RecruitAgents`] with the
    /// marked ids (empty keeps it open, like `create`); Esc cancels, marks kept.
    recruit: Option<String>,
    /// Pending escape bytes in recruit-overlay mode (split-arrow safety).
    recruit_esc: Vec<u8>,
    /// (x-96e8) The move-a-tab-to-another-squad picker: `(tab captured at open,
    /// candidate squad ids in the numbered order shown)`, `Some` while the
    /// selector `m` overlay is open. A digit sends [`Command::MoveTab`] for a tab
    /// source or [`Command::MovePane`] (cross-squad) for a pane source; the id
    /// is re-validated against the current catalog before it goes on the wire.
    move_pick: Option<MovePick>,
    /// Pending target and geometry for selector `p` placement.
    attach_place: Option<AttachPlace>,
    /// (x-9fd0) The portal-placement picker (selector `P`): the focused row's
    /// id plus a cursor. The rows are never stored - [`View::open_portal_rows`]
    /// derives them per frame.
    portal_pick: Option<PortalPick>,
    /// (x-96e8) The squad the selector cursor is tracking across a `J`/`K`
    /// reorder: the next `Layout` re-points the cursor at this squad's row so it
    /// visually follows the moved workspace. Cleared by any non-reorder key or a
    /// selector close.
    sel_follow: Option<u64>,
    /// (x-653d) The session-navigator overlay (prefix+f): a global goto picker
    /// over a flat catalog of every squad/tab/agent/card, filtered by typed text
    /// AND by agent state. `Some` while open; stdin diverts to [`nav_keys`].
    /// Client-local like `search` - opening never sends a message and reserves
    /// no row (it draws over the content top-left, not the bottom chrome).
    nav: Option<NavView>,
    /// Pending escape bytes in navigator mode, carried ACROSS reads (same
    /// split-arrow safety as [`View::search_esc`]).
    nav_esc: Vec<u8>,
    /// (x-c376) The read-only peek overlay (Space on a selector agent row),
    /// `Some` while open. Sits ON TOP of the selector; stdin diverts to
    /// [`peek_keys`] BEFORE selector routing. Client-local like `nav` - opening
    /// sends one `PeekAgent` and reserves no row.
    peek: Option<PeekView>,
    /// Pending escape bytes in peek mode (j/k arrow folding), same split-arrow
    /// safety as [`View::sel_esc`].
    peek_esc: Vec<u8>,
    /// (x-c376) Monotonic `PeekAgent` request counter, bumped per open/move so a
    /// body landing after a newer request is dropped by seq (AC1-FR).
    peek_seq: u64,
    /// (x-9c5f) The peek `m` free-text reply input: (target name captured at
    /// m-press, buffer). `Some` while typing; input mode wins the key route
    /// inside peek (digits/j/k/l/r are literal chars). Client-local like `peek`.
    peek_input: Option<(String, String)>,
    /// Split-CSI carry for the reply input (its own buffer, like `rename_esc`),
    /// so an arrow key mid-type never leaks a param byte into the buffer.
    peek_input_esc: Vec<u8>,
    /// (x-c914) The session-local active claude account: every mux-initiated
    /// worker spawn (prefix+g `DispatchNext`, a targeted `DispatchNode`)
    /// appends `--account <id>` while `Some`. Client-local ephemera like
    /// `nav`/`peek` - dropped on exit, never persisted, never touches a
    /// credential slot (Locked Decisions 1-2). Toggled via the Connections
    /// modal's set-active key; `None` = the default account (no flag).
    active_account: Option<String>,
    /// (x-84d7) The Connections modal (MENU -> connections): a stateful overlay
    /// listing provider accounts + combos, driving the `fno config accounts` CLI.
    /// `Some` while open; stdin diverts to [`connections_keys`]. Its reads run
    /// off the UI loop via the `conn_*` triad below (the needs-fold idiom).
    connections: Option<crate::connections_view::ConnectionsView>,
    /// Pending escape bytes in connections mode (arrow folding, split-arrow safe).
    conn_esc: Vec<u8>,
    /// (x-84d7) A connections read (list/combos fold) is wanted; the run loop
    /// spawns it at loop top and clears this, keeping the sender out of the deep
    /// stdin handler (the needs_want idiom).
    conn_want: bool,
    /// (x-84d7) A connections read is in flight; bounds concurrent folds to one.
    conn_inflight: bool,
    /// (x-84d7) Generation token, bumped per open/refresh so a read landing after
    /// the modal closed or refreshed again is discarded.
    conn_gen: u64,
    /// (x-84d7) A mutation/login verb wanted by a keypress; the run loop spawns
    /// it at loop top (the sender lives there, out of the stdin handler) and
    /// clears this. `(argv, child-env, is_login)`: `is_login` runs `fno mux pane
    /// run` (opens the login pane, keeps the pending notice), else a single-flight
    /// mutation guarded by `ConnectionsView::acting`.
    #[allow(clippy::type_complexity)]
    conn_action: Option<(Vec<String>, Vec<(String, String)>, bool)>,
    /// The last `fno doctor update --check` probe's outcome, or
    /// `None` before the first one lands. `build_sideline_menu` reads this
    /// directly rather than waiting on a fresh probe, so the menu always
    /// opens instantly (Locked Decision 4).
    update_outcome: Option<UpdateOutcome>,
    /// An update-readiness probe is wanted; the run loop spawns it
    /// at loop top and clears this. Set once after the first server frame
    /// lands, and again every time the sideline menu opens, so a menu opened
    /// an hour later is not showing an hour-old answer.
    update_probe_want: bool,
    /// An update-readiness probe is in flight; bounds concurrent
    /// probes to one, mirroring `conn_inflight`.
    update_probe_inflight: bool,
    /// A pending sweep verb (counts probe or scoped apply) for the run
    /// loop to spawn off the UI thread, mirroring `conn_action`.
    sweep_action: Option<SweepAction>,
    /// A sweep verb is in flight; one at a time, so a second tap queues
    /// nothing and is told so.
    sweep_inflight: bool,
}

/// Which half of `mux workspace prune` a sweep action runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepScope {
    Tabs,
    /// (x-cf97) The opt-in half: idle shells the operator has typed in. Its
    /// own row, its own count, its own confirmation - never folded into the
    /// default tabs half, whose posture stays exactly what it was.
    UsedShells,
    Dead,
    Both,
}

/// A sweep verb the run loop should spawn: a counts probe for the choice
/// modal, or an apply of one scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepAction {
    Counts,
    Apply(SweepScope),
}

/// What a finished sweep verb reports back to the UI loop.
#[derive(Debug, Clone)]
enum SweepMsg {
    Counts {
        tabs: usize,
        dead: usize,
        /// (x-cf97) The used-shell population, counted on every probe so its
        /// modal row can carry its own number even though the flag is off.
        used: usize,
    },
    Applied {
        closed: usize,
        reaped: usize,
    },
    Failed(String),
}

/// Spawn the meter sampler: one bounded `macmon pipe -s 1` sample per refresh
/// interval, the one-line reading sent to the UI loop. Exits when the view's
/// gate flips off, so a toggle-off never leaves a sampler running. Two
/// overlapping tasks are harmless: the channel is last-send-wins.
fn spawn_meter_sampler(
    gate: std::sync::Arc<std::sync::atomic::AtomicBool>,
    refresh: u64,
    meter_tx: tokio::sync::mpsc::UnboundedSender<String>,
) {
    tokio::spawn(async move {
        while gate.load(std::sync::atomic::Ordering::Relaxed) {
            let text = sample_macmon_line().await;
            if meter_tx.send(text).is_err() {
                // The UI loop is gone; nothing left to report to.
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(refresh)).await;
        }
    });
}

/// One bounded `macmon pipe -s 1` sample rendered as a status-row segment.
/// macmon streams forever, so the timeout is the normal exit; anything that
/// fails to arrive or parse renders as "sensor unavailable" - a dark sensor
/// is named, never read as a zero.
async fn sample_macmon_line() -> String {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(6),
        tokio::process::Command::new("macmon")
            .arg("pipe")
            .arg("-s")
            .arg("1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let parsed = match output {
        Ok(Ok(out)) => parse_macmon_sample(&out.stdout),
        _ => None,
    };
    parsed.unwrap_or_else(|| "meter: sensor unavailable".into())
}

fn parse_macmon_sample(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    let line = text.lines().find(|l| l.trim_start().starts_with('{'))?;
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let cpu = value.get("cpu_usage_pct")?.as_f64()?;
    let mem = value.get("memory")?;
    let total = mem.get("ram_total")?.as_f64()?;
    let usage = mem.get("ram_usage")?.as_f64()?;
    // macmon's measured contract is a 0-1 fraction; no percent spelling to
    // rescue (the lanes arm pins the same contract).
    let cpu_pct = cpu * 100.0;
    let mut line = format!(
        "cpu {cpu_pct:.0}% mem {:.0}G/{:.0}G",
        usage / 1e9,
        total / 1e9
    );
    if let Some(w) = value.get("sys_power").and_then(|p| p.as_f64()) {
        line.push_str(&format!(" {w:.0}W"));
    }
    Some(line)
}

mod confirm;

pub(crate) use confirm::{ConfirmAction, ConfirmKind};

/// The move-tab / move-pane destination picker's state (x-96e8, cursored by
/// x-3e17). Was a bare `(MoveSrc, Vec<u64>)` tuple, which had nowhere to keep a
/// cursor or the escape carry an arrow key needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MovePick {
    src: MoveSrc,
    /// Every candidate destination, uncapped (see `View::move_dst_squads`).
    squads: Vec<u64>,
    /// Index into `squads` of the highlighted destination.
    cursor: usize,
    /// Escape-sequence carry across reads, so an arrow split at a read boundary
    /// neither closes the picker nor leaks its tail into the pane.
    esc: Vec<u8>,
}

impl MovePick {
    /// The selected destination squad id, or `None` for an empty list.
    ///
    /// Test-only: the commit path consumes the picker and indexes `squads`
    /// directly, so this exists to let a test read the selection without
    /// reproducing that arithmetic. Unconditional, it is dead code in the
    /// shipped build.
    #[cfg(test)]
    fn target(&self) -> Option<u64> {
        self.squads.get(self.cursor).copied()
    }

    /// A freshly opened picker: cursor at the top, no escape carry.
    fn new(src: MoveSrc, squads: Vec<u64>) -> Self {
        Self {
            src,
            squads,
            cursor: 0,
            esc: Vec::new(),
        }
    }
}

/// Client-local in-scrollback search state (v12, x-e780).
struct SearchView {
    /// The pane the search opened on (captured so every step/clear targets it,
    /// even if focus shifts server-side mid-search).
    pane: u64,
    /// The input buffer (ASCII printable; typed in typing mode).
    query: String,
    /// `false` while typing (Enter submits), `true` while browsing (n/N step).
    submitted: bool,
    /// Latest `(total, current)` from the server, `None` until the first
    /// `SearchResult`. `total == 0` renders "no matches".
    result: Option<(u32, u32)>,
}

/// Client-local session-navigator overlay state (x-653d). The rows are NOT
/// stored here - they are recomputed from the live layout each keypress (the
/// same per-key re-read discipline as the selector/search), so a layout push
/// under an open navigator is reflected at once.
struct NavView {
    /// Incremental text filter (substring, case-insensitive) over row match
    /// keys: label + pane id + node id + slug + workspace (x-e10f).
    query: String,
    /// The active state chip; `None` = all states. `Tab` cycles it.
    state_filter: Option<PaneState>,
    /// Cursor into the CURRENTLY filtered rows (clamped per key, no wrap).
    cursor: usize,
}

/// (x-c376) The read-only peek overlay over a sideline agent row: its full
/// status sentence + recent transcript + (for a blocked row) the x-c929
/// answerable prompt. Opens ON TOP of the selector (which stays open
/// underneath); Esc drops back into it. The row is re-read from the live
/// `display_rows()` per frame (navigator-style), so only the index, the request
/// seq, and the fetched body live here - never a stale row snapshot.
struct PeekView {
    /// A `display_rows()` index, always kept on a `DisplayRow::Agent` row.
    cursor: usize,
    /// The seq of the last `PeekAgent` sent; a `PeekBody` with any other seq is
    /// dropped (A->B->A cycling defeats a name-only guard, AC1-FR).
    seq: u64,
    /// The fetched transcript: `None` = still loading (renders " loading…");
    /// `Some(lines)` = loaded (error/timeout text arrives in-band as lines).
    body: Option<Vec<String>>,
    /// The peeked row's name at fetch time. A layout shift that lands a
    /// DIFFERENT agent on `cursor` refetches instead of redrawing the new
    /// header over the old transcript (codex review): the seq guard covers a
    /// late body under the same request, this covers a changed row identity.
    name: String,
    /// (x-9c5f) When this row's transcript was last fetched, throttling the
    /// auto-refresh on Layout pushes to >= `PEEK_REFRESH_INTERVAL` (US9).
    last_fetch: Instant,
    /// (x-9c5f) An auto-refresh request is in flight (armed, body not yet
    /// landed). Guards against stacking a new refresh every Layout push while a
    /// slow `fno agents peek` is still running - without it a >3s peek read on a
    /// busy row would supersede each response before it arrives and never settle.
    /// Cleared when any body lands (`apply_peek_body`).
    refresh_pending: bool,
    /// (x-10ec) Some for a WORKSPACE peek: `sid` of the peeked squad. The body
    /// is rendered locally from the layout (a workspace has no transcript), so
    /// no request follows the seq `open_peek` consumed - which is the point:
    /// that seq can never be answered, so a late `PeekBody` for a superseded
    /// agent peek is dropped by the guard instead of landing in this overlay.
    squad: Option<u64>,
}

/// (x-8ccf US3) The which-key keybinds modal: a centered [`Popup`] built from
/// the single-source prefix-chord table, plus the [`Event`] each selectable row
/// runs (`None` for headers, rules, and display-only meta rows). Keeping the
/// events beside the popup lets Enter/click on the SELECTED row dispatch through
/// the exact path a typed chord would, so help can never advertise an action it
/// cannot run (Locked 3).
struct KeysModal {
    popup: Popup,
    row_events: Vec<Option<Event>>,
}

/// Build the modal's rows from [`key_bindings`] (the dispatcher's own table):
/// title, then each section's header + its bindings (key leading, action right),
/// its display-only meta rows, then a footer hint. `row_events` runs parallel to
/// `popup.rows` so a selected row's chord is one lookup away.
fn build_keys_modal() -> KeysModal {
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut events: Vec<Option<Event>> = Vec::new();
    let mut add = |row: PopupRow, ev: Option<Event>| {
        rows.push(row);
        events.push(ev);
    };
    add(PopupRow::Header("keybinds  ·  esc close".into()), None);
    let bindings = key_bindings();
    for section in [
        KeySection::Global,
        KeySection::Navigation,
        KeySection::WorkspacesTabs,
        KeySection::Panes,
        KeySection::SidelineRows,
    ] {
        add(PopupRow::Header(section.title().into()), None);
        for kb in bindings.iter().filter(|kb| kb.section == section) {
            add(
                PopupRow::Entry {
                    glyph: kb.disp.to_string(),
                    label: kb.label.to_string(),
                    // The stable id `[mux.keys]` names, beside the key it
                    // rebinds. The config contract promises this modal lists
                    // them.
                    hint: kb.action.to_string(),
                    enabled: true,
                },
                Some(kb.event.clone()),
            );
        }
        // Display-only rows (1-9 select tab, prefix-prefix literal): selectable
        // so the reference shows them, but not single-event chords, so Enter
        // BELs. No action id - `chord()` handles them structurally.
        for (disp, label, _) in meta_rows().iter().filter(|(_, _, s)| *s == section) {
            add(
                PopupRow::Entry {
                    glyph: disp.clone(),
                    label: label.clone(),
                    hint: String::new(),
                    enabled: true,
                },
                None,
            );
        }
    }
    add(PopupRow::Rule, None);
    add(
        PopupRow::Header("scroll wheel · pgup/pgdn · ⏎/click/tap runs".into()),
        None,
    );
    // (x-7683) The right-click config note. The mux side works whenever the
    // bytes arrive (FNO_MUX_MOUSE_TRACE proves it either way); the terminals
    // that never send them are named so the operator configures the terminal,
    // or reaches for the no-config paths, instead of reading a dead feature.
    add(PopupRow::Rule, None);
    add(
        PopupRow::Header("right-click works only where the terminal forwards it".into()),
        None,
    );
    add(
        PopupRow::Header("Terminal.app never does · iTerm2: report mouse events".into()),
        None,
    );
    // (x-b465) Ghostty joins the named list: it binds right-click to its own
    // context menu by default (`right-click-action`), measured against Warp on
    // the same build, where the identical press opens the menu. The SETTING is
    // named, not a value to set: which value restores forwarding is untested
    // here, and a config line this text cannot vouch for is the kind of
    // confident wrong answer that cost a whole diagnosis round already.
    add(
        PopupRow::Header("Ghostty binds it too · see right-click-action".into()),
        None,
    );
    add(
        PopupRow::Header(format!(
            "in tmux set mouse off · else m, or hold Left {}ms",
            MENU_LONG_PRESS.as_millis()
        )),
        None,
    );
    KeysModal {
        popup: Popup::new(rows, Anchor::Center),
        row_events: events,
    }
}

/// (x-8ccf US2) The right-click / `m` row context menu over a sideline agent
/// row. The target is pinned by NAME (not index) so a layout reshuffle between
/// open and click can only turn an action into a stale-name refusal, never
/// redirect it to a different agent (Concurrency). `actions` runs parallel to
/// the popup's flat targets (`popup.sel` indexes it directly).
struct RowMenu {
    popup: Popup,
    /// What the menu acts on, pinned at open. Execution fails closed if it no
    /// longer resolves, so a layout reshuffle between open and click can only
    /// produce a stale-target refusal, never a redirected action.
    target: MenuTarget,
    actions: Vec<MenuAction>,
}

/// What a row menu is acting on. A right-click resolves one of these at open
/// time; execution re-resolves it against the LIVE layout, so a target that
/// moved or vanished becomes a Notice rather than a misrouted action.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MenuTarget {
    Agent(AgentIdent),
    /// (x-1d91) A Backlog card pinned by node id (ids are unique in the graph,
    /// so unlike agent names they need no disambiguation).
    Card(String),
    /// A section header (a squad name row or a `~` band). `label` is cosmetic
    /// (the confirm prompt); `key` is the persisted section identity and `squad`
    /// the runtime one, present for a squad/mission header and `None` for a `~`
    /// band (which has no squad).
    Section {
        key: SectionKey,
        label: String,
        squad: Option<u64>,
    },
    /// A tab-strip cell (the tab menu). Pinned by stable [`TabId`]; execution
    /// re-resolves it against the live layout, so a tab that closed between
    /// open and pick is a Notice, never a redirected action.
    Tab(TabId),
}

/// The disambiguating identity of an agent row, captured when a row menu opens.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentIdent {
    name: String,
    pane_id: Option<u64>,
    attach_id: Option<String>,
}

impl AgentIdent {
    fn of(a: &AgentRow) -> Self {
        AgentIdent {
            name: a.name.clone(),
            pane_id: a.pane_id,
            attach_id: a.attach_id.clone(),
        }
    }
    fn matches(&self, a: &AgentRow) -> bool {
        a.name == self.name && a.pane_id == self.pane_id && a.attach_id == self.attach_id
    }
}

/// What a context-menu entry does, resolved against the LIVE agent row (found by
/// name) at execution time - a stale target becomes a Notice, not a wrong action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    /// Attach a bg (paneless) agent by repointing the focused pane (x-9f75).
    OpenHere,
    /// Attach a bg (paneless) agent as a new tab.
    NewTab,
    /// Attach a bg agent as a directional split of the current tab.
    Split(Dir),
    /// Relocate a pane-hosted row's LIVE pane `dir`-ward (`Command::MovePane`,
    /// PTY intact). Not `Split`/`AttachAgent`: on a session that already holds a
    /// live pane, `AttachAgent` reconciles to an idempotent focus of that pane
    /// ("already attached; focused existing pane") and so cannot relocate
    /// anything. `MovePane` is the only command that moves a live leaf, and it
    /// is what `commit_row_drag` already sends for a `RowSource::Pane` - one
    /// gesture, one meaning.
    MoveDir(Dir),
    /// Break a pane-hosted row's live pane out into its own tab
    /// (`Command::BreakPane`) - the menu twin of dragging its grip to the strip.
    BreakOut,
    /// Detach a pane-hosted worker while keeping its PTY live.
    Detach,
    /// Relocate a pane-hosted row's live pane into ANOTHER workspace. Opens the
    /// move picker (the same numbered picker `m` uses for a tab); the chosen
    /// workspace's active-tab focus pane is the `MovePane` anchor, so the live
    /// pane grafts in beside it and de-recruits from its source (the
    /// `move_pane_cross_tab` path the row drag already uses). Appended in
    /// [`View::open_row_menu`] only when another non-mission workspace exists,
    /// so the entry never offers a move with no destination.
    MoveToWorkspace,
    /// Focus an existing pane-hosted row.
    Focus,
    /// Open the read-only peek overlay.
    Peek,
    /// Toggle the git working-diff pane for this row's worktree.
    Diff,
    /// Stop a live row (StopAgent, or StopExternal for a daemon-roster row).
    Stop,
    /// Remove an exited row (RemoveAgent, or RemoveExternal for a roster row).
    Remove,
    /// (x-1d91) Run a reorder verb on a Backlog card.
    Backlog(BacklogVerb),
    /// Open a Backlog card's plan: through Obsidian, or as a plain file when
    /// the plan resolves outside the configured vault. Card-menu only (LD5);
    /// the resolution and the opener live in `link::plan_link`.
    OpenPlan,
    /// Remove EVERY exited row in the target section (x-f300). The section comes
    /// from [`MenuTarget::Section`], so this stays payload-free and `Copy`.
    ClearDead,
    /// Open the rename overlay for a workspace section header - menu parity with
    /// selector `r` (x-96e8). Only built for a section carrying a squad id.
    Rename,
    /// Reorder a workspace section `delta` slots on the sideline - menu parity
    /// with selector `J`/`K`, bound to the same `Command::MoveSquad`.
    MoveSquad(i32),
    /// Remove a whole workspace. Routed through the same
    /// `ConfirmKind::RemoveSquad` confirm the keyboard path uses - a mouse
    /// click must not skip the destructive-action gate.
    RemoveSquad,
    /// (x-92d3 5.1) Tab-menu entries, all bound to existing wire commands and
    /// all resolved against the pinned [`TabId`] at execute: a new tab on the
    /// target's squad, the rename overlay, a strip reorder, a join of the whole
    /// tab into the viewed tab as `dir` split of the focused pane, and the
    /// confirm-gated close.
    TabNew,
    TabRename,
    TabReorder(i32),
    /// (x-cf97) Open the move-to-position prompt on the pinned tab: type the
    /// 1-based destination, the client computes the delta and sends ONE
    /// `Command::ReorderTab`. The direct-destination gesture the ±1 pair
    /// cannot be - moving tab 22 to slot 3 costs one prompt, not nineteen
    /// presses.
    TabMoveTo,
    TabJoin(Dir),
    TabClose,
    /// (x-92d3 6.2) Respawn an exited row - the menu twin of peek `r`, the
    /// same `Command::RespawnAgent`.
    Resume,
    /// Reattach a live paneless row whose server-side session is still present.
    Reattach,
    /// (x-92d3 6.2) Mail this agent - opens the SAME peek overlay and its
    /// free-text composer (`peek_input`) that peek `m` opens, never a second
    /// input surface.
    Mail,
    /// Rename a sideline row's registry LABEL - opens the shared rename
    /// overlay on `RenameTarget::Agent`. Built only for a non-external,
    /// unambiguous agent row.
    RenameAgent,
}

impl MenuAction {
    /// (x-91a1) The stable id this action's in-menu accelerator is registered
    /// under in `keys::menu_bindings`, when it has one. The drawn hint and the
    /// dispatched byte both resolve through this id, so the key the menu
    /// advertises and the key it answers cannot drift. The tab verbs and the
    /// settled rename/close/remove pair carry one; everything else is
    /// Enter/click only and renders no key.
    fn accelerator_id(&self) -> Option<&'static str> {
        match self {
            MenuAction::TabNew => Some("new-tab"),
            MenuAction::TabRename => Some("rename-tab"),
            MenuAction::Rename => Some("rename-workspace"),
            MenuAction::TabClose => Some("close-tab"),
            MenuAction::TabReorder(delta) => Some(if *delta < 0 {
                "move-tab-left"
            } else {
                "move-tab-right"
            }),
            MenuAction::TabMoveTo => Some("move-tab-to"),
            MenuAction::Remove => Some("remove-row"),
            // (x-d545) The row-menu verbs join the keyboard: every entry that
            // can carry a key now shows one, and the drawn glyph and the
            // dispatched byte both resolve through the same menu-scope id.
            MenuAction::Stop => Some("stop-row"),
            MenuAction::Peek => Some("peek-row"),
            MenuAction::Mail => Some("mail-row"),
            MenuAction::RenameAgent => Some("rename-agent"),
            MenuAction::Diff => Some("diff-row"),
            MenuAction::OpenHere => Some("open-here"),
            MenuAction::Resume => Some("resume-row"),
            _ => None,
        }
    }
}

/// What the numbered move picker is relocating: a whole tab (selector `m`) or a
/// single pane-hosted row's live pane (the row menu's Move-to-workspace entry).
/// Both list destination workspaces and resolve the same way; only the command
/// the digit sends differs (`MoveTab` vs `MovePane`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveSrc {
    Tab(TabId),
    Pane(u64),
}

/// (x-91a1) An entry whose action has an IN-MENU accelerator: the hint is the
/// live glyph from the menu scope (`keys::menu_key_for`), never a prefix chord
/// - the open menu does not run prefix chords, so advertising one describes an
/// input path the reader is not on. An unscoped id resolves to nothing, which
/// is the honest hint (LD9 / AC8).
fn entry_acc(glyph: &str, label: &str, id: &str) -> PopupRow {
    PopupRow::Entry {
        glyph: glyph.into(),
        label: label.into(),
        hint: crate::keys::menu_key_for(id).unwrap_or_default(),
        enabled: true,
    }
}

/// Build the per-state row menu for the agent at `display_rows()` index `i`,
/// anchored at `anchor`. `None` for a non-agent row (the menu is agent-only).
/// Entry sets mirror the row's state so no dead item ever renders: a paneless
/// bg row gets the new-tab + 2x2 split grid (its whole point); a pane row gets
/// focus plus the move/break-out grid that relocates its live pane; an exited
/// row gets remove; peek/stop apply where they make sense.
fn build_row_menu(agent: &AgentRow, anchor: Anchor) -> RowMenu {
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut actions: Vec<MenuAction> = Vec::new();
    let mut add = |row: PopupRow, acts: &[MenuAction]| {
        rows.push(row);
        actions.extend_from_slice(acts);
    };
    let entry = |glyph: &str, label: &str| PopupRow::Entry {
        glyph: glyph.into(),
        label: label.into(),
        hint: String::new(),
        enabled: true,
    };
    let cell = |glyph: &str, label: &str| GridCell {
        glyph: glyph.into(),
        label: label.into(),
    };
    // A live row cannot be removed - the server refuses `RemoveAgent` on one
    // with "still live - stop it first". Rendering the entry greyed beside Stop
    // says the row CAN be removed and names the precondition, where leaving it
    // out of the live menus entirely said the action does not exist. Disabled
    // contributes zero targets (`PopupRow::cells`), so it carries no action and
    // the actions vector stays aligned with the selectable rows. (x-d545) The
    // hint carries the key too: the one moment the menu could teach the byte
    // that WILL remove the row one keypress later is this one, so the glyph
    // (resolved through the menu scope, never a literal) rides beside the
    // precondition.
    let inert = |glyph: &str, label: &str| PopupRow::Entry {
        glyph: glyph.into(),
        label: label.into(),
        hint: format!(
            "{} stop first",
            crate::keys::menu_key_for("remove-row").unwrap_or_default()
        ),
        enabled: false,
    };
    add(PopupRow::Header(agent.name.clone()), &[]);
    add(PopupRow::Rule, &[]);
    if agent.exited {
        add(
            entry_acc("✕", "Remove", "remove-row"),
            &[MenuAction::Remove],
        );
        add(entry_acc("◉", "Peek", "peek-row"), &[MenuAction::Peek]);
        // Resume above the rule (AC7): the menu twin of peek `r`, on the row
        // state `r` accepts - an exited row.
        add(
            entry_acc("↻", "Resume", "resume-row"),
            &[MenuAction::Resume],
        );
    } else if agent.pane_id.is_some() {
        // Live pane row: already placed, so re-placement is a MOVE of the live
        // pane, never an attach. Same 2x2 grid geometry the paneless branch uses
        // below, so the two menus read as one system; the verbs differ because
        // the operations do (move a running pane vs. place a new one).
        add(entry("→", "Focus"), &[MenuAction::Focus]);
        add(entry_acc("◉", "Peek", "peek-row"), &[MenuAction::Peek]);
        add(entry_acc("✉", "Mail", "mail-row"), &[MenuAction::Mail]);
        add(PopupRow::Rule, &[]);
        add(
            PopupRow::FullWidth("▭ New Tab".into()),
            &[MenuAction::BreakOut],
        );
        add(entry("⇱", "Detach pane"), &[MenuAction::Detach]);
        // Ungated by pane count. A row whose pane is on screen and has no
        // neighbour `dir`-ward gets the server's "no pane in that direction"
        // notice, the same fail-closed feedback the paneless branch relies on;
        // a row whose pane is off screen always has somewhere to land (the
        // current view), so gating on the source tab would be wrong anyway.
        add(
            PopupRow::Grid(vec![cell("◧", "Move Left"), cell("◨", "Move Right")]),
            &[
                MenuAction::MoveDir(Dir::Left),
                MenuAction::MoveDir(Dir::Right),
            ],
        );
        add(
            PopupRow::Grid(vec![cell("⬒", "Move Up"), cell("⬓", "Move Down")]),
            &[MenuAction::MoveDir(Dir::Up), MenuAction::MoveDir(Dir::Down)],
        );
        add(PopupRow::Rule, &[]);
        add(entry_acc("■", "Stop", "stop-row"), &[MenuAction::Stop]);
        add(inert("✕", "Remove"), &[]);
    } else if agent.attach_id.is_some() {
        // Paneless bg row: the motivating case - open as a tab or a split pane.
        // Open-here leads (repoint the focused viewer). The client can't know viewer-ness, so the
        // server's fail-closed notice is the feedback path when the focus isn't a detachable viewer.
        add(
            entry_acc("⊙", "Open Here", "open-here"),
            &[MenuAction::OpenHere],
        );
        add(
            PopupRow::FullWidth("▭ New Tab".into()),
            &[MenuAction::NewTab],
        );
        add(PopupRow::Rule, &[]);
        // 2x2 spatial grid: Left/Right on top, Up/Down below (the cell you pick
        // IS the direction). Glyphs are half-block squares; a non-nerd-font
        // terminal still shows the label beside them.
        add(
            PopupRow::Grid(vec![cell("◧", "Split Left"), cell("◨", "Split Right")]),
            &[MenuAction::Split(Dir::Left), MenuAction::Split(Dir::Right)],
        );
        add(
            PopupRow::Grid(vec![cell("⬒", "Split Up"), cell("⬓", "Split Down")]),
            &[MenuAction::Split(Dir::Up), MenuAction::Split(Dir::Down)],
        );
        add(PopupRow::Rule, &[]);
        add(entry_acc("◉", "Peek", "peek-row"), &[MenuAction::Peek]);
        add(entry_acc("✉", "Mail", "mail-row"), &[MenuAction::Mail]);
        add(entry_acc("■", "Stop", "stop-row"), &[MenuAction::Stop]);
        add(inert("✕", "Remove"), &[]);
    } else {
        // A live row that is neither pane-hosted nor attachable here.
        add(entry_acc("◉", "Peek", "peek-row"), &[MenuAction::Peek]);
        add(entry_acc("✉", "Mail", "mail-row"), &[MenuAction::Mail]);
        if agent.no_pane_reason == Some(AgentNoPaneReason::LivePaneless) {
            add(entry("↩", "Reattach"), &[MenuAction::Reattach]);
        }
        add(entry_acc("■", "Stop", "stop-row"), &[MenuAction::Stop]);
        add(inert("✕", "Remove"), &[]);
    }
    // Diff is common to every row state: it reads the row's worktree,
    // which an exited or paneless row has just as much as a live pane-hosted
    // one - and a finished worker's diff is the one you most want to read.
    // (x-d545) Bound in menu scope now, so its hint is the live key.
    add(PopupRow::Rule, &[]);
    add(entry_acc("±", "Diff", "diff-row"), &[MenuAction::Diff]);
    // Live AND exited rows are renamable; an EXTERNAL row is claude-owned.
    if !agent.external {
        add(entry("✎", "Rename"), &[MenuAction::RenameAgent]);
    }
    RowMenu {
        popup: Popup::new(rows, anchor),
        target: MenuTarget::Agent(AgentIdent::of(agent)),
        actions,
    }
}

/// (x-1d91) The v1 reorder menu for a Backlog card: float to top, defer. Both
/// route through `fno backlog` server-side; the mux never writes the graph.
/// Floated READY cards carry a "may dispatch" hint: the dispatcher can pick
/// one up in about a minute, and the guards it applies (containers, batching,
/// stale candidates, project scope) are not modeled here, so the hint promises
/// nothing.
fn build_card_menu(
    card: &BacklogCard,
    obsidian: &crate::digest_overlay::ObsidianCfg,
    anchor: Anchor,
) -> RowMenu {
    let label = if card.slug.is_empty() {
        &card.id
    } else {
        &card.slug
    };
    let float_hint = match card.state {
        CardState::Ready => "may dispatch",
        _ => "",
    };
    let mut rows = vec![
        PopupRow::Header(label.clone()),
        PopupRow::Rule,
        PopupRow::Entry {
            glyph: "▲".into(),
            label: "Float to top".into(),
            hint: float_hint.into(),
            enabled: true,
        },
        PopupRow::Entry {
            glyph: "⏸".into(),
            label: "Defer".into(),
            hint: String::new(),
            enabled: true,
        },
    ];
    let mut actions = vec![
        MenuAction::Backlog(BacklogVerb::RankTop),
        MenuAction::Backlog(BacklogVerb::Defer),
    ];
    // LD7: a node with no plan is greyed (state can change; the item will
    // apply later). Obsidian off is absent instead - no state change in this
    // menu can unlock it, so a permanently-greyed item would advertise a
    // capability nothing here can turn on.
    match crate::link::plan_link(card.plan_path.as_deref().map(Path::new), obsidian) {
        crate::link::PlanLink::Unavailable(crate::link::PlanUnavailable::NoPlan) => {
            rows.push(PopupRow::Entry {
                glyph: "▤".into(),
                label: "Open plan".into(),
                hint: "no plan".into(),
                enabled: false,
            });
            // Disabled: 0 cells, so no action slot - actions stays index-aligned
            // with Popup::targets(), never with rows.
        }
        crate::link::PlanLink::Unavailable(crate::link::PlanUnavailable::ObsidianOff) => {}
        crate::link::PlanLink::Obsidian { .. } => {
            rows.push(PopupRow::Entry {
                glyph: "▤".into(),
                label: "Open plan".into(),
                hint: String::new(),
                enabled: true,
            });
            actions.push(MenuAction::OpenPlan);
        }
        crate::link::PlanLink::PlainFile(_) => {
            rows.push(PopupRow::Entry {
                glyph: "▤".into(),
                label: "Open plan (file)".into(),
                hint: String::new(),
                enabled: true,
            });
            actions.push(MenuAction::OpenPlan);
        }
    }
    RowMenu {
        popup: Popup::new(rows, anchor),
        target: MenuTarget::Card(card.id.clone()),
        actions,
    }
}
/// The command that clears ONE dead row, by what kind of row it is. Three
/// stores hold dead rows and each has its own verb: a member TOMBSTONE lives in
/// the squad's member list (`RemoveAgent` resolves only against the agent
/// registry, so it would answer "no such agent" and leave the row on screen), an
/// EXTERNAL row routes by its stable attach_id (x-7561), and a registry row goes
/// by name. One mapping so the row menu and the bulk clear cannot disagree.
fn remove_dead(a: &AgentRow) -> Command {
    match (a.tombstone, a.squad, a.external, a.attach_id.clone()) {
        (true, Some(squad), _, Some(attach_id)) => Command::DismissMember { squad, attach_id },
        (_, _, true, Some(attach_id)) => Command::RemoveExternal {
            attach_id,
            name: a.name.clone(),
        },
        _ => Command::RemoveAgent {
            name: a.name.clone(),
            harness_session_id: a.harness_session_id.clone(),
        },
    }
}

/// How many rows one clear-dead may remove. Each row costs the server a
/// `fno agents rm` subprocess (`agent_action` spawns one per command, unbounded),
/// so an unbounded fan-out would let a long-lived section stampede the daemon.
/// ponytail: a flat cap, repeat to clear the rest; the upgrade is a section-scoped
/// bulk verb server-side, which the single-process `ReapAgents` already models.
const CLEAR_DEAD_MAX: usize = 25;

/// (x-f300) The section-header context menu. A workspace section (`squad`
/// present) offers `Rename` - menu parity with selector `r`. `Clear dead` is
/// added only when `dead > 0`; its label count is both what it advertises AND
/// what the commit runs, so the two can never disagree. The caller guarantees
/// at least one of {renamable, `dead > 0`} holds, so the menu is never empty.
fn build_section_menu(
    key: SectionKey,
    label: String,
    squad: Option<u64>,
    dead: usize,
    anchor: Anchor,
) -> RowMenu {
    let mut rows = vec![PopupRow::Header(label.clone()), PopupRow::Rule];
    let mut actions: Vec<MenuAction> = Vec::new();
    if squad.is_some() {
        let entry = |glyph: &str, label: &str| PopupRow::Entry {
            glyph: glyph.into(),
            label: label.into(),
            hint: String::new(),
            enabled: true,
        };
        rows.push(entry_acc("✎", "Rename", "rename-workspace"));
        actions.push(MenuAction::Rename);
        rows.push(entry("▲", "Move up"));
        actions.push(MenuAction::MoveSquad(-1));
        rows.push(entry("▼", "Move down"));
        actions.push(MenuAction::MoveSquad(1));
        rows.push(PopupRow::Rule);
        rows.push(entry("✕", "Remove workspace"));
        actions.push(MenuAction::RemoveSquad);
    }
    if dead > 0 {
        rows.push(PopupRow::Entry {
            glyph: "✕".into(),
            label: format!("Clear dead ({dead})"),
            hint: String::new(),
            enabled: true,
        });
        actions.push(MenuAction::ClearDead);
    }
    RowMenu {
        popup: Popup::new(rows, anchor),
        target: MenuTarget::Section { key, label, squad },
        actions,
    }
}

/// (x-92d3 5.1) The tab-strip context menu for one tab cell, resolved through
/// the SAME `tab_cell_at` the drag pickup uses (LD-A: one hit test per
/// surface, so a drag and a click can never disagree about where a tab is).
/// Every item binds an existing wire command; nothing here needs a server
/// change, because a tab-bar cell sits in no pane rect and was never
/// forwarded. Destructive items sit last, after a `Rule`.
///
/// Save/apply layout are deliberately ABSENT: `ControlVerb::LayoutGet` /
/// `LayoutApply` ride one-shot `ClientMsg::Control` connections (`fno mux
/// pane ...`), which an attached TUI client cannot send, so a menu item for
/// them would bind to a verb this socket can never carry. That needs a
/// `Command` surface and is filed rather than faked.
fn build_tab_menu(idx: usize, tab: &TabMeta, anchor: Anchor) -> RowMenu {
    let mut rows: Vec<PopupRow> = Vec::new();
    let mut actions: Vec<MenuAction> = Vec::new();
    let mut add = |row: PopupRow, acts: &[MenuAction]| {
        rows.push(row);
        actions.extend_from_slice(acts);
    };
    let cell = |glyph: &str, label: &str| GridCell {
        glyph: glyph.into(),
        label: label.into(),
    };
    // Join mirrors the row menu's split grid: the picked cell IS the side of
    // the focused pane the joined tab lands on. No hint: no prefix binding
    // names a join (the gesture path is the tab drag), and LD9 forbids a
    // literal chord standing in for one.
    add(
        PopupRow::Header(tab_group_label(
            tab_label_text(&tab.name, idx, tab.named),
            tab.panes.len(),
        )),
        &[],
    );
    add(PopupRow::Rule, &[]);
    // (x-91a1) Every tab verb answers a bare in-menu key from the same
    // registry its hint reads: n for New tab, the angle brackets for the
    // reorder pair - app vocabulary beside the prefix chords (prefix+c, and
    // prefix+< / prefix+> mean the same moves from outside the menu).
    add(entry_acc("▭", "New tab", "new-tab"), &[MenuAction::TabNew]);
    add(
        entry_acc("✎", "Rename", "rename-tab"),
        &[MenuAction::TabRename],
    );
    add(
        entry_acc("◧", "Move left", "move-tab-left"),
        &[MenuAction::TabReorder(-1)],
    );
    add(
        entry_acc("◨", "Move right", "move-tab-right"),
        &[MenuAction::TabReorder(1)],
    );
    add(
        entry_acc("⇥", "Move to…", "move-tab-to"),
        &[MenuAction::TabMoveTo],
    );
    add(
        PopupRow::Grid(vec![cell("◧", "Join Left"), cell("◨", "Join Right")]),
        &[
            MenuAction::TabJoin(Dir::Left),
            MenuAction::TabJoin(Dir::Right),
        ],
    );
    add(
        PopupRow::Grid(vec![cell("⬒", "Join Up"), cell("⬓", "Join Down")]),
        &[MenuAction::TabJoin(Dir::Up), MenuAction::TabJoin(Dir::Down)],
    );
    add(PopupRow::Rule, &[]);
    // `✕ Close`, not `✕ Close tab`: one shape with the row menu's `✕ Remove`,
    // so the two destructive affordances read as one vocabulary. The prefix
    // `&` chord is untouched; in-menu the entry answers the scoped `x`.
    add(
        entry_acc("✕", "Close", "close-tab"),
        &[MenuAction::TabClose],
    );
    RowMenu {
        popup: Popup::new(rows, anchor),
        target: MenuTarget::Tab(tab.id),
        actions,
    }
}

/// (x-8ccf US4/US5) The sideline MENU popup and the minimal settings modal share
/// this one aux-popup type: a [`Popup`] plus the [`AuxAction`] each selectable
/// row runs. The two chain (MENU -> settings) by swapping the `aux` slot.
struct AuxPopup {
    popup: Popup,
    actions: Vec<AuxAction>,
}

/// What a MENU / settings-modal / mini-kanban row does. Menu entries open a
/// surface or detach; settings entries change a live setting; a kanban entry
/// names a card. Not `Copy` since x-1d91 - a card action carries its node id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuxAction {
    OpenKeybinds,
    OpenSettings,
    OpenConnections,
    /// Open the update-readiness overlay: version pair, changelog,
    /// and the one computed guidance line. Only offered by the menu when the
    /// last probe reported ready (or degraded) - see `build_sideline_menu`.
    OpenUpdate,
    /// Probe `mux workspace prune --dry-run` once and open the centered
    /// sweep-threads choice modal from its counts. Both halves of the prune
    /// (surplus pristine tabs, dead member rows) live behind this one entry.
    OpenSweep,
    /// Apply the prune with one scope. Each choice is the confirmation: the
    /// modal named the counts, the tap picked the half.
    SweepTabs,
    /// (x-cf97) The opt-in used-shell half: `--tabs-only
    /// --include-used-shells`. Its own row so a tap can only ever pick the
    /// posture the modal showed.
    SweepUsedShells,
    SweepDeadAgents,
    SweepBoth,
    Detach,
    ToggleHoverFocus,
    ToggleStatus,
    /// The whole-machine resource meter: flip the status-row meter, persist
    /// `resource_meter.enabled`, start or stop the sampler.
    ToggleResourceMeter,
    /// (x-f75e) Apply the named mux theme now: swap the in-memory theme, then
    /// persist via `fno config set mux.theme`. The picker lists the shipped
    /// names, so this carries one of them.
    ApplyTheme(String),
    /// Apply a validated mux prefix change now, then persist it through the CLI.
    ApplyPrefix(String),
    /// (x-e4f1) Open the color picker for one `[sideline.colors]` axis key
    /// (existing or just typed). The axis names its table
    /// (`harness` / `route` / `model` / `row`).
    LaneColorEdit(String, String),
    /// Open the text entry for naming a NEW key on an axis.
    LaneColorAdd(String),
    /// From the picker, open the free-form color entry (indexed(n) / #rrggbb).
    LaneColorCustom(String, String),
    /// Persist one lane color through `fno config set` block-replace, then
    /// restart-free via `reload_palette`.
    LaneColorSet(String, String, String),
    /// (x-1d91) Jump the sideline selector to this Backlog card and close the
    /// mini-kanban - the overlay is a scanning surface, so acting on a card
    /// hands you back to the row where its full menu lives.
    BacklogGoto(String),
}

/// The settings modal's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    General,
    Theme,
    Keys,
    Colors,
}

const PREFIX_PICKS: [&str; 4] = ["C-a", "C-b", "C-x", "C-t"];

fn build_prefix_settings_rows(live_prefix: &str) -> (Vec<PopupRow>, Vec<AuxAction>) {
    let mut rows = vec![PopupRow::Header(format!("prefix: {live_prefix}"))];
    let mut actions = Vec::new();
    for spec in PREFIX_PICKS {
        let active = live_prefix == spec;
        rows.push(PopupRow::Entry {
            glyph: if active { "●".into() } else { "○".into() },
            label: spec.into(),
            hint: if active {
                "active".into()
            } else {
                String::new()
            },
            enabled: true,
        });
        actions.push(AuxAction::ApplyPrefix(spec.into()));
    }
    (rows, actions)
}

/// (x-1d91) Build the mini-kanban: the Backlog's lanes as collapsed columns, each
/// a header carrying its TRUE count over the cards the feed is holding.
///
/// It is the QUEUE's lanes, not the whole board's. The feed carries only
/// actionable work (ready / blocked / in-flight), so done and idea nodes never
/// reach it and a `Done` column never appears - this is a scan of what is up for
/// grabs, and `fno backlog board` remains the full-board view. The counts are
/// true for what they claim: every queue card, including those past the render
/// cap.
///
/// Lanes stack vertically rather than sitting side by side: the sideline is
/// narrow, and a stacked list needs no 2D navigation to scan. The `counts` are
/// the uncapped per-lane totals, so a lane whose cards were cut by the feed cap
/// still states how much work it really holds.
fn build_kanban(cards: &[BacklogCard], counts: &[(String, usize)], anchor: Anchor) -> AuxPopup {
    let mut rows = vec![PopupRow::Header("backlog".into()), PopupRow::Rule];
    let mut actions = Vec::new();
    for (lane, total) in counts {
        rows.push(PopupRow::Header(format!("{lane}  {total}")));
        let mut shown = 0usize;
        for c in cards.iter().filter(|c| card_lane(c) == lane.as_str()) {
            let label = if c.slug.is_empty() { &c.id } else { &c.slug };
            rows.push(PopupRow::Entry {
                glyph: lattice_glyph(card_lattice_state(c.state)).0.into(),
                label: label.clone(),
                hint: if c.head {
                    "head".into()
                } else {
                    c.priority.clone()
                },
                enabled: true,
            });
            actions.push(AuxAction::BacklogGoto(c.id.clone()));
            shown += 1;
        }
        // Say so when the lane holds more than the feed carries, rather than
        // letting the header count silently disagree with the rows under it.
        if *total > shown {
            rows.push(PopupRow::Header(format!("  +{} more", total - shown)));
        }
    }
    AuxPopup {
        popup: Popup::new(rows, anchor),
        actions,
    }
}

/// The lane a card belongs to in the mini-kanban. A card with no
/// `_kanban_column` still needs a home, so it gets a named one rather than
/// vanishing from the board.
fn card_lane(c: &BacklogCard) -> &str {
    c.lane.as_deref().unwrap_or(UNLANED)
}

/// The bucket for cards carrying no `_kanban_column`.
const UNLANED: &str = "unlaned";

/// The client's view of `fno doctor update --check`'s payload - only
/// the fields the menu row and overlay render. `#[serde(default)]` on
/// `changelog` tolerates an absent key rather than failing the whole parse;
/// every other field is required, so a shape the Python resolver no longer
/// emits degrades the probe instead of silently rendering stale/zeroed data.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct UpdateReadiness {
    update_ready: bool,
    installed_rev: Option<String>,
    source_rev: Option<String>,
    #[serde(default)]
    changelog: Vec<String>,
    guidance: String,
    degraded: Option<String>,
}

/// The result of one `fno doctor update --check` probe: parsed
/// readiness, or a degraded reason (missing binary, non-zero exit, timeout,
/// unparseable JSON). Mirrors `connections_view::ReadOutcome` (Locked
/// Decision 4) - the TUI computes nothing beyond folding this into rows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UpdateOutcome {
    Ok(UpdateReadiness),
    Degraded(String),
}

/// Well above the Connections read timeout (1.5s): `--check` shells out to
/// `mux ls` (5s), `agents list` (15s), and `git log` (5s) SEQUENTIALLY on the
/// Python side, so its own worst-case latency alone is ~25s. This never
/// blocks the UI loop (the probe runs off it and the menu opens on whatever
/// outcome is already in hand), so there is no cost to sizing it well above
/// that worst case rather than racing it.
const UPDATE_PROBE_TIMEOUT: Duration = Duration::from_millis(30_000);

/// Run `fno doctor update --check` off the UI loop and fold it into an
/// [`UpdateOutcome`]. Mirrors `connections_view::read_json` exactly (Locked
/// Decision 4): the event loop never blocks on this subprocess: a
/// timeout, non-zero exit, or unparseable JSON all degrade rather than hang
/// or panic (AC6-EDGE).
///
/// `--check` already prints JSON on its own (`update` has no local `--json`
/// option, and the global `--json` flag only applies before the verb) - do
/// not add `--json` after `--check` here, it makes the CLI exit 2 and every
/// probe degrade (P1, codex on PR #881).
async fn probe_update_readiness() -> UpdateOutcome {
    let mut command = crate::process_admission::tokio_command(crate::server::fno_bin());
    command
        .args(["doctor", "update", "--check"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = match tokio::time::timeout(UPDATE_PROBE_TIMEOUT, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return UpdateOutcome::Degraded(format!("update --check: {e}")),
        Err(_) => return UpdateOutcome::Degraded("update --check: timed out".into()),
    };
    if !output.status.success() {
        return UpdateOutcome::Degraded(format!(
            "update --check: exit {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    match serde_json::from_slice::<UpdateReadiness>(&output.stdout) {
        Ok(r) => UpdateOutcome::Ok(r),
        Err(e) => UpdateOutcome::Degraded(format!("update --check: unparseable output ({e})")),
    }
}

/// Build the sideline MENU popup (US4), anchored at the footer's menu cell:
/// an update row (only when the last probe has landed and is ready or
/// degraded), then keybinds / settings / detach. `reload config` is
/// intentionally absent - there is no config-reload machinery to route it to
/// (a net-new capability, not a re-route), so the menu advertises only what
/// actually works.
fn build_sideline_menu(anchor: Anchor, update: Option<&UpdateOutcome>) -> AuxPopup {
    let entry = |glyph: &str, label: &str| PopupRow::Entry {
        glyph: glyph.into(),
        label: label.into(),
        hint: String::new(),
        enabled: true,
    };
    let mut rows = vec![PopupRow::Header("menu".into()), PopupRow::Rule];
    let mut actions = Vec::new();
    // A probe still in flight (or never fired yet) builds the menu
    // WITHOUT an update row rather than waiting - the menu opens instantly.
    match update {
        Some(UpdateOutcome::Ok(r)) if r.update_ready => {
            rows.push(entry("⬆", "update ready"));
            actions.push(AuxAction::OpenUpdate);
        }
        // A successfully-parsed probe (Python always exits 0) can still be
        // internally degraded (e.g. `fno mux ls` failed inside the check).
        // Without this arm that state falls to `_ => {}` and the menu shows
        // nothing, hiding a real check failure from the operator.
        Some(UpdateOutcome::Ok(r)) if r.degraded.is_some() => {
            rows.push(entry("⬆", "update check degraded"));
            actions.push(AuxAction::OpenUpdate);
        }
        Some(UpdateOutcome::Degraded(_)) => {
            rows.push(entry("⬆", "update check failed"));
            actions.push(AuxAction::OpenUpdate);
        }
        _ => {}
    }
    rows.push(entry("♺", "sweep threads"));
    rows.push(entry("⌨", "keybinds"));
    rows.push(entry("⚙", "settings"));
    rows.push(entry("⇄", "connections"));
    rows.push(entry("⏏", "detach"));
    actions.push(AuxAction::OpenSweep);
    actions.push(AuxAction::OpenKeybinds);
    actions.push(AuxAction::OpenSettings);
    actions.push(AuxAction::OpenConnections);
    actions.push(AuxAction::Detach);
    AuxPopup {
        popup: Popup::new(rows, anchor),
        actions,
    }
}

/// Build the update-readiness overlay from the last probe outcome:
/// version pair, up to ten changelog subjects, a rule, then the one computed
/// guidance line - or, for a degraded probe, the degraded reason in the
/// guidance line's place. Never an empty body (AC5-HP/AC6-EDGE): `outcome`
/// is only `None` if this is somehow opened before any probe ever ran, which
/// `build_sideline_menu` never offers as a way in.
fn build_update_modal(outcome: Option<&UpdateOutcome>) -> AuxPopup {
    let mut rows = vec![PopupRow::Header("update".into()), PopupRow::Rule];
    match outcome {
        Some(UpdateOutcome::Ok(r)) => {
            let installed = r.installed_rev.as_deref().unwrap_or("unknown");
            let source = r.source_rev.as_deref().unwrap_or("unknown");
            rows.push(PopupRow::Header(format!("{installed} -> {source}")));
            if !r.changelog.is_empty() {
                rows.push(PopupRow::Rule);
                for subject in &r.changelog {
                    rows.push(PopupRow::Header(subject.clone()));
                }
            }
            rows.push(PopupRow::Rule);
            rows.push(PopupRow::Header(r.guidance.clone()));
        }
        Some(UpdateOutcome::Degraded(reason)) => {
            rows.push(PopupRow::Header(format!("update check failed: {reason}")));
        }
        None => {
            rows.push(PopupRow::Header("update check has not run yet".into()));
        }
    }
    AuxPopup {
        popup: Popup::new(rows, Anchor::Center)
            .title("update")
            .footer("esc close"),
        actions: Vec::new(),
    }
}

/// Build the centered sweep-threads choice modal from one
/// `mux workspace prune --dry-run` reading: close the surplus pristine
/// tabs, close the opt-in used-shell tabs, reap the dead member rows, or
/// combinations. A zero count greys its entry out (0 targets, so arrows skip
/// it and a click is swallowed); with every count zero there is nothing to
/// choose, and the header says so. Each row carries its OWN count, and the
/// tap IS the confirmation - the used-shell half is a separate row, never a
/// rider on the default tabs half, so the sweep's posture is visible before
/// it acts (x-cf97).
fn build_sweep_modal(tabs: usize, used: usize, dead: usize) -> AuxPopup {
    let choice = |label: String, hint: &str, enabled: bool| PopupRow::Entry {
        glyph: "♺".into(),
        label,
        hint: hint.into(),
        enabled,
    };
    let mut rows = vec![PopupRow::Header("sweep threads".into()), PopupRow::Rule];
    let mut actions: Vec<AuxAction> = Vec::new();
    rows.push(choice(
        format!("tabs ({tabs})"),
        "close surplus shell tabs",
        tabs > 0,
    ));
    if tabs > 0 {
        actions.push(AuxAction::SweepTabs);
    }
    rows.push(choice(
        format!("+ used shells ({used})"),
        // (review) The flag is ADDITIVE on the CLI: the apply closes the
        // spent shells AND the surplus pristine tabs, so the hint names the
        // real total and the row says "+" - the count a row shows must bound
        // what its tap closes.
        &format!(
            "close spent shells plus the {tabs} surplus tabs ({} total)",
            tabs + used
        ),
        used > 0,
    ));
    if used > 0 {
        actions.push(AuxAction::SweepUsedShells);
    }
    rows.push(choice(
        format!("dead agents ({dead})"),
        "reap dead member rows",
        dead > 0,
    ));
    if dead > 0 {
        actions.push(AuxAction::SweepDeadAgents);
    }
    rows.push(choice(
        "both".into(),
        "tabs and dead agents",
        tabs > 0 || dead > 0,
    ));
    if tabs > 0 || dead > 0 {
        actions.push(AuxAction::SweepBoth);
    }
    if actions.is_empty() {
        rows.push(PopupRow::Header("nothing to sweep".into()));
    }
    AuxPopup {
        popup: Popup::new(rows, Anchor::Center)
            .title("sweep threads")
            .footer("esc close"),
        actions,
    }
}

/// The operator tapped a choice: the modal named the counts, so the tap IS
/// the confirmation. Queue the apply for the run loop (or say why not).
fn begin_sweep_apply(view: &mut View, scope: SweepScope) {
    view.aux = None;
    if view.sweep_inflight {
        view.set_notice("a sweep is already running".into());
    } else {
        view.sweep_action = Some(SweepAction::Apply(scope));
    }
}

/// Run one `mux workspace prune` verb off the UI thread: a `--dry-run --json`
/// probe for the choice modal's counts, or a scoped apply whose JSON receipt
/// becomes the notice. Bounded so a wedged server cannot hold the UI loop's
/// sweep slot forever; the counts parse fails loud rather than opening a
/// modal with fabricated zeros.
async fn run_sweep_verb(action: SweepAction) -> SweepMsg {
    let mut command = crate::process_admission::tokio_command(crate::server::fno_bin());
    let mut args = vec![
        "mux".to_string(),
        "workspace".to_string(),
        "prune".to_string(),
        "--json".to_string(),
    ];
    let apply_secs = match action {
        SweepAction::Counts => {
            args.push("--dry-run".to_string());
            10
        }
        SweepAction::Apply(scope) => {
            match scope {
                SweepScope::Tabs => args.push("--tabs-only".to_string()),
                // (x-cf97) The opt-in half: tabs-only PLUS the flag that
                // widens the tab fold to spent shells. Never the default.
                SweepScope::UsedShells => {
                    args.push("--tabs-only".to_string());
                    args.push("--include-used-shells".to_string());
                }
                SweepScope::Dead => args.push("--dead-only".to_string()),
                // Both halves, and nothing else: bare prune would also remove
                // stale squad rows, which the modal never offered to remove.
                SweepScope::Both => {
                    args.push("--tabs-only".to_string());
                    args.push("--dead-only".to_string());
                }
            }
            // Each folded tab is one control roundtrip; a big workspace
            // sweep legitimately outlasts a modal probe.
            60
        }
    };
    command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let fut = crate::process_admission::tokio_output(&mut command);
    let output = match tokio::time::timeout(Duration::from_secs(apply_secs), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return SweepMsg::Failed(format!("prune spawn failed: {e}")),
        Err(_) => return SweepMsg::Failed("prune timed out".into()),
    };
    if !output.status.success() {
        return SweepMsg::Failed(format!(
            "prune exited {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(e) => return SweepMsg::Failed(format!("prune output unparseable ({e})")),
    };
    match action {
        SweepAction::Counts => {
            // A missing field means the two processes disagree about the JSON
            // shape (a stale deployed binary): fail the probe rather than open
            // a modal with fabricated zeros.
            let (Some(tabs), Some(dead)) = (
                parsed["tabs_would_close"].as_u64().map(|v| v as usize),
                parsed["members_reaped"].as_u64().map(|v| v as usize),
            ) else {
                return SweepMsg::Failed("prune output missing count fields".into());
            };
            // (x-cf97) The used-shell population rides every probe: the field
            // is missing only when the deployed CLI predates it, which is the
            // same two-process disagreement the tabs/dead reads refuse on -
            // but the refusal names the remedy instead of a dead end.
            // (review) A zero default would grey the row out and LIE about a
            // population the stale CLI cannot count, so the probe stays
            // fail-loud.
            let Some(used) = parsed["tabs_used_shells"].as_u64().map(|v| v as usize) else {
                return SweepMsg::Failed(
                    "prune output missing count fields - stale fno CLI? run fno doctor update"
                        .into(),
                );
            };
            if let Some(notice) = parsed["notice"].as_str() {
                if !notice.is_empty() {
                    return SweepMsg::Failed(notice.to_string());
                }
            }
            SweepMsg::Counts { tabs, dead, used }
        }
        SweepAction::Apply(_) => {
            let (Some(closed), Some(reaped)) = (
                parsed["tabs_closed"].as_u64().map(|v| v as usize),
                parsed["members_reaped"].as_u64().map(|v| v as usize),
            ) else {
                return SweepMsg::Failed("prune output missing count fields".into());
            };
            SweepMsg::Applied { closed, reaped }
        }
    }
}

impl View {
    fn new(term: (u16, u16), session: String, layout: LayoutView) -> Self {
        // Persisted per-section state wins (x-975a); pruned to the squads this
        // layout actually has, so a workspace deleted since the last run is
        // absent from the map (and so from the next write).
        // Load only - do NOT prune here. A real attach constructs the View with
        // an EMPTY placeholder layout and waits for the server's first push, so
        // pruning against it would delete every persisted entry before the
        // session ever learns what squads exist. `set_layout` owns the prune,
        // where a real squad list is in hand.
        // (x-c5ee) Load-only: the map holds persisted operator choices, nothing
        // else. The active-squad "open on first frame" default is no longer
        // seeded here - it lives in `section_view()`, computed live each frame so
        // a majority-exited squad can downgrade to LiveOnly as agents exit
        // (AC3-FR), which a one-time seed snapshot cannot.
        let section_view = view_store::load();
        // (x-b186) Layout-independent, unlike the section map above, so it is
        // safe to resolve against the empty placeholder layout a real attach
        // constructs with. A missing or corrupt store reads as the defaults.
        let (density, agent_sort, stored_width) = view_store::load_prefs();
        // (x-2e86) The stored intent, resolved once. Deliberately NOT clamped to
        // the current terminal here: `panel_w` clamps transiently on every paint,
        // and pre-clamping the stored value would let a small startup terminal
        // permanently shrink a width chosen on a large one (AC1-FR / Locked 4).
        // `None` (fresh install or corrupt width) falls back to the density's
        // canonical size, so nothing changes until the first drag.
        let sideline_width = stored_width.unwrap_or_else(|| canonical_width(density));
        View {
            backlog_pending: None,
            term,
            session,
            layout,
            frames: HashMap::new(),
            panel_on: true,
            density,
            sideline_width,
            agent_sort,
            status_on: true,
            resource_meter_on: false,
            resource_meter_text: None,
            resource_meter_refresh: 5,
            resource_meter_gate: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resource_meter_sampling: false,
            hint: false,
            keys_modal: None,
            pane_ids_until: None,
            keys_modal_esc: Vec::new(),
            row_menu: None,
            row_menu_esc: Vec::new(),
            aux: None,
            aux_esc: Vec::new(),
            section_view,
            section_chosen: HashMap::new(),
            idle_expanded: HashSet::new(),
            selector: None,
            sel_esc: Vec::new(),
            sel_hover_armed: false,
            sideline_offset: 0,
            answers: None,
            ans_esc: Vec::new(),
            needs_fold: None,
            mine_fold: None,
            needs_fold_at: None,
            needs_degraded: false,
            mine_degraded: false,
            mine_adding: None,
            mine_action: None,
            mine_acting: false,
            questions_fold: None,
            questions_degraded: false,
            question_answering: None,
            question_action: None,
            question_acting: false,
            needs_want: false,
            needs_inflight: false,
            needs_gen: 0,
            yard: None,
            yard_esc: Vec::new(),
            yard_fold: None,
            yard_fold_at: None,
            yard_degraded: false,
            yard_want: false,
            yard_inflight: false,
            yard_gen: 0,
            court: crate::court_overlay::Panel::default(),
            digest: None,
            notice: None,
            row_stamp: None,
            row_arm: None,
            row_slot: None,
            search: None,
            search_esc: Vec::new(),
            hover_focus: true,
            obsidian: crate::digest_overlay::ObsidianCfg::default(),
            show_missions: true,
            show_backlog: true,
            theme: Theme::default_theme(),
            settings_tab: SettingsTab::General,
            lane: LaneColorsUi::default(),
            hover_pending: None,
            link_hover: LinkHoverState::default(),
            hover_row: None,
            hover_seam: None,
            seam_drag: None,
            hover_sideline_border: false,
            sideline_drag: None,
            hover_grip: None,
            pane_drag: None,
            tab_drag: None,
            row_drag: None,
            press_hold: None,
            confirm: None,
            modal_release_swallow: false,
            create: None,
            create_esc: Vec::new(),
            rename: None,
            rename_esc: Vec::new(),
            move_to: None,
            pending_new_tab: None,
            marks: std::collections::HashSet::new(),
            recruit: None,
            recruit_esc: Vec::new(),
            move_pick: None,
            attach_place: None,
            portal_pick: None,
            sel_follow: None,
            nav: None,
            nav_esc: Vec::new(),
            peek: None,
            peek_esc: Vec::new(),
            peek_seq: 0,
            peek_input: None,
            peek_input_esc: Vec::new(),
            active_account: None,
            connections: None,
            conn_esc: Vec::new(),
            conn_want: false,
            conn_inflight: false,
            conn_gen: 0,
            conn_action: None,
            update_outcome: None,
            update_probe_want: false,
            update_probe_inflight: false,
            sweep_action: None,
            sweep_inflight: false,
        }
    }

    /// (x-84d7) Open the Connections modal in its loading state and arm the first
    /// read. Bumps the gen so any in-flight read from a prior open is discarded.
    fn open_connections(&mut self) {
        self.conn_gen = self.conn_gen.wrapping_add(1);
        let mut cv = crate::connections_view::ConnectionsView::new()
            .with_active_account(self.active_account.clone());
        cv.gen = self.conn_gen;
        self.connections = Some(cv);
        self.conn_esc.clear();
        self.conn_want = true; // the run loop spawns the fold at loop top
    }

    /// (x-84d7) Close the modal and bump the gen so a late read is dropped.
    fn close_connections(&mut self) {
        self.connections = None;
        self.conn_esc.clear();
        self.conn_gen = self.conn_gen.wrapping_add(1);
    }

    /// (x-84d7) Arm a fresh read (R refresh) under a new gen.
    fn refresh_connections(&mut self) {
        self.conn_gen = self.conn_gen.wrapping_add(1);
        if let Some(cv) = self.connections.as_mut() {
            cv.gen = self.conn_gen;
            cv.state = crate::connections_view::ModalState::Loading;
            cv.notice = None;
            // NB: do NOT clear `acting` here. A manual R during an in-flight
            // mutation must keep the single-flight guard up until the subprocess
            // actually exits, or a second use/register/update could overlap the
            // first and race two config/credential writes. The action-result arm
            // clears `acting` unconditionally on completion, so R can't wedge it.
        }
        self.conn_want = true;
    }

    /// (x-84d7) Re-read after a mutation: keep the current lists + the result
    /// notice visible (no Loading blank) while the fresh data folds in. This is
    /// the read-after-write that keeps the modal from trusting optimistic state.
    fn rearm_connections_read(&mut self) {
        self.conn_gen = self.conn_gen.wrapping_add(1);
        if let Some(cv) = self.connections.as_mut() {
            cv.gen = self.conn_gen;
        }
        self.conn_want = true;
    }

    /// The unified needs-me queue (x-feec), worst-first: the live badge leg
    /// (this session's blocked / done-unseen rows, instant from the layout)
    /// merged with the event-fold leg (`review_wedged` / `budget_stop`), each
    /// fold item joined to a roster row when one exists. Owned rows so a per-key
    /// mutation of `answers` never aliases the borrow (the reason the old
    /// blocked_queue cloned too). Sorted `(kind, ts, name)`.
    /// Name -> worst open need, for the attention sort's first term. Reuses
    /// `needs_queue()` (already worst-first sorted) rather than re-folding the
    /// event log, so the table and the needs-me overlay can never disagree
    /// about who needs the operator.
    fn attention_needs(&self) -> HashMap<String, NeedKind> {
        let mut worst: HashMap<String, NeedKind> = HashMap::new();
        for r in &self.needs_queue() {
            worst.entry(r.name.clone()).or_insert(r.kind);
        }
        worst
    }

    fn needs_queue(&self) -> Vec<NeedRow> {
        let mut rows: Vec<NeedRow> = Vec::new();

        // Leg 1: live badge rows from the current layout (no shell-out).
        for a in &self.layout.agents {
            if is_blocked_row(a) {
                let kind = if a.answerable.is_some() {
                    NeedKind::BlockedAnswerable
                } else {
                    NeedKind::BlockedFocusOnly
                };
                let reason = a.reason.clone().unwrap_or_else(|| {
                    if a.answerable.is_some() {
                        "needs an answer".into()
                    } else {
                        "needs focus".into()
                    }
                });
                rows.push(NeedRow {
                    kind,
                    name: a.name.clone(),
                    reason,
                    ts: String::new(),
                    id_key: a.name.clone(),
                    answerable: a.answerable.clone(),
                    pane_id: a.pane_id,
                    attach_id: a.attach_id.clone(),
                    squad: a.squad,
                    tab: a.tab,
                });
            } else if !a.exited
                && pane_state(a.badge, a.seen, a.pane_activity) == PaneState::DoneUnseen
            {
                rows.push(NeedRow {
                    kind: NeedKind::DoneUnseen,
                    name: a.name.clone(),
                    reason: a.reason.clone().unwrap_or_else(|| "done, unseen".into()),
                    ts: String::new(),
                    id_key: a.name.clone(),
                    answerable: None,
                    pane_id: a.pane_id,
                    attach_id: a.attach_id.clone(),
                    squad: a.squad,
                    tab: a.tab,
                });
            }
        }

        // Leg 2: event-fold items, joined to a roster row, else rendered
        // squadless when live, else dropped (a dead session's stale stop must
        // not nag forever - Locked 5).
        if let Some(items) = &self.needs_fold {
            for item in items {
                let kind = match item.kind.as_str() {
                    "review_wedged" => NeedKind::ReviewWedged,
                    "budget_stop" => NeedKind::BudgetStop,
                    "mail_question" => NeedKind::MailQuestion,
                    "operator_question" => NeedKind::Question,
                    "carveout_stale" | "stale_claims" => NeedKind::Decision,
                    _ => continue,
                };
                match self.join_fold_row(item) {
                    Some(a) => rows.push(NeedRow {
                        kind,
                        name: a.name.clone(),
                        reason: item.evidence.clone(),
                        ts: item.ts.clone(),
                        id_key: item.session_id.clone(),
                        answerable: None,
                        pane_id: a.pane_id,
                        attach_id: a.attach_id.clone(),
                        squad: a.squad,
                        tab: a.tab,
                    }),
                    None if item.live => rows.push(NeedRow {
                        kind,
                        name: item
                            .name
                            .clone()
                            .or_else(|| item.node.clone())
                            .unwrap_or_else(|| item.session_id.clone()),
                        reason: item.evidence.clone(),
                        ts: item.ts.clone(),
                        id_key: item.session_id.clone(),
                        answerable: None,
                        pane_id: None,
                        attach_id: None,
                        squad: None,
                        tab: None,
                    }),
                    None => {} // unjoined + not live: drop (stale-nag guard)
                }
            }
        }

        rows.sort_by(|a, b| {
            a.kind
                .cmp(&b.kind)
                .then_with(|| a.ts.cmp(&b.ts))
                .then_with(|| a.name.cmp(&b.name))
        });
        rows
    }

    /// `needs_queue()` filtered for the operator panel: a `MailQuestion` row is
    /// agent-to-agent mail traffic (`acme-web -> fno-peer: ...`), real signal
    /// for a live badge/sprite but not a question addressed to the operator.
    /// A `Question` row is the bare `operator_question` event - the panel
    /// renders the richer, separately-folded [`crate::needs_overlay::QuestionItem`]
    /// leg instead (asker/options/liveness), so rendering both would show the
    /// same open question twice. Both are dropped here; `needs_queue()` itself
    /// is untouched so badges and `fno-agents needs --json` still see them.
    fn needs_operator_queue(&self) -> Vec<NeedRow> {
        self.needs_queue()
            .into_iter()
            .filter(|r| !matches!(r.kind, NeedKind::MailQuestion | NeedKind::Question))
            .collect()
    }

    /// The roster row a fold item joins to: a name / node / session-id match
    /// against a layout row's name or its cwd basename (`cwd_base`, now carried
    /// on every row since x-6851 US3, not only orphans).
    fn join_fold_row(&self, item: &crate::needs_overlay::FoldItem) -> Option<&AgentRow> {
        let keys: Vec<&str> = [
            item.name.as_deref(),
            item.node.as_deref(),
            Some(item.session_id.as_str()),
        ]
        .into_iter()
        .flatten()
        .collect();
        self.layout.agents.iter().find(|a| {
            keys.iter().any(|k| a.name == *k)
                || a.cwd_base.as_deref().is_some_and(|c| keys.contains(&c))
        })
    }

    /// The two-lane overlay projection: MINE first (open before done, file
    /// order preserved within each group - the operator wrote that order),
    /// then the operator-filtered needs queue (already worst-first). Each
    /// lane is capped independently at ten so a noisy lane never crowds the
    /// other out; `*_total` carries the true count for the footer.
    fn needs_projection(&self) -> NeedsProjection {
        let mut mine: Vec<crate::needs_overlay::MineItem> =
            self.mine_fold.clone().unwrap_or_default();
        mine.sort_by_key(|m| m.done);
        let mine_total = mine.len();
        let mine_shown = mine.len().min(MINE_CAP);
        let mut rows: Vec<NeedsOverlayRow> = mine
            .into_iter()
            .take(MINE_CAP)
            .map(NeedsOverlayRow::Mine)
            .collect();

        // Questions lead the NEED section (x-f730 task 2.3): a real operator
        // question, with an asker to answer back to, outranks a bare
        // carveout/claims pile. Ranked by the record's own `rank` (x-7979
        // already orders these); an unranked row sorts last within the group
        // rather than floating to the front on a missing field.
        let mut questions: Vec<crate::needs_overlay::QuestionItem> =
            self.questions_fold.clone().unwrap_or_default();
        questions.sort_by_key(|q| q.rank.unwrap_or(u32::MAX));
        let need = self.needs_operator_queue();
        let need_total = questions.len() + need.len();
        let need_shown = need_total.min(NEEDS_CAP);
        rows.extend(
            questions
                .into_iter()
                .map(NeedsOverlayRow::Question)
                .chain(need.into_iter().map(NeedsOverlayRow::Need))
                .take(NEEDS_CAP),
        );

        NeedsProjection {
            rows,
            mine_shown,
            mine_total,
            need_shown,
            need_total,
        }
    }

    /// The THEY NEED YOU footer state: a failed fold degrades loudly
    /// (AC2-ERR), an unfetched fold reads as still folding, else it has
    /// landed. The lane is fed by two independent legs (events, questions);
    /// either one failing degrades the whole lane - a bare "half of what
    /// should be here loaded" is not worth rendering as a clean "as of now".
    fn needs_footer(&self) -> NeedsFooter {
        if self.needs_degraded || self.questions_degraded {
            NeedsFooter::Degraded
        } else if self.needs_fold.is_none() || self.questions_fold.is_none() {
            NeedsFooter::Folding
        } else {
            NeedsFooter::AsOf
        }
    }

    /// Same as [`Self::needs_footer`] for the MINE leg.
    fn mine_footer(&self) -> NeedsFooter {
        if self.mine_degraded {
            NeedsFooter::Degraded
        } else if self.mine_fold.is_none() {
            NeedsFooter::Folding
        } else {
            NeedsFooter::AsOf
        }
    }

    /// (x-f730 task 2.2) Apply a finished MINE mutation: success re-folds so
    /// the render reflects the file (the client never simulates the write
    /// itself - `mine_fold` is untouched here either way); a failure shows
    /// the reason as a notice and leaves everything - the file included -
    /// unchanged (AC3-ERR: never a silent no-op).
    fn apply_mine_action_result(&mut self, result: Result<(), String>) {
        self.mine_acting = false;
        match result {
            Ok(()) => self.needs_want = true,
            Err(msg) => self.set_notice(format!("mine: {msg}")),
        }
    }

    /// (x-f730 task 2.3) Same contract as [`Self::apply_mine_action_result`]
    /// for a question answer: success re-folds so the row leaves the queue on
    /// the next fold (AC1-HP), a failure shows the reason and leaves the
    /// question open (AC3-ERR).
    fn apply_question_action_result(&mut self, result: Result<(), String>) {
        self.question_acting = false;
        match result {
            Ok(()) => self.needs_want = true,
            Err(msg) => self.set_notice(format!("outstanding: {msg}")),
        }
    }

    /// (x-b2bf) The yard crowd: every roster agent as `(name, eye, crown)`,
    /// the eye derived from that row's own badge/need reading at render time
    /// and the crown read off the same wire field the sideline orders by.
    /// This is the whole multi-citizen surface - one glyph each, no sprite,
    /// so any density including Slim can hold it.
    fn yard_crowd(&self) -> Vec<(&str, crate::sprites::Eye, u32)> {
        let queue = self.needs_queue();
        self.layout
            .agents
            .iter()
            .map(|a| {
                let need = queue.iter().find(|r| r.name == a.name).map(|r| r.kind);
                (
                    a.name.as_str(),
                    yard_eye(a, need),
                    a.crown_level.unwrap_or(0),
                )
            })
            .collect()
    }

    /// (x-b2bf) The identity payload for a citizen, joined by name the same
    /// way the needs fold joins: first match on a display name. A
    /// display-name collision degrades to whichever citizen the fold listed
    /// first - noted, not fixed here.
    fn yard_identity(&self, name: &str) -> Option<&crate::yard_overlay::YardItem> {
        self.yard_fold
            .as_ref()
            .and_then(|f| f.iter().find(|c| c.name == name))
    }

    /// (x-b2bf) The selected yard citizen's name, for re-anchoring the
    /// spotlight across a layout push: the crowd follows `layout.agents`
    /// order, so a scrape that removes or reorders rows before the cursor
    /// would silently move the spotlight onto a different citizen (the same
    /// index-vs-identity trap the sideline cursor re-anchors out of).
    fn yard_selected_name(&self) -> Option<String> {
        // The None-check first: this runs on every layout push (set_layout
        // captures the name before the swap), so a closed yard must not pay
        // the crowd build.
        let yv = self.yard.as_ref()?;
        let crowd = self.yard_crowd();
        crowd
            .get(yv.sel.min(crowd.len().saturating_sub(1)))
            .map(|(name, _, _)| (*name).to_string())
    }

    /// (x-b2bf) The yard overlay's footer state, same three shapes as the
    /// needs fold's.
    fn yard_footer(&self) -> NeedsFooter {
        if self.yard_degraded {
            NeedsFooter::Degraded
        } else if self.yard_fold.is_none() {
            NeedsFooter::Folding
        } else {
            NeedsFooter::AsOf
        }
    }

    /// The identity of the currently-selected overlay row (MINE or needs), for
    /// re-anchoring the cursor across a layout push or fold merge (AC3-UI).
    fn answers_selected_id(&self) -> Option<NeedsOverlayId> {
        let cur = self.answers?;
        let projection = self.needs_projection();
        projection.rows.get(cur).map(NeedsOverlayRow::id)
    }

    /// Re-anchor the answer cursor to `prev` (its item identity) after the
    /// projection recomputed: keep it on the same item if still present, else
    /// clamp. The overlay stays open on an empty projection (the "nothing
    /// needs you" state, AC4-EDGE) with the cursor clamped to 0 so a later
    /// merge lands cleanly.
    fn reanchor_answers(&mut self, prev: Option<NeedsOverlayId>) {
        if self.answers.is_none() {
            return;
        }
        let projection = self.needs_projection();
        if projection.rows.is_empty() {
            self.answers = Some(0);
            return;
        }
        let idx = prev
            .and_then(|id| projection.rows.iter().position(|r| r.id() == id))
            .or(self.answers)
            .unwrap_or(0)
            .min(projection.rows.len() - 1);
        self.answers = Some(idx);
    }

    /// Open the new-workspace name overlay modally (x-9e5e): clear any
    /// keyboard-opened overlay first. `create_keys` is routed AFTER
    /// selector/answers in `handle_stdin`, so a lingering selector would
    /// otherwise swallow the typed name (codex peer review).
    fn open_create(&mut self) {
        self.selector = None;
        self.answers = None;
        self.yard = None;
        self.search = None;
        self.rename = None;
        self.move_pick = None;
        self.attach_place = None;
        self.portal_pick = None;
        self.nav = None;
        self.recruit = None;
        self.recruit_esc.clear();
        self.clear_peek();
        self.create = Some(String::new());
        self.create_esc.clear();
    }

    /// Open the recruit workspace-name overlay modally (x-8f11), clearing other
    /// keyboard-opened overlays first (x-260a). The marks are NOT cleared - they
    /// are the payload; Esc keeps them, a submit clears them.
    fn open_recruit(&mut self) {
        self.selector = None;
        self.answers = None;
        self.yard = None;
        self.search = None;
        self.rename = None;
        self.create = None;
        self.move_pick = None;
        self.attach_place = None;
        self.portal_pick = None;
        self.nav = None;
        self.confirm = None;
        self.clear_peek();
        self.recruit = Some(String::new());
        self.recruit_esc.clear();
    }

    /// Arm the card-dispatch confirm modally (x-a496) with the same
    /// overlay-clearing discipline as [`View::open_create`]: the confirm wins
    /// the stdin routing, so a selector left open behind it would swallow the
    /// keystrokes that follow the confirm's resolution (sigma review x-260a -
    /// reachable by mouse-clicking a card while prefix+w is open).
    fn open_confirm(&mut self, action: ConfirmAction) {
        self.selector = None;
        self.sel_hover_armed = false;
        self.answers = None;
        self.yard = None;
        self.search = None;
        // A half-typed workspace name is dropped too (gemini review): the
        // confirm owns the bottom row, and resuming a hidden create overlay
        // after the confirm resolves reads as a stuck client.
        self.create = None;
        self.create_esc.clear();
        // Same for a half-typed rename (x-c150): the confirm must not hide a
        // live text-input overlay whose next Enter it would steal.
        self.rename = None;
        self.rename_esc.clear();
        self.move_pick = None;
        self.attach_place = None;
        self.portal_pick = None;
        // A live navigator overlay (x-653d) must not linger behind the confirm:
        // it wins stdin routing after the confirm resolves and would swallow the
        // next keys, same reasoning as the selector above.
        self.nav = None;
        self.recruit = None;
        self.recruit_esc.clear();
        self.clear_peek();
        self.confirm = Some(action);
    }

    /// Open the move-to-position prompt for `tab` (x-cf97), clearing any other
    /// keyboard-opened overlay first - the same discipline as
    /// [`View::open_rename`], whose shape this prompt copies: a surface that
    /// forgets one of those clears leaves two overlays live at once.
    fn open_move_to(&mut self, tab: TabId) {
        self.selector = None;
        self.answers = None;
        self.yard = None;
        self.search = None;
        self.move_pick = None;
        self.attach_place = None;
        self.portal_pick = None;
        self.create = None;
        self.nav = None;
        self.recruit = None;
        self.recruit_esc.clear();
        self.clear_peek();
        self.move_to = Some((tab, String::new()));
    }

    /// The greatest tab id in the active squad (ids are monotonic + never
    /// reused, so the max is the newest tab), or `None` when the active squad
    /// is absent/empty (x-0f9d US1). Used only as the arm-time baseline.
    fn active_squad_max_tab_id(&self) -> Option<u64> {
        self.layout
            .squads
            .iter()
            .find(|s| s.id == self.layout.active_squad)
            .and_then(|s| s.tabs.iter().map(|t| t.id).max())
    }

    /// The id of the active squad's ACTIVE tab (x-0f9d US1). For the active
    /// squad the server projects `active_tab` per-viewer, and a NewTab switches
    /// only its sender to the new tab (Locked 3), so this identifies THIS
    /// client's own newly-created tab - not a concurrent tab another client
    /// created in the same squad (codex review).
    fn active_squad_active_tab_id(&self) -> Option<u64> {
        self.layout
            .squads
            .iter()
            .find(|s| s.id == self.layout.active_squad)
            .and_then(|s| s.tabs.get(s.active_tab).map(|t| t.id))
    }

    /// Arm the create-time name prompt for the NEXT tab (x-0f9d US1): a bare
    /// NewTab (keyboard `c`, strip `+`) records the current newest tab id so
    /// the layout that adds a higher one opens rename on it. Other commands are
    /// ignored, so only an explicit create arms the prompt.
    fn note_command_sent(&mut self, cmd: &Command) {
        if matches!(cmd, Command::NewTab) {
            self.pending_new_tab = Some(self.active_squad_max_tab_id());
        }
    }

    /// If a create-time name prompt is armed and THIS client's active tab is a
    /// newly-minted one (its id is beyond the arm-time baseline), open the
    /// x-c150 rename overlay on it (x-0f9d US1): type + Enter names it, Esc /
    /// empty Enter leaves it unnamed. Called from [`View::set_layout`] after the
    /// swap. Keying on the active tab (not the max id) means a concurrent tab
    /// another client created in the same squad never steals this prompt.
    fn maybe_prompt_new_tab_name(&mut self) {
        if let Some(baseline) = self.pending_new_tab {
            let fresh = match (baseline, self.active_squad_active_tab_id()) {
                // No tabs before -> any active tab that now exists is the new one.
                (None, Some(active_id)) => Some(active_id),
                // Had tabs -> the sender's active tab is the new one iff its id
                // is beyond the baseline (the only id > baseline is the fresh
                // tab; a scrape tick still on the old active tab does not fire).
                (Some(prev), Some(active_id)) if active_id > prev => Some(active_id),
                _ => None,
            };
            if let Some(new_id) = fresh {
                self.pending_new_tab = None;
                self.open_rename(RenameTarget::Tab(new_id));
            }
        }
    }

    /// Open the move-tab-to-squad picker modally for `tab` (x-96e8), listing the
    /// candidate destination squads (source excluded). Same overlay-clearing
    /// discipline as the others. The cursor starts at the top; the list is no
    /// longer capped at the digit range, so rows past nine are cursor-only.
    fn open_move_pick(&mut self, src: MoveSrc, squads: Vec<u64>) {
        self.selector = None;
        self.answers = None;
        self.search = None;
        self.create = None;
        self.rename = None;
        self.confirm = None;
        self.nav = None;
        self.recruit = None;
        self.recruit_esc.clear();
        self.attach_place = None;
        self.portal_pick = None;
        self.clear_peek();
        self.move_pick = Some(MovePick::new(src, squads));
    }

    /// Clear the read-only peek overlay (x-c376) and its escape carry. Called by
    /// every modal `open_*` helper so a mouse-driven overlay open (the mouse
    /// pre-pass runs before overlay routing) never leaves peek rendering on top.
    fn clear_peek(&mut self) {
        self.peek = None;
        self.peek_esc.clear();
        // (x-9c5f) The reply input lives inside peek; closing peek drops it too.
        self.peek_input = None;
        self.peek_input_esc.clear();
    }

    /// Open the which-key keybinds modal (prefix+?, x-8ccf US3). Clears peek like
    /// every other overlay open so a mouse-driven open never leaves peek on top.
    fn open_keys_modal(&mut self) {
        self.clear_peek();
        self.keys_modal = Some(build_keys_modal());
        self.keys_modal_esc.clear();
    }

    fn reveal_pane_ids_at(&mut self, now: Instant) {
        self.pane_ids_until = Some(now + PANE_ID_REVEAL_WINDOW);
    }

    /// The flat popup target under a screen cell while the modal is open, for
    /// mouse hover/click. Renders the modal (windowed by the live scroll) and
    /// walks the visible line's hit spans; `None` off the popup.
    fn keys_modal_hit(&self, row: u16, col: u16) -> Option<usize> {
        let m = self.keys_modal.as_ref()?;
        let r = m.popup.render(self.term);
        let (r0, c0) = r.origin;
        let li = (row as usize).checked_sub(r0)?;
        let line = r.lines.get(li)?;
        let cc = (col as usize).checked_sub(c0)?;
        // The footer's close target is not a row index; returning it here would
        // clamp `select(usize::MAX)` onto the LAST entry on a hover sweep.
        line.hits
            .iter()
            .find(|(t, off, len)| {
                *t != crate::chrome::ESC_CLOSE_HIT && cc >= *off && cc < *off + *len
            })
            .map(|(t, _, _)| *t)
    }

    /// Keep the selected modal row inside the scrolled viewport after an arrow
    /// move, by delegating to the ONE implementation.
    ///
    /// This used to hand-roll the scroll arithmetic with `rows.len().min(trows)`,
    /// the pre-chrome viewport formula. `Popup::viewport_h` now subtracts
    /// `chrome.rows_overhead()`, so the copy drifted the moment overlays gained
    /// a frame: on a 24-row terminal it believed it had 24 body rows when it had
    /// 22, and arrowing far enough parked the highlighted row permanently below
    /// the fold while Enter still executed it. That is the invisible-Enter-target
    /// case `follow_sel` exists to prevent, reached through the copy that did not
    /// get the fix. `clamp_sel_to_view` never drifted because it already routed
    /// through `viewport_h`.
    fn follow_modal_selection(&mut self) {
        let trows = self.term.0.max(1) as usize;
        if let Some(m) = self.keys_modal.as_mut() {
            m.popup.follow_sel(trows);
        }
    }

    /// (x-1d91) Every queue card the graph holds, cap included - the sum of the
    /// per-lane counts, so the section's remainder and the kanban's lane headers
    /// are the same number twice rather than two independent claims.
    fn backlog_total(&self) -> usize {
        self.layout.backlog_lanes.iter().map(|(_, n)| n).sum()
    }

    /// (x-1d91) Open the mini-kanban over the Backlog section.
    fn open_kanban(&mut self, anchor: Anchor) {
        self.clear_peek();
        self.aux = Some(build_kanban(
            &self.layout.backlog,
            &self.layout.backlog_lanes,
            anchor,
        ));
        self.aux_esc.clear();
    }

    /// (x-1d91) Whether this card is wearing the dispatched-verb `…` marker.
    fn card_pending(&self, id: &str) -> bool {
        self.backlog_pending.as_ref().is_some_and(|p| p.node == id)
    }

    /// Arm the pending marker for a dispatched reorder verb, snapshotting what
    /// the TARGET card looked like at dispatch. Returns `false` when one is
    /// already in flight - the double-press guard, so a second Enter on the same
    /// card cannot fire a duplicate shellout (and a no-op second `rank --top`
    /// cannot churn the graph).
    fn arm_backlog_pending(&mut self, node: &str, verb: BacklogVerb) -> bool {
        if self.backlog_pending.is_some() {
            return false;
        }
        self.backlog_pending = Some(BacklogPending {
            node: node.to_string(),
            verb,
            was: card_mark(&self.layout.backlog, node),
            deadline: Instant::now() + BACKLOG_PENDING_TTL,
        });
        true
    }

    /// Clear the pending marker once the feed confirms THIS verb landed: the
    /// target card's own position or state changed, or it left the feed (what a
    /// successful defer looks like). Called with the INCOMING backlog before it
    /// is stored.
    ///
    /// Deliberately narrower than "the card set changed at all": claims and
    /// routing fields churn the set on unrelated cards every few seconds, so a
    /// whole-set comparison would clear the marker on someone else's news and
    /// release the single-flight guard while this verb was still running - a
    /// false confirmation, which is the one thing this marker exists to prevent.
    fn confirm_backlog_pending(&mut self, incoming: &[BacklogCard]) {
        let landed = self
            .backlog_pending
            .as_ref()
            .is_some_and(|p| card_mark(incoming, &p.node) != p.was);
        if landed {
            self.backlog_pending = None;
        }
    }

    /// Clear the pending marker because the verb reported its own outcome. The
    /// server routes each verb's verdict back as one notice to the requesting
    /// client, so a notice arriving mid-verb is that verdict: the marker must go
    /// rather than spin out its full timeout and then replace a specific failure
    /// ("rank x-a: lock contention") with a generic one. Clearing early on an
    /// unrelated notice is harmless - the rendered order is never optimistic, so
    /// the marker is the only thing at stake.
    fn settle_backlog_pending_on_notice(&mut self) {
        self.backlog_pending = None;
    }

    /// The pending marker's expiry deadline, for the select loop's timer arm.
    fn backlog_pending_deadline(&self) -> Option<Instant> {
        self.backlog_pending.as_ref().map(|p| p.deadline)
    }

    /// Declare an unconfirmed verb lost: clear the marker and say so. The row
    /// must never keep a `…` the feed will not resolve, and silence would read
    /// as success (this is the same fail-loud stance as the verb's own error
    /// notice - the order is already truthful; only the marker was a claim).
    fn expire_backlog_pending(&mut self) {
        if let Some(p) = self.backlog_pending.take() {
            self.set_notice(format!("{} {}: no confirmation", p.verb.label(), p.node));
        }
    }

    /// The candidate destination squads for a Move-to-workspace gesture on a row
    /// owned by `own`: every other non-mission workspace. Shared by the menu
    /// entry's construction, its dispatch, and the tab-move picker so the three
    /// cannot drift on what counts as a destination.
    ///
    /// It used to `.take(9)`, the digit range. At 14 workspaces that dropped
    /// five with no notice, and a list that silently omits its tail is worse
    /// than one that admits it: the operator cannot tell a missing workspace
    /// from a gone one. The digit accelerator is still nine wide; the LIST is
    /// not, and the pickers carry a cursor to reach the rest.
    fn move_dst_squads(&self, own: Option<u64>) -> Vec<u64> {
        self.layout
            .squads
            .iter()
            .map(|s| s.id)
            .filter(|id| !is_mission_squad(*id) && Some(*id) != own)
            .collect()
    }

    /// The candidate destination workspaces for an attach placement: every
    /// non-mission workspace. A synthetic mission squad is a render-time
    /// grouping header, not a real squad `place_spawned_pane` can route into,
    /// so it is excluded here rather than at each call site.
    ///
    /// Both entry paths into the placement picker (the click path through
    /// `apply_hit` and the keyboard path through `p`/Enter) built this list
    /// independently and identically. Two constructions of one list is the
    /// N-reachable-paths trap in miniature: the mission-squad exclusion had
    /// already been fixed twice, and the `.take(9)` cap had to be removed
    /// twice. Now there is one.
    fn attach_dst_squads(&self) -> Vec<u64> {
        self.layout
            .squads
            .iter()
            .map(|s| s.id)
            .filter(|id| !is_mission_squad(*id))
            .collect()
    }

    /// Open the row context menu on `display_rows()` index `i`, anchored at
    /// `anchor` (x-8ccf US2): the agent lifecycle menu, (x-1d91) the Backlog
    /// card's reorder menu, or (x-f300) a section header's clear-dead menu (a
    /// squad name row or a `~` band). Returns whether it opened - `false` for a
    /// row with no menu, which the caller turns into "close whatever is open".
    fn open_row_menu(&mut self, i: usize, anchor: Anchor) -> bool {
        enum Pick {
            Menu(Box<RowMenu>),
            Section(SectionKey, String, Option<u64>),
        }
        // Resolve what the row needs while `display_rows()` holds the borrow, so
        // the section arm below is free to mutate `self`.
        let pick = match self.display_rows().get(i) {
            Some(DisplayRow::Agent(a)) => {
                let mut menu = build_row_menu(a, anchor);
                // A pane-hosted row can relocate its live pane into another
                // workspace; a paneless row already gets the `p` placement
                // picker. Append the entry only when another non-mission
                // workspace exists, so it never offers a move to nowhere. Built
                // here (where the layout is) rather than in build_row_menu so
                // the per-state builder stays layout-free and its direct tests
                // stay untouched.
                if a.pane_id.is_some() {
                    let move_dsts = self.move_dst_squads(a.squad);
                    if !move_dsts.is_empty() {
                        menu.popup.rows.push(PopupRow::Rule);
                        menu.popup.rows.push(PopupRow::Entry {
                            glyph: "↪".into(),
                            label: "Move to workspace".into(),
                            hint: String::new(),
                            enabled: true,
                        });
                        menu.actions.push(MenuAction::MoveToWorkspace);
                    }
                }
                Some(Pick::Menu(Box::new(menu)))
            }
            // (x-1d91) A Backlog card gets the reorder menu.
            Some(DisplayRow::Card(c)) => Some(Pick::Menu(Box::new(build_card_menu(
                c,
                &self.obsidian,
                anchor,
            )))),
            Some(DisplayRow::Sel(row)) if row.tab.is_none() => squad_key(&self.layout, row.squad)
                .map(|key| {
                    let label = self
                        .layout
                        .squads
                        .iter()
                        .find(|s| s.id == row.squad)
                        .map(|s| s.name.clone())
                        .unwrap_or_default();
                    Pick::Section(key, label, Some(row.squad))
                }),
            Some(DisplayRow::Header { key, label, .. }) => {
                Some(Pick::Section(key.clone(), (*label).to_string(), None))
            }
            _ => None,
        };
        match pick {
            Some(Pick::Menu(m)) => {
                self.clear_peek();
                self.row_menu = Some(*m);
                self.row_menu_esc.clear();
                true
            }
            Some(Pick::Section(key, label, squad)) => {
                // Cards have no exited state, so the Backlog section has no
                // menu at all - a notice there would imply "none right now"
                // about a section that can never have any.
                if key == SectionKey::WorkQueue {
                    return false;
                }
                // A section with nothing to clear would leave a one-entry menu
                // whose only entry is a no-op; say so instead (the row menu's
                // "no dead item ever renders" rule, applied to the whole menu).
                // "nothing to clear" covers both an all-live section and a key
                // `section_dead_rows` refused as ambiguous - it never claims
                // there are no dead rows when the truth is we won't guess which.
                let dead = self.section_dead_rows(&key, squad).len();
                // A workspace section always has a menu (it can be renamed). A
                // non-workspace header (Elsewhere/Mission) with nothing to clear
                // says so rather than opening a one-entry no-op menu.
                if dead == 0 && squad.is_none() {
                    self.set_notice(format!("no dead rows in {label}"));
                    return false;
                }
                self.clear_peek();
                self.row_menu = Some(build_section_menu(key, label, squad, dead, anchor));
                self.row_menu_esc.clear();
                true
            }
            None => false,
        }
    }

    /// (x-92d3 5.1) Open the tab-strip context menu on the tab cell at
    /// `(row, col)`, through the same `tab_cell_at` the drag pickup uses.
    /// `false` (and no swallow) for the `+` NewTab cell, a notice-overlay cell,
    /// or anything off the strip: those cells are not a tab, so the press
    /// falls through exactly as it did before the menu existed (AC4).
    fn open_tab_menu(&mut self, row: u16, col: u16, anchor: Anchor) -> bool {
        let Some(tid) = self.tab_cell_at(row, col) else {
            return false;
        };
        self.open_tab_menu_by_id(tid, anchor)
    }

    /// (x-7683) Open the tab menu pinned to a stable [`TabId`] - the ONE
    /// menu-open sequence (build, clear peek, reset the esc buffer) behind
    /// the cell-resolved right-press, the long-press release arm, and the
    /// dead-drag reaper, so the three paths can never drift apart.
    /// `false` (nothing opened) for an unknown/closed tab.
    fn open_tab_menu_by_id(&mut self, tid: TabId, anchor: Anchor) -> bool {
        // Clone the tab out of the layout borrow before mutating `self`
        // (menu open is cold; the clone is one small Vec). Unreachable today -
        // `tab_cell_at` walks spans minted from this same layout with no await
        // between - kept as the fail-closed shape if spans are ever cached.
        let Some((idx, tab)) = self.find_tab(tid).map(|(_, i, t)| (i, t.clone())) else {
            return false;
        };
        self.clear_peek();
        self.row_menu = Some(build_tab_menu(idx, &tab, anchor));
        self.row_menu_esc.clear();
        true
    }

    /// A tab id resolved against the LIVE layout: its squad, its ordinal
    /// within that squad (for the strip's label form), and the `TabMeta`
    /// itself. `None` for an unknown/closed tab - the stale-target refusal
    /// every menu action shares.
    fn find_tab(&self, tid: TabId) -> Option<(u64, usize, &TabMeta)> {
        self.layout.squads.iter().find_map(|s| {
            s.tabs
                .iter()
                .enumerate()
                .find(|(_, t)| t.id == tid)
                .map(|(i, t)| (s.id, i, t))
        })
    }

    /// (x-7683) The `display_rows()` index of the agent row that owns `pid`, or
    /// `None` when no visible row hosts that pane (a scratch pane, or a roster
    /// that no longer carries it). A pane-cell right-press resolves its menu
    /// through this, so a pane and its sideline row can never disagree about
    /// whose menu opens. Cold-path only (menu open), like `open_row_menu`.
    fn agent_row_index_for_pane(&self, pid: u64) -> Option<usize> {
        self.display_rows()
            .iter()
            .position(|r| matches!(r, DisplayRow::Agent(a) if a.pane_id == Some(pid)))
    }

    /// (x-7683) Whether any mode-owning overlay is open. The PANE context
    /// menu refuses to open over one: most overlays never intercepted the
    /// mouse, so a pane press fell through to the pane, and open_row_menu
    /// clears only peek - a menu opening under rename would steal the
    /// overlay's keys, since the key router checks row_menu first.
    fn overlay_open(&self) -> bool {
        self.menu_usurping_open()
            || self.answers.is_some()
            || self.yard.is_some()
            || self.peek.is_some()
            || self.digest.is_some()
    }

    /// (x-7683) The narrower guard for the ROW/TAB menu paths (right-press
    /// and long-press alike): only overlays a menu would actually USURP -
    /// text inputs (rename/create/recruit/search/peek_input/nav, whose typed
    /// buffer the menu would orphan) and overlays that own live keys routed
    /// after row_menu (answers dispatches PaneAnswer digits and goto/close on
    /// Enter; yard takes n/N/q; digest swallows its next key dismissing
    /// itself). None of those is cleared on menu-open, so a menu over one
    /// steals its keys. `peek` is the one deliberate absence: the menu-open
    /// path clears peek itself, so the two can never stack - a right-press
    /// on a sideline row opened the row menu over an open peek BEFORE this
    /// diff, and that behavior must survive the guard.
    fn menu_usurping_open(&self) -> bool {
        self.keys_modal.is_some()
            || self.row_menu.is_some()
            || self.aux.is_some()
            || self.connections.is_some()
            || self.confirm.is_some()
            || self.move_pick.is_some()
            || self.attach_place.is_some()
            || self.portal_pick.is_some()
            || self.create.is_some()
            || self.rename.is_some()
            || self.move_to.is_some()
            || self.recruit.is_some()
            || self.search.is_some()
            || self.peek_input.is_some()
            || self.nav.is_some()
            || self.answers.is_some()
            || self.yard.is_some()
            || self.digest.is_some()
    }

    /// (x-7683) Open the owning agent's row menu for the pane under
    /// `(row, col)`, the one shared sequence behind the fresh right-press and
    /// the in-menu re-anchor (one implementation of "a pane is
    /// menu-bearing", so the two paths can never disagree about whose menu
    /// opens). `false` when the cell is no pane, the pane has no agent row,
    /// or the row has no menu - the caller leaves the press to fall through.
    fn open_pane_menu(&mut self, row: u16, col: u16) -> bool {
        let Some((pane, _, _)) = self.hit_test(row, col) else {
            return false;
        };
        self.agent_row_index_for_pane(pane)
            .is_some_and(|i| self.open_row_menu(i, Anchor::At { row, col }))
    }

    /// (x-7683) Open the context menu for whichever new drag is live, pinned
    /// to its captured source - the dead-drag reaper's path, which has no
    /// press cell to hit-test. A motionless hold past MENU_LONG_PRESS emits
    /// no events at all, so the reaper at PANE_DRAG_TIMEOUT is the only
    /// thing that can fire for a hold longer than the drag timeout; without
    /// this the advertised hold silently stops working for exactly the most
    /// deliberate holds. `false` (drag left untouched for the reaper) when
    /// no drag is live, it moved, or it has not qualified yet.
    fn open_drag_menu(&mut self) -> bool {
        // Guarded like the release paths: a menu materializing under a
        // usurping overlay (rename typing, an open picker) would steal its
        // keys - the reaper's caller cancels the drag instead, as it did
        // before this menu existed.
        if self.menu_usurping_open() {
            return false;
        }
        if let Some(d) = self.tab_drag {
            if held_long_enough(d.start_at, d.moved) {
                if self.open_tab_menu_by_id(d.src_tab, Anchor::Center) {
                    self.tab_drag = None;
                    return true;
                }
                // The held tab closed before the reaper fired - say so, same
                // as the release-path long-press arms, rather than let a
                // qualifying hold end in silence.
                self.set_notice("no menu on the held tab".into());
            }
            return false;
        }
        if let Some(d) = self.row_drag.as_ref() {
            if !held_long_enough(d.start_at, d.moved) {
                return false;
            }
            let idx = match &d.src {
                RowSource::Pane(pid) => self.agent_row_index_for_pane(*pid),
                RowSource::Attach(id) => self.display_rows().iter().position(
                    |r| matches!(r, DisplayRow::Agent(a) if a.attach_id.as_deref() == Some(id)),
                ),
            };
            if let Some(i) = idx {
                if self.open_row_menu(i, Anchor::Center) {
                    self.row_drag = None;
                    return true;
                }
            }
            self.set_notice("no menu on the held row".into());
            return false;
        }
        // (x-b465) The press-hold latch gets the same treatment, and needs it
        // more: a hold on a workspace row emits no events at all while it is
        // held, so without this the menu waits for the release. Identity
        // re-checked here too - the reaper fires precisely when a layout push
        // is most likely to have landed under the held pointer.
        if let Some((i, id, start)) = self.press_hold.clone() {
            if !held_long_enough(start, false) {
                return false;
            }
            self.press_hold = None;
            if self.row_identity(i).as_ref() != Some(&id) {
                self.set_notice("the held row moved".into());
                return false;
            }
            if self.open_row_menu(i, Anchor::Center) {
                return true;
            }
            self.set_notice("no menu on the held row".into());
        }
        false
    }

    /// The flat popup target under a screen cell while the row menu is open, for
    /// mouse hover/click; `None` off the popup.
    fn row_menu_hit(&self, row: u16, col: u16) -> Option<usize> {
        let m = self.row_menu.as_ref()?;
        let r = m.popup.render(self.term);
        let (r0, c0) = r.origin;
        let li = (row as usize).checked_sub(r0)?;
        let line = r.lines.get(li)?;
        let cc = (col as usize).checked_sub(c0)?;
        // The footer's close target is not a row index; returning it here would
        // clamp `select(usize::MAX)` onto the LAST entry on a hover sweep.
        line.hits
            .iter()
            .find(|(t, off, len)| {
                *t != crate::chrome::ESC_CLOSE_HIT && cc >= *off && cc < *off + *len
            })
            .map(|(t, _, _)| *t)
    }

    /// Open the sideline MENU popup anchored at `anchor` (x-8ccf US4). Also
    /// re-arms the update-readiness probe so a menu opened long
    /// after the last probe is never showing a stale answer; the menu itself
    /// still renders instantly from whatever outcome is already in hand.
    fn open_sideline_menu(&mut self, anchor: Anchor) {
        self.clear_peek();
        self.aux = Some(build_sideline_menu(anchor, self.update_outcome.as_ref()));
        self.aux_esc.clear();
        self.update_probe_want = true;
    }

    /// Rebuild an already-open sideline MENU so a landing update probe shows
    /// up (or clears a stale row) without the operator closing and reopening
    /// it (P2, codex on PR #881). A no-op when the open aux is some other
    /// popup (settings, kanban, the update overlay itself): `OpenKeybinds` is
    /// pushed only by `build_sideline_menu`, so its presence is the marker.
    /// Preserves selection the same way `reopen_settings_keeping_sel` does.
    fn refresh_open_sideline_menu(&mut self) {
        let Some(aux) = self.aux.as_ref() else {
            return;
        };
        if !aux.actions.contains(&AuxAction::OpenKeybinds) {
            return;
        }
        let anchor = aux.popup.anchor;
        let sel = aux.popup.sel;
        let mut menu = build_sideline_menu(anchor, self.update_outcome.as_ref());
        let n = menu.popup.targets().len();
        menu.popup.sel = if n > 0 { sel.min(n - 1) } else { 0 };
        self.aux = Some(menu);
    }

    /// Build the settings modal: general toggles plus theme and prefix pickers.
    fn build_settings_modal(&self) -> AuxPopup {
        let tab = self.settings_tab;
        let mut rows = Vec::new();
        let mut actions: Vec<AuxAction> = Vec::new();
        match tab {
            SettingsTab::General => {
                let toggle = |on: bool, label: &str| PopupRow::Entry {
                    glyph: if on { "☑".into() } else { "☐".into() },
                    label: label.into(),
                    hint: String::new(),
                    enabled: true,
                };
                rows.push(toggle(self.hover_focus, "focus follows mouse"));
                rows.push(toggle(self.status_on, "status row"));
                rows.push(toggle(
                    self.resource_meter_on,
                    "resource meter (needs macmon)",
                ));
                actions.push(AuxAction::ToggleHoverFocus);
                actions.push(AuxAction::ToggleStatus);
                actions.push(AuxAction::ToggleResourceMeter);
            }
            SettingsTab::Theme => {
                // The four shipped palettes; the active one is marked. Enter on a
                // name applies it (an explicit action, not a cursor-move preview).
                for name in crate::theme::THEME_NAMES {
                    let active = self.theme.name == name;
                    rows.push(PopupRow::Entry {
                        glyph: if active { "●".into() } else { "○".into() },
                        label: name.into(),
                        hint: if active {
                            "active".into()
                        } else {
                            String::new()
                        },
                        enabled: true,
                    });
                    actions.push(AuxAction::ApplyTheme(name.into()));
                }
            }
            SettingsTab::Keys => {
                (rows, actions) = build_prefix_settings_rows(&crate::keys::prefix_display());
            }
            SettingsTab::Colors => {
                (rows, actions) = crate::lane_colors_panel::build_lane_color_rows(
                    crate::sideline_color::palette(),
                    &self.lane,
                );
            }
        }
        let popup = Popup::new(rows, Anchor::Center)
            .title("settings")
            .tabs(vec![
                ("general".to_string(), tab == SettingsTab::General),
                ("theme".to_string(), tab == SettingsTab::Theme),
                ("keys".to_string(), tab == SettingsTab::Keys),
                ("colors".to_string(), tab == SettingsTab::Colors),
            ])
            .footer("tab switches section · esc close");
        AuxPopup { popup, actions }
    }

    /// Rebuild the settings modal after a toggle so its glyph reflects the new
    /// state, preserving the current selection (a keyboard toggle must re-toggle
    /// the SAME row on the next Enter, not reset to row 0).
    fn reopen_settings_keeping_sel(&mut self) {
        let sel = self.aux.as_ref().map(|m| m.popup.sel).unwrap_or(0);
        let mut modal = self.build_settings_modal();
        let n = modal.popup.targets().len();
        modal.popup.sel = if n > 0 { sel.min(n - 1) } else { 0 };
        self.aux = Some(modal);
    }

    /// The flat popup target under a screen cell while an aux popup is open.
    fn aux_hit(&self, row: u16, col: u16) -> Option<usize> {
        let m = self.aux.as_ref()?;
        let r = m.popup.render(self.term);
        let (r0, c0) = r.origin;
        let li = (row as usize).checked_sub(r0)?;
        let line = r.lines.get(li)?;
        let cc = (col as usize).checked_sub(c0)?;
        // The footer's close target is not a row index; returning it here would
        // clamp `select(usize::MAX)` onto the LAST entry on a hover sweep.
        line.hits
            .iter()
            .find(|(t, off, len)| {
                *t != crate::chrome::ESC_CLOSE_HIT && cc >= *off && cc < *off + *len
            })
            .map(|(t, _, _)| *t)
    }

    // (x-f75e) In-block guards for the three popup click routers. A click that
    // hits no target used to read as "off the popup" and dismiss, so clicking a
    // Header (no target) closed the menu. These distinguish an in-block miss
    // (swallow) from an off-block click (dismiss) - the same fix at every site,
    // since guarding one router of three leaves the bug live on the other two.
    fn row_menu_block_contains(&self, row: u16, col: u16) -> bool {
        self.row_menu
            .as_ref()
            .map(|m| m.popup.render(self.term).contains(row, col))
            .unwrap_or(false)
    }
    fn aux_block_contains(&self, row: u16, col: u16) -> bool {
        self.aux
            .as_ref()
            .map(|m| m.popup.render(self.term).contains(row, col))
            .unwrap_or(false)
    }
    fn keys_modal_block_contains(&self, row: u16, col: u16) -> bool {
        self.keys_modal
            .as_ref()
            .map(|m| m.popup.render(self.term).contains(row, col))
            .unwrap_or(false)
    }

    /// True when `(row, col)` lands on any of a popup's `esc`-close hit spans:
    /// the footer's `esc close` words, the Full title bar's ` esc ` chip, or a
    /// Bare menu's inline bottom-border chip (x-020d) - every one of them is
    /// `ESC_CLOSE_HIT`-tagged by `chrome::frame`/`top_border`/`bottom_border`,
    /// so one generic scan over the rendered line's hits covers all three.
    /// Checked BEFORE the entry hit routers so a close target is never
    /// mistaken for a row index.
    fn chrome_close_hit(&self, popup: &Popup, row: u16, col: u16) -> bool {
        let r = popup.render(self.term);
        let (r0, c0) = r.origin;
        let (Some(li), Some(cc)) = (
            (row as usize).checked_sub(r0),
            (col as usize).checked_sub(c0),
        ) else {
            return false;
        };
        r.lines.get(li).is_some_and(|line| {
            line.hits.iter().any(|(t, off, len)| {
                *t == crate::chrome::ESC_CLOSE_HIT && cc >= *off && cc < *off + *len
            })
        })
    }

    fn active_overlay_layout(&self) -> Option<OverlayLayout> {
        let rows = self.term.0 as usize;
        if let Some(action) = &self.confirm {
            return Some(self.confirm_overlay_layout(rows, action));
        }
        if let Some(name) = &self.create {
            return Some(self.name_modal_layout("new workspace", name, None));
        }
        if let Some((target, name)) = &self.rename {
            let noun = match target {
                RenameTarget::Tab(_) => "tab",
                RenameTarget::Squad(_) => "workspace",
                RenameTarget::Agent(_) => "row",
            };
            // An agent label is never derived, so there is no auto to reset
            // to: the hint names the grammar instead of the blank-clears
            // semantics the tab/squad targets share.
            let hint = match target {
                RenameTarget::Agent(_) => Some("a-z 0-9 - _ (1-64 chars)"),
                _ => Some("empty resets to auto"),
            };
            return Some(self.name_modal_layout(&format!("rename {noun}"), name, hint));
        }
        if let Some(name) = &self.recruit {
            return Some(self.name_modal_layout(
                &format!("recruit {} into", self.marks.len()),
                name,
                Some("create-if-absent"),
            ));
        }
        // The two Full-chrome overlays below draw through the same
        // layout_lines_overlay pair (draw arm + this hit-test layout), so the
        // footer's esc close words resolve to the glyph they paint. Order
        // mirrors the draw chain: connections paints above peek.
        if let Some(conn) = &self.connections {
            return Some(self.connections_overlay_layout(rows, conn));
        }
        if let Some(peek) = &self.peek {
            return Some(self.peek_overlay_layout(rows, peek));
        }
        None
    }

    fn cancel_active_overlay(&mut self) {
        if self.confirm.take().is_some() {
            return;
        }
        if self.create.take().is_some() {
            self.create_esc.clear();
            return;
        }
        if self.rename.take().is_some() {
            self.rename_esc.clear();
            return;
        }
        if self.recruit.take().is_some() {
            self.recruit_esc.clear();
            return;
        }
        if self.connections.take().is_some() {
            return;
        }
        if self.peek.is_some() {
            self.clear_peek();
        }
    }

    /// Apply a `PeekBody` under the seq guard (x-c376, AC1-FR): store `lines`
    /// only when peek is open AND `seq` is the current request. Returns whether
    /// it applied (the caller redraws on true). A stale body (any other seq) is
    /// dropped, so a peek moved on to another row never shows the prior row's
    /// transcript.
    fn apply_peek_body(&mut self, seq: u64, lines: Vec<String>) -> bool {
        match self.peek.as_mut().filter(|p| p.seq == seq) {
            Some(peek) => {
                peek.body = Some(lines);
                // A body landed: any in-flight auto-refresh is settled, so the
                // next Layout push may arm a new one (x-9c5f US9).
                peek.refresh_pending = false;
                true
            }
            None => false,
        }
    }

    /// Open the read-only peek overlay on `cursor` (x-c376), a `display_rows()`
    /// index the caller verified is a `DisplayRow::Agent`. Bumps the request seq
    /// and starts in the loading state; the caller sends the matching
    /// `PeekAgent` with the returned seq. Deliberately unlike the modal `open_*`
    /// helpers: the selector stays open UNDERNEATH so Esc drops back into it.
    fn open_peek(&mut self, cursor: usize, name: String) -> u64 {
        self.peek_seq = self.peek_seq.wrapping_add(1);
        self.peek = Some(PeekView {
            cursor,
            seq: self.peek_seq,
            body: None,
            name,
            last_fetch: Instant::now(),
            refresh_pending: false,
            squad: None,
        });
        self.peek_esc.clear();
        self.peek_seq
    }

    /// (x-10ec) Open a read-only peek on a WORKSPACE row, rendered locally
    /// from the layout the client already holds: its tabs and its member rows
    /// with their states. No wire command - a workspace has no transcript to
    /// fetch - and the auto-refresh and re-anchor paths key off `squad` so
    /// nothing ever sends a `PeekAgent` for a workspace label.
    fn open_squad_peek(&mut self, cursor: usize, sid: u64) {
        let label = self
            .layout
            .squads
            .iter()
            .find(|s| s.id == sid)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        let _unanswered = self.open_peek(cursor, label);
        if let Some(p) = self.peek.as_mut() {
            p.squad = Some(sid);
            p.body = Some(squad_peek_lines(&self.layout, sid));
        }
    }

    /// The `display_rows()` index of squad `id`'s own row (a `Sel` with no tab),
    /// or `None` if it is not currently a visible row. Used to re-point the
    /// selector cursor onto a squad after a `J`/`K` reorder (x-96e8).
    fn squad_row(&self, id: u64) -> Option<usize> {
        self.display_rows()
            .iter()
            .position(|r| matches!(r, DisplayRow::Sel(s) if s.tab.is_none() && s.squad == id))
    }

    /// The sideline's width in columns, or 0 when it is not rendering.
    ///
    /// The single width authority (AC5-EDGE): every consumer routes through
    /// here, so the work-pane minimum is enforced in ONE place rather than at
    /// each call site. Since x-2e86 the width is [`View::sideline_width`] (the
    /// operator's stored intent) clamped to `[MIN_SLIM_PANEL_W .. `
    /// [`sideline_max_width`]`]`, not a pure function of the density. The clamp
    /// is TRANSIENT: `sideline_width` is never mutated here, so a terminal
    /// shrink-then-grow restores the chosen width (AC1-FR / Locked 4).
    ///
    /// The rail auto-hides when the terminal cannot admit the current density
    /// (`max < `[`min_admit_width`]`)`: a `Regular` tree or `Extended` table too
    /// cramped to read gives its columns back to the panes, exactly as before
    /// free width. `Slim`'s admit floor is [`MIN_SLIM_PANEL_W`], so it hides only
    /// when even the slim rail cannot leave [`MIN_CONTENT_COLS`] (AC6-EDGE). The
    /// gate is on terminal CAPACITY, not the stored width, so a rail the terminal
    /// admits still shows at a small dragged width.
    fn panel_w(&self) -> u16 {
        if !self.panel_on {
            return 0;
        }
        let max = sideline_max_width(self.term.1);
        if max < min_admit_width(self.density) {
            return 0;
        }
        self.sideline_width.clamp(MIN_SLIM_PANEL_W, max)
    }

    /// Whether the bottom row belongs to chrome. Geometry beats the toggle:
    /// a too-short terminal recovers the line for content (AC4-ERR).
    fn status_visible(&self) -> bool {
        self.status_on && self.term.0 >= MIN_ROWS_FOR_STATUS
    }

    fn status_rows(&self) -> u16 {
        if self.status_visible() {
            STATUS_ROWS
        } else {
            0
        }
    }

    /// The CONTENT-AREA viewport reported to the server (terminal minus
    /// chrome). Never zero, so a degenerate terminal cannot wedge the server.
    fn content_dims(&self) -> (u16, u16) {
        (
            self.term
                .0
                .saturating_sub(TAB_BAR_ROWS + self.status_rows())
                .max(1),
            self.term.1.saturating_sub(self.panel_w()).max(1),
        )
    }

    /// Content viewport as `(origin, dims)` in `usize`, for centering a
    /// [`draw_lines_overlay`] popover against the content rect (right of the
    /// sideline, above any splits) instead of the outer terminal. One call
    /// site for every corner-anchored popover (x-e9c3).
    fn overlay_viewport(&self) -> ((usize, usize), (usize, usize)) {
        let (rows, cols) = self.content_dims();
        (
            (TAB_BAR_ROWS as usize, self.panel_w() as usize),
            (rows as usize, cols as usize),
        )
    }

    /// Map an outer-terminal cell (0-based) to `(pane, pane_row, pane_col)` when
    /// it falls inside a pane's content rect. `None` for a chrome cell (tab bar,
    /// sideline) or a content divider, so the caller swallows it - a mouse event
    /// on chrome never forwards to a pane (AC3-UI). Rects are content-area
    /// relative; the content origin is `(TAB_BAR_ROWS, panel_w)`.
    fn hit_test(&self, row: u16, col: u16) -> Option<(u64, u16, u16)> {
        let panel_w = self.panel_w();
        if row < TAB_BAR_ROWS || col < panel_w {
            return None;
        }
        let cr = row - TAB_BAR_ROWS;
        let cc = col - panel_w;
        for (pid, rect) in &self.layout.panes {
            if cr >= rect.y && cr < rect.y + rect.rows && cc >= rect.x && cc < rect.x + rect.cols {
                return Some((*pid, cr - rect.y, cc - rect.x));
            }
        }
        None
    }

    /// The pane covering a content-relative cell, if any. The shared primitive
    /// behind [`View::hit_test`] and [`View::seam_at`].
    fn pane_covering(&self, cr: u16, cc: u16) -> Option<u64> {
        self.layout
            .panes
            .iter()
            .find(|(_, r)| cr >= r.y && cr < r.y + r.rows && cc >= r.x && cc < r.x + r.cols)
            .map(|(pid, _)| *pid)
    }

    /// The seam under an outer-terminal cell, addressed by the panes flanking it.
    ///
    /// The client never receives the pane tree - `Layout` carries a flat
    /// `Vec<(PaneId, Rect)>` - so a seam cannot be addressed by branch path here.
    /// A flanking pane pair is derivable from the rects and resolves to exactly
    /// one branch child pair server-side, which is why it is the wire address
    /// (`Command::ResizeSeam`) rather than the topological one the design
    /// originally assumed.
    ///
    /// `None` on a covered cell, on chrome, and on a `┼` crossing, where the two
    /// candidate seams are genuinely ambiguous and picking one would resize a
    /// divider the operator was not pointing at.
    fn seam_at(&self, row: u16, col: u16) -> Option<Seam> {
        let panel_w = self.panel_w();
        if row < TAB_BAR_ROWS || col < panel_w {
            return None;
        }
        let (cr, cc) = (row - TAB_BAR_ROWS, col - panel_w);
        if self.pane_covering(cr, cc).is_some() {
            return None;
        }
        // Horizontal axis == children side by side == a vertical divider line.
        let across = cc
            .checked_sub(1)
            .and_then(|l| self.pane_covering(cr, l))
            .zip(cc.checked_add(1).and_then(|r| self.pane_covering(cr, r)))
            .map(|(a, b)| Seam {
                a,
                b,
                axis: Axis::Horizontal,
            });
        let down = cr
            .checked_sub(1)
            .and_then(|u| self.pane_covering(u, cc))
            .zip(cr.checked_add(1).and_then(|d| self.pane_covering(d, cc)))
            .map(|(a, b)| Seam {
                a,
                b,
                axis: Axis::Vertical,
            });
        match (across, down) {
            (Some(s), None) => Some(s),
            (None, Some(s)) => Some(s),
            _ => None,
        }
    }

    /// The rect of a pane by id, from the last layout.
    fn pane_rect(&self, pid: u64) -> Option<Rect> {
        self.layout
            .panes
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(_, r)| *r)
    }

    /// Whether a seam still separates the exact pair that addressed it.
    ///
    /// Membership is deliberately not the test. A concurrent same-axis split
    /// can insert a pane between the two while keeping both ids alive, and a
    /// membership check would call that seam live: the drag stays latched,
    /// `set_seam_ratio` refuses every command for the now non-adjacent pair,
    /// and the divider looks dead until release with no notice ever shown.
    /// Geometry is what the address actually means, so geometry is what is
    /// checked - one divider cell between them, overlapping across it.
    fn seam_is_live(&self, seam: Seam) -> bool {
        let (Some(ra), Some(rb)) = (self.pane_rect(seam.a), self.pane_rect(seam.b)) else {
            return false;
        };
        let abuts = |start: u16, len: u16, next: u16| start.saturating_add(len) + 1 == next;
        let overlaps = |a0: u16, a_len: u16, b0: u16, b_len: u16| {
            a0 < b0.saturating_add(b_len) && b0 < a0.saturating_add(a_len)
        };
        match seam.axis {
            Axis::Horizontal => {
                abuts(ra.x, ra.cols, rb.x) && overlaps(ra.y, ra.rows, rb.y, rb.rows)
            }
            Axis::Vertical => abuts(ra.y, ra.rows, rb.y) && overlaps(ra.x, ra.cols, rb.x, rb.cols),
        }
    }

    /// Where the seam sits now, as a content-area coordinate along its axis.
    /// Pair-relative, not branch-relative: the client cannot see the branch's
    /// other children, so the server rescales this against the pair's own total.
    /// Where the seam sits now, as a content-area coordinate along its axis.
    /// The divider is the cell immediately past pane `a`.
    fn seam_pos(&self, seam: Seam) -> Option<u16> {
        let ra = self.pane_rect(seam.a)?;
        Some(match seam.axis {
            Axis::Horizontal => ra.x.saturating_add(ra.cols),
            Axis::Vertical => ra.y.saturating_add(ra.rows),
        })
    }

    /// Where an outer-terminal cell puts the divider, in the same content-area
    /// coordinates. Converting a position to a ratio is deliberately the
    /// server's job: it needs the branch child's extent, and a pane's rect is
    /// not that extent once axes alternate more than one level deep.
    fn seam_pos_at(&self, seam: Seam, row: u16, col: u16) -> Option<u16> {
        let (cr, cc) = (
            row.checked_sub(TAB_BAR_ROWS)?,
            col.checked_sub(self.panel_w())?,
        );
        Some(match seam.axis {
            Axis::Horizontal => cc,
            Axis::Vertical => cr,
        })
    }

    /// Recompute which grabbable chrome (seam, sideline border, pane grip) is
    /// accented, from the pointer position `(row, col)`.
    ///
    /// Terminals cannot change the cursor shape, so this accent is the entire
    /// draggability affordance. It must be refreshed on hover AND at the end of
    /// a drag: `Drag` events never touch hover state, so a drag that ends off
    /// the thing it grabbed would otherwise leave it lit until the next bare
    /// `Move` (the stale-highlight the pane relocation drag already avoids by
    /// re-hit-testing at its release coords).
    fn refresh_hover_affordances(&mut self, row: u16, col: u16) {
        self.hover_seam = self.seam_at(row, col);
        self.hover_sideline_border = self.on_sideline_border(row, col);
        self.hover_grip = self.grip_at(row, col);
    }

    /// End a seam drag and recompute hover from `(row, col)`. Both the release
    /// arm and the non-left cancellation arm (a wheel, another button) route
    /// through here: a Drag event never refreshes hover, so however the gesture
    /// ends, the accent must be recomputed or it lingers until the next bare
    /// Move (codex peer review of the sideline-border affordance).
    fn end_seam_drag(&mut self, row: u16, col: u16) {
        self.seam_drag = None;
        self.refresh_hover_affordances(row, col);
    }

    /// End a sideline-border drag and recompute hover; see [`View::end_seam_drag`].
    ///
    /// (x-2e86) Persists the reached width on the way out - but only if the drag
    /// actually moved it, so a bare press-and-release on the border writes
    /// nothing. Every non-revert end routes here (release, a non-left event, and
    /// the stuck-drag timeout), so all three keep the width the operator dragged
    /// to (AC2-FR); only Esc reverts, via [`View::revert_sideline_drag`].
    fn end_sideline_drag(&mut self, row: u16, col: u16) {
        if let Some(drag) = self.sideline_drag.take() {
            if drag.start_width != self.sideline_width {
                view_store::save_width(self.sideline_width);
            }
        }
        self.refresh_hover_affordances(row, col);
    }

    /// (x-2e86) End a sideline drag by reverting to the width at grab (a bare
    /// Esc), mirroring [`View::revert_seam_drag`]. Returns whether the width
    /// changed, so the caller re-reports the content viewport only when it did.
    /// Client-local, so unlike the seam revert nothing travels to the server
    /// beyond the resulting `Resize`; the on-disk width is untouched because a
    /// reverted drag never persisted.
    fn revert_sideline_drag(&mut self) -> bool {
        let Some(drag) = self.sideline_drag.take() else {
            return false;
        };
        let changed = self.sideline_width != drag.start_width;
        self.sideline_width = drag.start_width;
        changed
    }

    /// True on the sideline's right border column - the grab band for the
    /// density drag. False when the sideline is hidden: there is no border to
    /// grab, so revealing it stays on the existing toggle.
    fn on_sideline_border(&self, row: u16, col: u16) -> bool {
        let panel_w = self.panel_w();
        panel_w > 0 && row >= TAB_BAR_ROWS && col == panel_w - 1
    }

    /// Grab a seam, remembering the share it currently holds so Esc can put it
    /// back. A seam whose panes have already gone is not grabbable.
    fn begin_seam_drag(&mut self, seam: Seam, now: Instant) {
        let Some(start) = self.seam_pos(seam) else {
            return;
        };
        self.seam_drag = Some(SeamDrag {
            seam,
            start_pos: start,
            last_pos: start,
            last_at: now,
        });
    }

    /// The command for a drag that has reached an outer cell, or `None` when
    /// the seam has not moved. A drag reports far more cells than the seam has
    /// positions, so this is what keeps the wire quiet between crossings.
    ///
    /// The seam's span is invariant under its own resize - the pair's total
    /// extent does not change, only the split point inside it - so the target
    /// stays stable as the server's layout updates mid-drag, and a command lost
    /// on the way self-heals at the next cell.
    fn seam_drag_to(&mut self, row: u16, col: u16, now: Instant) -> Option<Command> {
        let drag = self.seam_drag?;
        let target = self.seam_pos_at(drag.seam, row, col)?;
        if target == drag.last_pos {
            return None;
        }
        let live = self.seam_drag.as_mut()?;
        live.last_pos = target;
        live.last_at = now;
        Some(Command::ResizeSeam {
            a: drag.seam.a,
            b: drag.seam.b,
            pos: target,
        })
    }

    /// (x-2e86) Set the sideline to a free width from the dragged border column,
    /// clamped to `[MIN_SLIM_PANEL_W .. `[`sideline_max_width`]`]`. Returns
    /// whether the width changed, so the caller re-reports the content viewport
    /// only on a real crossing (the drag reports far more cells than columns).
    ///
    /// Reverses x-b186's snap-only rule: the width is now continuous. The lower
    /// clamp is the constant `MIN_SLIM_PANEL_W`, NOT the current density's floor,
    /// so a drag can shrink past a mode's structural floor - crossing it demotes
    /// the mode (Locked 5) rather than stopping the drag (AC2-EDGE).
    fn drag_sideline_to(&mut self, col: u16, now: Instant) -> bool {
        // Every drag report is motion, even one that clamps to the same width at
        // a bound: refresh the stuck-drag deadline FIRST, before any early
        // return, so an actively-held drag pinned against a clamp never expires
        // under the hand (codex P2). Only a genuinely report-less interval - the
        // swallowed mouse-up - then lets the timeout fire.
        if let Some(drag) = self.sideline_drag.as_mut() {
            drag.last_at = now;
        }
        let max = sideline_max_width(self.term.1);
        // A WINCH can shrink the terminal below MIN_SLIM + MIN_CONTENT mid-drag,
        // making `max < MIN_SLIM_PANEL_W`; a `clamp(MIN_SLIM_PANEL_W, max)` with
        // min > max panics (codex P2). The rail is hidden at that size, so there
        // is nothing to set - keep the stored width for when it grows back.
        if max < MIN_SLIM_PANEL_W {
            return false;
        }
        // The border sits on the sideline's last column, so the width the
        // operator is asking for is one past it.
        let want = col.saturating_add(1).clamp(MIN_SLIM_PANEL_W, max);
        if want == self.sideline_width {
            return false;
        }
        self.sideline_width = want;
        // Locked 5: a drag below the current mode's structural floor demotes the
        // mode to the widest that still renders. Only Extended has a floor above
        // MIN_SLIM_PANEL_W, so in practice this is Extended -> Regular; Regular
        // and Slim render down to the slim floor and never trip it. Same
        // re-anchor ordering as `cycle_density`: the row set changes with the
        // density, so re-anchor the selector (which re-clamps the scroll) after.
        if self.sideline_width < min_render_width(self.density) {
            let held = self.selected_agent_name();
            self.density = Density::Regular;
            view_store::save_prefs(self.density, self.agent_sort);
            self.reanchor_selector(held);
        }
        true
    }

    /// End the drag and put the seam back where it started, returning the
    /// command that does it. `None` when the seam never moved: a press that
    /// released without a crossing sent nothing, so there is nothing to undo.
    fn revert_seam_drag(&mut self) -> Option<Command> {
        let drag = self.seam_drag.take()?;
        (drag.last_pos != drag.start_pos).then_some(Command::ResizeSeam {
            a: drag.seam.a,
            b: drag.seam.b,
            pos: drag.start_pos,
        })
    }

    // -- pane relocation (x-aa95) -------------------------------------------

    /// The grip's row and column span for a pane rect, in OUTER terminal
    /// coordinates. Shared by the renderer and the hit test so a press always
    /// lands exactly where the glyphs drew.
    ///
    /// `None` when the pane cannot spare the cells. The grip is an affordance,
    /// never the only way in - the keyboard move-pane bind reaches the same
    /// operation - so a cramped pane loses the handle and keeps the gesture.
    ///
    /// Deliberately PERSISTENT rather than hover-only: a terminal cannot change
    /// the cursor shape, so a handle that appears only once the pointer is
    /// already on it is a handle nobody discovers. The cost is real and accepted
    /// - these three cells of each pane's top row always show `···` and always
    /// start a drag rather than reaching the inner app, so a program drawing
    /// there is overdrawn and unclickable at those columns. Bounded to
    /// multi-pane tabs, to three cells, and to panes wide enough to spare them,
    /// with the keyboard path as the escape hatch.
    fn grip_span(&self, rect: Rect) -> Option<(u16, std::ops::Range<u16>)> {
        let w = GRIP.chars().count() as u16;
        // Two spare cells so the grip never abuts the pane's own borders, where
        // it would read as part of the divider lattice rather than as a handle.
        if rect.cols < w + 2 {
            return None;
        }
        let row = TAB_BAR_ROWS + rect.y;
        let c0 = self.panel_w() + rect.x + (rect.cols - w) / 2;
        Some((row, c0..c0 + w))
    }

    /// The pane whose grip covers an outer cell.
    ///
    /// `None` on a single-pane tab: with nowhere to relocate to, a grip there
    /// would be an affordance for an operation that cannot succeed.
    fn grip_at(&self, row: u16, col: u16) -> Option<u64> {
        if self.layout.panes.len() < 2 {
            return None;
        }
        self.layout.panes.iter().find_map(|(pid, rect)| {
            let (grow, gcols) = self.grip_span(*rect)?;
            (row == grow && gcols.contains(&col)).then_some(*pid)
        })
    }

    /// The drop zone under an outer cell during a relocation drag.
    ///
    /// Seams come straight from x-d807's [`View::seam_at`], so the resize and
    /// relocate gestures cannot disagree about where a divider is - including
    /// its refusal on a `┼`, which stays "no zone here" rather than becoming a
    /// rejected drop. Outer edges are this node's own surface precisely because
    /// `seam_at` yields nothing off a live divider.
    fn drop_zone_at(&self, row: u16, col: u16) -> Option<DropZone> {
        if let Some(seam) = self.seam_at(row, col) {
            // Landing "between the pair" is landing after the left/top flank.
            return Some(DropZone {
                target: seam.a,
                dir: match seam.axis {
                    Axis::Horizontal => Dir::Right,
                    Axis::Vertical => Dir::Down,
                },
            });
        }
        self.edge_zone_at(row, col)
    }

    /// The zone for a cell on the content area's outer rim.
    ///
    /// An edge drop lands beside the pane the operator actually pointed at,
    /// not spanning the full side of the layout. Predictability wins over
    /// power here: "it goes next to this one" is what the pointer already
    /// says, and a full-side insert would need a root-level tree op whose
    /// result the operator cannot see themselves asking for.
    fn edge_zone_at(&self, row: u16, col: u16) -> Option<DropZone> {
        let panel_w = self.panel_w();
        if row < TAB_BAR_ROWS || col < panel_w {
            return None;
        }
        let (cr, cc) = (row - TAB_BAR_ROWS, col - panel_w);
        let (a_rows, a_cols) = self.layout.area;
        if a_rows == 0 || a_cols == 0 || cr >= a_rows || cc >= a_cols {
            return None;
        }
        let target = self.pane_covering(cr, cc)?;
        // Corners resolve left/right before up/down rather than being refused:
        // unlike a `┼` (where two seams genuinely compete), both answers at a
        // corner are the same drop from the operator's view - the pane ends up
        // in that corner either way - so a fixed order beats a dead cell.
        let dir = if cc == 0 {
            Dir::Left
        } else if cc + 1 == a_cols {
            Dir::Right
        } else if cr == 0 {
            Dir::Up
        } else if cr + 1 == a_rows {
            Dir::Down
        } else {
            // Interior cell: instead of going dark (the old `return None`),
            // resolve to the nearest edge of the pane UNDER the pointer, so the
            // destination band tracks the pointer live rather than only waking
            // at the content-area rim (x-aa95 shipped rim-only; a relocation
            // gave no preview until the pointer was dragged all the way to an
            // outer edge). Keyed off THIS pane's own centre so it stays
            // predictable: the pointer's dominant offset from centre picks the
            // axis, its sign the side. A dead-centre tie resolves horizontal,
            // matching the corner tie-break above. Rim/seam cells never reach
            // here (handled above and by `drop_zone_at`'s seam check), so their
            // semantics and every rim test are unchanged.
            let rect = self.pane_rect(target)?;
            let (lx, ly) = (
                cc.saturating_sub(rect.x) as i32,
                cr.saturating_sub(rect.y) as i32,
            );
            let (cols, rows) = (rect.cols as i32, rect.rows as i32);
            // Compare |nx-0.5| vs |ny-0.5| without floats: cross-multiply the
            // half-cell-centred offsets by the opposite dimension.
            let hx = 2 * lx + 1 - cols; // <0 left half, >0 right half
            let hy = 2 * ly + 1 - rows; // <0 top half,  >0 bottom half
            if hx.abs() * rows >= hy.abs() * cols {
                if hx < 0 {
                    Dir::Left
                } else {
                    Dir::Right
                }
            } else if hy < 0 {
                Dir::Up
            } else {
                Dir::Down
            }
        };
        Some(DropZone { target, dir })
    }

    /// (v43, x-d6a8 G1) Whether a cell lies on the tab strip (the top bar right
    /// of the sideline panel) rather than the content area. The same region
    /// [`View::chrome_hit`] treats as the strip, so a pane dropped anywhere on it
    /// breaks into a new tab.
    fn strip_at(&self, row: u16, col: u16) -> bool {
        row < TAB_BAR_ROWS && col >= self.panel_w()
    }

    /// (v43, x-d6a8 G2) The stable tab id under a strip cell, if it names an
    /// existing tab (a drag SOURCE). Walks the same spans [`View::chrome_hit`]
    /// walks, so a drag pickup and a click resolve identically. The `+` (NewTab)
    /// is not a drag source, and a cell under the notice overlay is not the strip.
    fn tab_cell_at(&self, row: u16, col: u16) -> Option<u64> {
        let panel_w = self.panel_w();
        if row >= TAB_BAR_ROWS || col < panel_w {
            return None;
        }
        let col = col as usize;
        if let Some((start, text)) = self.notice_overlay(self.term.1 as usize) {
            if col >= start && col < start + text.chars().count() {
                return None;
            }
        }
        let mut c = panel_w as usize;
        for span in self.tab_bar_window() {
            let w = span.text.chars().count();
            if col >= c && col < c + w {
                return match span.hit? {
                    TabHit::Tab(tid) => Some(tid),
                    TabHit::NewTab => None,
                };
            }
            c += w;
        }
        None
    }

    /// (x-b465) A content-derived identity for the sideline row at `i`, stable
    /// across a layout push. `None` for a row with no identity of its own (a
    /// spacer, a table head): those cannot be re-checked after a push, so a
    /// gesture must not latch onto one in the first place.
    ///
    /// An INDEX is not an identity. `display_rows()` is rebuilt on every layout,
    /// so a row that vanishes mid-gesture slides a different row under the same
    /// number - which is why `set_layout` re-anchors the selector by agent name
    /// and why `RowDrag` carries a `RowSource` rather than a position.
    fn row_identity(&self, i: usize) -> Option<String> {
        match self.display_rows().get(i)? {
            DisplayRow::Agent(a) => Some(format!("agent:{}", a.name)),
            DisplayRow::Sel(s) => Some(format!("squad:{}:{:?}", s.squad, s.tab)),
            DisplayRow::Card(c) => Some(format!("card:{}", c.id)),
            DisplayRow::Header { key, .. } => Some(format!("header:{key:?}")),
            DisplayRow::IdleFold { key, .. } => Some(format!("idlefold:{key:?}")),
            // A `Sub` line's own text is not unique: it carries an agent's
            // `cwd_base`, and two agents in one directory render the same
            // string, as do repeated `+N more` remainders. Anchor it to the
            // nearest identifiable row above plus its offset from that anchor,
            // which survives a push the way a bare index does not.
            DisplayRow::Sub(t) => {
                let rows = self.display_rows();
                let anchor = (0..i)
                    .rev()
                    .find(|&j| !matches!(rows.get(j), Some(DisplayRow::Sub(_))));
                let (head, off) = match anchor {
                    Some(j) => (self.row_identity(j).unwrap_or_default(), i - j),
                    None => (String::new(), i),
                };
                Some(format!("sub:{head}+{off}:{t}"))
            }
            DisplayRow::NewSquad => Some("newsquad".into()),
            DisplayRow::Blank | DisplayRow::TableHead | DisplayRow::TableEmpty => None,
        }
    }

    /// (x-b465) The sideline row a press at `(row, col)` should HOLD on, with the
    /// identity to re-check it by, for the rows `row_drag_source_at` declines.
    /// Inert rows qualify: a hold that opens no menu answers with a notice, and
    /// silence is the defect this fixes. A row with no stable identity does not,
    /// so its press keeps the pre-hold behavior rather than latching.
    ///
    /// The density button is the one positional exclusion, mirroring
    /// `row_drag_source_at`'s own row-0 guard - it is pinned chrome painted OVER
    /// an agent row, so holding it must not open that row's menu.
    fn press_hold_row_at(&self, row: u16, col: u16) -> Option<(usize, String)> {
        if row == 0 {
            if let Some(range) = self.density_button_range(self.panel_w() as usize) {
                if range.contains(&(col as usize)) {
                    return None;
                }
            }
        }
        let i = self.sideline_row_at(row, col)?;
        Some((i, self.row_identity(i)?))
    }

    /// (v43, x-d6a8 G3) The drag source of a sideline agent row under a cell, if
    /// any. A pane-hosted row drags its pane; a paneless bg row drags its attach
    /// id; a row with neither (a dead external tombstone) is not draggable.
    fn row_drag_source_at(&self, row: u16, col: u16) -> Option<RowSource> {
        // The density button is pinned chrome painted over row 0 (chrome_hit
        // resolves it to CycleDensity before any row action). A press there must
        // cycle density, not drag the agent row drawn underneath it - so it is
        // never a drag source. Mirrors chrome_hit's own row-0 guard.
        if row == 0 {
            if let Some(range) = self.density_button_range(self.panel_w() as usize) {
                if range.contains(&(col as usize)) {
                    return None;
                }
            }
        }
        let i = self.sideline_row_at(row, col)?;
        match self.display_rows().get(i)? {
            DisplayRow::Agent(a) => {
                if let Some(pid) = a.pane_id {
                    Some(RowSource::Pane(pid))
                } else {
                    a.attach_id.clone().map(RowSource::Attach)
                }
            }
            _ => None,
        }
    }

    fn begin_pane_drag(&mut self, mover: u64, now: Instant) {
        self.pane_drag = Some(PaneDrag {
            mover,
            zone: None,
            on_strip: false,
            last_at: now,
        });
    }

    /// Track the pointer to a new candidate zone. Returns whether the highlight
    /// changed, so a drag across a pane's interior costs no redraws.
    fn pane_drag_to(&mut self, row: u16, col: u16, now: Instant) -> bool {
        let Some(mover) = self.pane_drag.map(|d| d.mover) else {
            return false;
        };
        // Over the strip (G1): the drop breaks the pane into its own tab, so the
        // content zone is cleared - a cell is the strip XOR a content zone.
        let on_strip = self.strip_at(row, col);
        // A zone the dragged pane already occupies is the origin: no highlight,
        // so the gesture reads as "this drop does nothing" before the release.
        // BOTH flanks count - a seam is addressed by its left/top pane, so the
        // divider a pane already abuts on its far side names the NEIGHBOUR and
        // would otherwise light up as a real destination.
        let abuts = |s: Seam| s.a == mover || s.b == mover;
        let zone = if on_strip {
            None
        } else {
            self.drop_zone_at(row, col)
                .filter(|z| z.target != mover && self.pane_rect(mover).is_some())
                .filter(|_| !self.seam_at(row, col).is_some_and(abuts))
        };
        let drag = self.pane_drag.as_mut().expect("checked above");
        drag.last_at = now;
        let changed = drag.zone != zone || drag.on_strip != on_strip;
        drag.zone = zone;
        drag.on_strip = on_strip;
        changed
    }

    /// End the drag and return the command that applies it.
    ///
    /// `None` on every cancel path - released off any zone, on the origin, or
    /// after the dragged pane went away - so a cancelled drag provably puts
    /// nothing on the wire rather than sending a command the server will refuse.
    /// Over the strip it breaks the pane into its own tab instead (G1).
    fn commit_pane_drag(&mut self) -> Option<Command> {
        let drag = self.pane_drag.take()?;
        if drag.on_strip {
            return Some(Command::BreakPane { pane: drag.mover });
        }
        let zone = drag.zone?;
        Some(Command::MovePane {
            mover: Some(drag.mover),
            target: Some(zone.target),
            dir: zone.dir,
        })
    }

    // ---- (v43, x-d6a8 G2) tab-cell join drag -------------------------------

    fn begin_tab_drag(&mut self, src_tab: u64, now: Instant) {
        self.tab_drag = Some(TabDrag {
            src_tab,
            zone: None,
            last_at: now,
            start_at: now,
            moved: false,
        });
    }

    /// Track a tab-cell drag to a content-edge zone. Returns whether the
    /// highlight changed. A drag of the CURRENT tab lights nothing: any zone in
    /// its own content would be a self-join, suppressed exactly like an origin
    /// drop (x-aa95 grammar).
    fn tab_drag_to(&mut self, row: u16, col: u16, now: Instant) -> bool {
        let Some(src_tab) = self.tab_drag.map(|d| d.src_tab) else {
            return false;
        };
        let self_join = self.active_squad_active_tab_id() == Some(src_tab);
        let zone = if self_join {
            None
        } else {
            self.drop_zone_at(row, col)
        };
        let drag = self.tab_drag.as_mut().expect("checked above");
        drag.last_at = now;
        let changed = drag.zone != zone;
        drag.zone = zone;
        changed
    }

    /// End the tab-cell drag and return the join command, or `None` on any cancel
    /// path (off-zone, or a self-join onto the source tab's own content - the
    /// server also refuses BAD_REQUEST, belt and braces).
    fn commit_tab_drag(&mut self) -> Option<Command> {
        let drag = self.tab_drag.take()?;
        if self.active_squad_active_tab_id() == Some(drag.src_tab) {
            return None;
        }
        let zone = drag.zone?;
        Some(Command::JoinTab {
            src_tab: drag.src_tab,
            anchor_pane: zone.target,
            dir: zone.dir,
        })
    }

    fn cancel_tab_drag(&mut self) -> bool {
        self.tab_drag.take().is_some()
    }

    // ---- (v43, x-d6a8 G3) sideline-row placement drag ----------------------

    fn begin_row_drag(&mut self, src: RowSource, now: Instant) {
        self.row_drag = Some(RowDrag {
            src,
            zone: None,
            last_at: now,
            start_at: now,
            moved: false,
        });
    }

    /// Track a sideline-row drag to a content-edge zone. Unlike a pane drag this
    /// does NOT gate on `pane_rect(mover)`: a pane-hosted row names a pane living
    /// in ANOTHER tab, off the current layout by design (the server resolves it).
    fn row_drag_to(&mut self, row: u16, col: u16, now: Instant) -> bool {
        let mover = match self.row_drag.as_ref().map(|d| &d.src) {
            None => return false,
            Some(RowSource::Pane(pid)) => Some(*pid),
            Some(RowSource::Attach(_)) => None,
        };
        // Suppress an origin drop the same way `pane_drag_to` does, but only for
        // a mover that is ON this layout: the server discards an origin move in
        // silence (it reads as a deliberate cancel), so a zone that lights up
        // and then moves nothing is the whole defect. An off-layout mover cannot
        // be an origin - it is not in this tree at all - and must keep every
        // zone, which is why the `pane_rect` gate stays out of this path.
        let zone = match mover.filter(|m| self.pane_rect(*m).is_some()) {
            Some(mover) => {
                let abuts = |s: Seam| s.a == mover || s.b == mover;
                self.drop_zone_at(row, col)
                    .filter(|z| z.target != mover)
                    .filter(|_| !self.seam_at(row, col).is_some_and(abuts))
            }
            None => self.drop_zone_at(row, col),
        };
        let drag = self.row_drag.as_mut().expect("checked above");
        drag.last_at = now;
        let changed = drag.zone != zone;
        drag.zone = zone;
        changed
    }

    /// End the sideline-row drag and return its placement command, or `None` off
    /// any zone. A pane-hosted row moves its pane cross-tab (`MovePane`); a
    /// paneless bg row attaches at the drop slot (`AttachAgent` with a placement
    /// anchored to the dropped-on pane in the current tab).
    fn commit_row_drag(&mut self) -> Option<Command> {
        let drag = self.row_drag.take()?;
        let zone = drag.zone?;
        Some(match drag.src {
            RowSource::Pane(pid) => Command::MovePane {
                mover: Some(pid),
                target: Some(zone.target),
                dir: zone.dir,
            },
            // The anchor (`at`) alone names the drop slot: the server resolves
            // its squad and tab from the anchor pane's live location (overriding
            // the agent's owner-squad routing), so the client leaves `tab`
            // unset rather than guess "the active tab".
            RowSource::Attach(id) => Command::AttachAgent {
                id,
                placement: PanePlacement {
                    at: Some(zone.target),
                    split: Some(zone.dir),
                    here: false,
                    ..PanePlacement::default()
                },
            },
        })
    }

    fn cancel_row_drag(&mut self) -> bool {
        self.row_drag.take().is_some()
    }

    /// Abandon a drag with no command. Esc, the timeout, and the dragged pane
    /// exiting mid-gesture all land here. Returns whether one was in flight.
    fn cancel_pane_drag(&mut self) -> bool {
        self.pane_drag.take().is_some()
    }

    /// The outer cells to light for a candidate zone: the one-cell band along
    /// the `dir` side of the target's rect.
    ///
    /// One formula serves both zone kinds because a zone always names the gap
    /// the pane will land in - beside a seam that band IS the divider between
    /// the pair. On the layout's outer rim there is no gap yet, so the band
    /// clamps onto the target's own edge column/row: still honest ("it lands on
    /// this side of this pane") without reaching into the sideline or the tab
    /// bar, which own those cells.
    fn drop_band(&self, zone: DropZone) -> Option<(std::ops::Range<u16>, std::ops::Range<u16>)> {
        let rect = self.pane_rect(zone.target)?;
        let panel_w = self.panel_w();
        // Saturating throughout: `rect` comes from the last Layout, which can
        // lag a sideline toggle or a resize, so these sums are not guaranteed to
        // stay inside the terminal. Overflow here would panic a debug build for
        // a highlight; clamping just draws the band at the edge instead.
        let (r0, c0) = (
            TAB_BAR_ROWS.saturating_add(rect.y),
            panel_w.saturating_add(rect.x),
        );
        let (r1, c1) = (r0.saturating_add(rect.rows), c0.saturating_add(rect.cols));
        // The band sits one cell outside the rect, except on the rim where that
        // cell belongs to the sideline or the tab bar - there it clamps back
        // onto the rect's own edge.
        // BOTH sides must be bounded. Guarding only the low side left the right
        // and bottom rims resolving to a cell one past the terminal, which
        // compose() then clamped away to an empty range: the zone rendered
        // nothing while a release there still relocated the pane. In this UI's
        // own vocabulary an unlit zone means "this drop does nothing" (an origin
        // drop deliberately blanks the zone to say exactly that), so the
        // asymmetry taught the wrong thing on half the rim.
        let (term_rows, term_cols) = self.term;
        // `outside` is None when there is no cell on that side at all (the
        // rect already starts at the edge). Checked rather than wrapping: the
        // old version leaned on wrapping to u16::MAX to fail the range test,
        // which worked but read as an accident.
        let band_col = |outside: Option<u16>, own: u16| {
            let c = outside
                .filter(|c| (panel_w..term_cols).contains(c))
                .unwrap_or(own);
            c..c.saturating_add(1)
        };
        let band_row = |outside: Option<u16>, own: u16| {
            let r = outside
                .filter(|r| (TAB_BAR_ROWS..term_rows).contains(r))
                .unwrap_or(own);
            r..r.saturating_add(1)
        };
        // saturating: a zero-column rect from the server would otherwise
        // underflow `c1 - 1` and panic in debug.
        Some(match zone.dir {
            Dir::Left => (r0..r1, band_col(c0.checked_sub(1), c0)),
            Dir::Right => (r0..r1, band_col(Some(c1), c1.saturating_sub(1))),
            Dir::Up => (band_row(r0.checked_sub(1), r0), c0..c1),
            Dir::Down => (band_row(Some(r1), r1.saturating_sub(1)), c0..c1),
        })
    }

    /// The column range of the footer's `☰ menu` button (x-8ccf US4), shared by
    /// the renderer and the hit-test so a click lands where it draws. `None` when
    /// the panel is too narrow to add the button beside the `+ new workspace`
    /// affordance, or a recruit-mark tally is competing for the row.
    fn footer_menu_range(&self, panel_w: usize) -> Option<std::ops::Range<usize>> {
        // last column is the divider
        let tw = panel_w.saturating_sub(1);
        // Display columns, not char count: the menu trigram (U+2630) is two display columns, so a
        // char-count range under-reserves by one and the button crosses the
        // divider into the pane.
        let mw = FOOTER_MENU.chars().map(glyph_cols).sum::<usize>();
        (self.marks.is_empty() && tw >= FOOTER_NEW_LABEL.len() + 2 + mw).then(|| (tw - mw)..tw)
    }

    /// (x-b186) The column range of the density button on the sideline's top
    /// row, shared by the renderer and the hit-test so a click lands where it
    /// draws. `None` when the panel is too narrow to reserve the button without
    /// eating the header's own label.
    ///
    /// The button is an affordance, never the only way in: Locked Decision 5
    /// puts the density cycle on a keybind too, so a too-narrow panel loses the
    /// button and keeps the gesture.
    fn density_button_range(&self, panel_w: usize) -> Option<std::ops::Range<usize>> {
        let tw = panel_w.saturating_sub(1); // last column is the divider
        (tw >= DENSITY_BTN_W + 6).then(|| (tw - DENSITY_BTN_W)..tw)
    }

    /// Map a left-click on chrome (the tab bar or the sideline) to what it does:
    /// switch tab/squad, focus an agent's pane, open a new tab, or a local hint
    /// for a row that isn't directly actionable (a work-only agent, a card).
    /// `None` = not a chrome cell (the caller falls through to [`hit_test`]), so
    /// clicking anywhere off the panel still reaches the pane underneath.
    fn chrome_hit(&self, row: u16, col: u16) -> Option<ChromeHit> {
        let panel_w = self.panel_w();
        // Tab strip (row 0, scoped to the content columns since x-cd67 US1): it
        // begins at `panel_w`, walking the same spans the renderer paints (with
        // the same origin). A row-0 click LEFT of the divider (`col < panel_w`)
        // belongs to the sideline's reclaimed row 0 and falls through below.
        // `panel_w == 0` (no sideline) -> strip from col 0, unchanged.
        if row < TAB_BAR_ROWS && col >= panel_w {
            let col = col as usize;
            if let Some((start, text)) = self.notice_overlay(self.term.1 as usize) {
                if col >= start && col < start + text.chars().count() {
                    return None;
                }
            }
            let mut c = panel_w as usize;
            for span in self.tab_bar_window() {
                let w = span.text.chars().count();
                if col >= c && col < c + w {
                    return match span.hit? {
                        TabHit::Tab(tid) => Some(ChromeHit::Cmds(vec![Command::SelectTab(tid)])),
                        TabHit::NewTab => Some(ChromeHit::Cmds(vec![Command::NewTab])),
                    };
                }
                c += w;
            }
            return None;
        }
        // Sideline: the panel column minus its divider. Off/narrow => no panel.
        if panel_w == 0 || col >= panel_w - 1 {
            return None;
        }
        // The bottom row is overlaid by the status / which-key / search chrome
        // (draw_bottom_row paints last), so a click there belongs to that chrome,
        // not the sideline row drawn underneath it (codex P2).
        if row as usize == (self.term.0 as usize).saturating_sub(1) && self.bottom_row_is_chrome() {
            return None;
        }
        // (x-b186) The density button rides the sideline's top row, painted over
        // whatever display row is scrolled to it. It is chrome pinned to row 0,
        // not a property of that row, so the check is on the PAINTED row and
        // must precede the display-row resolution below.
        if row == 0 {
            if let Some(range) = self.density_button_range(panel_w as usize) {
                if range.contains(&(col as usize)) {
                    return Some(ChromeHit::CycleDensity);
                }
            }
        }
        // Display row i is painted at `i - sideline_offset` (draw_sideline, since
        // the sideline owns row 0), so invert with the offset - else a click on a
        // scrolled row activates the wrong row. Mirrors sideline_row_at.
        let i = row as usize + self.sideline_offset;
        if let Some(hit) = self.table_header_hit(i, col) {
            return Some(hit);
        }
        // x-8ccf US4: a click on the footer's `☰ menu` region opens the sideline
        // MENU popup; the rest of the footer row keeps its `+ new` create action.
        if matches!(self.display_rows().get(i), Some(DisplayRow::NewSquad)) {
            if let Some(range) = self.footer_menu_range(panel_w as usize) {
                if range.contains(&(col as usize)) {
                    return Some(ChromeHit::OpenSidelineMenu { row, col });
                }
            }
        }
        self.row_action(i)
    }

    fn table_header_hit(&self, row: usize, col: u16) -> Option<ChromeHit> {
        if self.density != Density::Extended
            || !matches!(self.display_rows().get(row), Some(DisplayRow::TableHead))
        {
            return None;
        }
        let layout = TableLayout::fitting(self.panel_w().saturating_sub(1))?;
        if layout.status.contains(col) {
            Some(ChromeHit::SortColumn(AgentSortColumn::Status))
        } else if layout.agent.contains(col) {
            Some(ChromeHit::SortColumn(AgentSortColumn::Agent))
        } else if layout.tail.is_some_and(|span| span.contains(col)) {
            Some(ChromeHit::SortColumn(AgentSortColumn::LastMessage))
        } else if layout.pr.contains(col) {
            Some(ChromeHit::SortColumn(AgentSortColumn::Pr))
        } else if layout.age.contains(col) {
            Some(ChromeHit::SortColumn(AgentSortColumn::Age))
        } else {
            None
        }
    }

    /// What acting on sideline display row `i` does - the single resolver both
    /// a mouse click ([`View::chrome_hit`]) and the prefix+w selector's Enter
    /// route through (x-260a), so the two inputs can never diverge. `None` only
    /// for an out-of-range index or an inert [`DisplayRow::Header`].
    fn row_action(&self, i: usize) -> Option<ChromeHit> {
        match self.display_rows().get(i)? {
            DisplayRow::Sel(row) => match row.tab {
                // Acting on the already-active squad row was a silent no-op
                // (SelectSquad to the squad you're on); it now toggles the
                // caret locally instead (x-2f99). Inactive rows keep
                // SelectSquad - auto-expand in set_layout completes the
                // gesture when the resulting layout push lands. A mission
                // squad has no server-side squad to select (SelectSquad would
                // refuse "no such squad"), so it always just toggles locally.
                None if row.squad == self.layout.active_squad || is_mission_squad(row.squad) => {
                    Some(ChromeHit::CycleSection(squad_key(&self.layout, row.squad)?))
                }
                None => Some(ChromeHit::Cmds(vec![Command::SelectSquad(row.squad)])),
                Some(t) => {
                    let squad = self.layout.squads.iter().find(|s| s.id == row.squad)?;
                    let tid = squad.tabs.get(t)?.id;
                    // SelectTab already resolves the squad server-side (find_tab
                    // -> set_view), so one command switches squad+tab in a single
                    // layout push - sending SelectSquad first would flicker
                    // through the squad's previously-active tab (gemini review).
                    Some(ChromeHit::Cmds(vec![Command::SelectTab(tid)]))
                }
            },
            // A pane-hosted agent focuses its pane; a paneless claude bg row
            // attaches; a non-attachable row says so. Resolved by [`agent_hit`],
            // shared with the navigator's goto so a click and a keyboard jump
            // never diverge on what an agent's action is (x-653d).
            DisplayRow::Agent(a) => Some(agent_hit(a, self.layout.active_squad)),
            // A work-queue card dispatches/focuses via [`View::card_hit`], the
            // same resolver the navigator uses (x-653d).
            DisplayRow::Card(c) => Some(self.card_hit(c)),
            // (x-975a) A `~` section header cycles its own view state, exactly
            // like a squad name row. It stays `row_is_inert` so the selector
            // cursor still skips it (the x-260a "never rests on a label"
            // invariant): this makes it CLICKABLE, not selectable.
            DisplayRow::Header { key, .. } => Some(ChromeHit::CycleSection(key.clone())),
            // (x-c5ee) The idle fold row toggles its squad's idle expansion -
            // the idle sibling of a header's CycleSection. Actionable, so it is
            // NOT inert: both a click and a selector Enter route here.
            DisplayRow::IdleFold { key, .. } => Some(ChromeHit::ToggleIdle(key.clone())),
            // Inert rows (subline, spacer, table column header) resolve to no
            // action (x-cd67).
            DisplayRow::Sub(_)
            | DisplayRow::Blank
            | DisplayRow::TableHead
            | DisplayRow::TableEmpty => None,
            // The `+` footer opens the name-input overlay (x-9e5e).
            DisplayRow::NewSquad if self.term.0 < MIN_ROWS_FOR_STATUS => Some(ChromeHit::Notice(
                "terminal too short for the name prompt".into(),
            )),
            DisplayRow::NewSquad => Some(ChromeHit::OpenCreate),
        }
    }

    /// The [`ChromeHit`] for one work-queue card - the resolver shared by a
    /// sideline click ([`View::row_action`]) and the navigator's goto
    /// ([`View::nav_rows`], x-653d). A method (not a free fn like [`agent_hit`])
    /// because the Ready confirm needs the term-height guard.
    ///
    /// Only a READY card starts a session (x-a496) - the same nodes prefix+g
    /// picks - and only behind a one-keypress confirm (too costly for a stray
    /// tap). A blocked/in-flight card is work prefix+g never selects, so it says
    /// why or routes to the running session (x-54fa, priority pane > attach >
    /// notice) rather than opening the confirm.
    fn card_hit(&self, c: &BacklogCard) -> ChromeHit {
        match c.state {
            // A terminal too short to render the bottom-row prompt refuses
            // instead of arming an INVISIBLE confirm that would capture keys and
            // could dispatch blind (sigma review x-260a).
            CardState::Ready if self.term.0 < MIN_ROWS_FOR_STATUS => {
                ChromeHit::Notice("terminal too short for the dispatch prompt".into())
            }
            CardState::Ready => ChromeHit::Confirm(ConfirmAction {
                action: ConfirmKind::Dispatch { node: c.id.clone() },
                label: if c.slug.is_empty() {
                    c.id.clone()
                } else {
                    c.slug.clone()
                },
            }),
            CardState::Blocked => ChromeHit::Notice("card blocked - unmet deps".into()),
            CardState::InFlight => match (c.pane_id, &c.attach_id) {
                (Some(pid), _) => ChromeHit::Cmds(vec![Command::FocusPane(pid)]),
                (None, Some(id)) => ChromeHit::Cmds(vec![Command::attach_agent(id)]),
                (None, None) => ChromeHit::Notice(
                    c.where_hint
                        .clone()
                        .unwrap_or_else(|| "card in flight - no session visible here".into()),
                ),
            },
        }
    }

    /// The navigator's flat GLOBAL catalog (x-653d): one [`NavRow`] per squad,
    /// per tab (ignoring expand state - a collapsed squad's tabs still appear,
    /// the key difference from [`display_rows`]), per plain pane (v22: those NOT
    /// already shown as an agent row), per agent, and per work-queue card across
    /// the WHOLE session. Shares the agent/card -> [`ChromeHit`]
    /// mapping with [`row_action`] (via [`agent_hit`]/[`card_hit`]) so a keyboard
    /// goto and a mouse click never diverge. Squad/tab rows carry their own
    /// SelectSquad/SelectTab in `hit`; an agent row carries a `goto_squad`
    /// prefix (its pane lives in another squad). The `+ new workspace` footer is
    /// omitted - the navigator is a goto-existing picker (Discretion 4). Fully
    /// owned (no layout borrow) so goto can mutate the view after building it.
    fn nav_rows(&self) -> Vec<NavRow> {
        let mut out = Vec::new();
        let cross = |sq: u64| (sq != self.layout.active_squad).then_some(sq);
        // (x-e10f) The pane -> work-queue join behind "find by node": an agent
        // row's pane is the pane a card's `pane_id` names when that node is in
        // flight, so the card carries the node id and title-slug the pane's own
        // row can be searched by. Built ONCE per catalog rebuild (F5 on PR
        // 1194): a linear backlog scan per agent row was O(agents x backlog)
        // on every keypress.
        let mut card_by_pane: std::collections::HashMap<u64, &BacklogCard> =
            std::collections::HashMap::new();
        for c in &self.layout.backlog {
            if let Some(p) = c.pane_id {
                card_by_pane.entry(p).or_insert(c);
            }
        }
        let card_for_pane = |pid: Option<u64>| -> Option<&BacklogCard> {
            pid.and_then(|p| card_by_pane.get(&p)).copied()
        };
        for s in &self.layout.squads {
            // Always SelectSquad (unlike the sideline's active-squad
            // CycleSection): the navigator is a jump, never a view-state cycle.
            out.push(NavRow::new(
                s.name.clone(),
                PaneState::Idle,
                None,
                None,
                ChromeHit::Cmds(vec![Command::SelectSquad(s.id)]),
                &[],
            ));
            for (t, tab) in s.tabs.iter().enumerate() {
                let tab_text = tab_label_text(&tab.name, t, tab.named);
                out.push(NavRow::new(
                    format!("{} › {}", s.name, tab_text),
                    PaneState::Idle,
                    // SelectTab resolves the squad server-side, so one command
                    // switches squad+tab (row_action's tab arm, gemini review).
                    None,
                    None,
                    ChromeHit::Cmds(vec![Command::SelectTab(tab.id)]),
                    std::slice::from_ref(&s.name),
                ));
                // Plain panes of the tab (v22): a pane already shown as an agent
                // row is skipped (the agent row is the richer view of the same
                // pane); the rest become goto-able so a bare shell pane in any
                // tab/squad is reachable, not just the active view (codex review).
                for p in &tab.panes {
                    if self.layout.agents.iter().any(|a| a.pane_id == Some(p.id)) {
                        continue;
                    }
                    out.push(NavRow::new(
                        format!("{} › {} › {}", s.name, tab_text, p.label),
                        PaneState::Idle,
                        cross(s.id),
                        Some(tab.id),
                        ChromeHit::Cmds(vec![Command::FocusPane(p.id)]),
                        &[p.id.to_string(), s.name.clone()],
                    ));
                }
            }
            for a in self.layout.agents.iter().filter(|a| a.squad == Some(s.id)) {
                // (x-0090, x-0f9d US3) A pane-hosted agent's context resolves
                // inside-out: a NAMED tab leads with the agent then the tab name
                // (`build › reviews`); an unnamed tab keeps today's
                // `{squad} › {agent} ·N`; a watch-only row (no tab) falls back
                // to the squad.
                let label = match self.agent_tab_context(a.squad, a.tab) {
                    Some(TabContext::Named(name)) => format!("{} › {}", a.name, name),
                    Some(TabContext::Ordinal(n)) => format!("{} › {} ·{n}", s.name, a.name),
                    None => format!("{} › {}", s.name, a.name),
                };
                let card = card_for_pane(a.pane_id);
                out.push(NavRow::new(
                    label,
                    nav_agent_state(a),
                    // Switch to the agent's squad first when it is not active, so
                    // the following FocusPane lands there (the server resolves the
                    // pane's tab on focus; the ordinal is display-only).
                    cross(s.id),
                    None,
                    agent_hit(a, self.layout.active_squad),
                    &[
                        a.pane_id.map(|p| p.to_string()).unwrap_or_default(),
                        card.map(|c| c.id.clone()).unwrap_or_default(),
                        card.map(|c| c.slug.clone()).unwrap_or_default(),
                        s.name.clone(),
                        // (x-0719) The portal index joins the match key so an
                        // EXISTING portal is reachable by number through the
                        // navigator; the `portal:` prefix keeps it from
                        // colliding with pane ids and node hex.
                        a.portal.map(|p| format!("portal:{p}")).unwrap_or_default(),
                    ],
                ));
            }
        }
        // Orphan agents (no live squad), mirroring display_rows' orphan section.
        for a in self.layout.agents.iter().filter(
            |a| !matches!(a.squad, Some(id) if self.layout.squads.iter().any(|s| s.id == id)),
        ) {
            let card = card_for_pane(a.pane_id);
            out.push(NavRow::new(
                a.name.clone(),
                nav_agent_state(a),
                None,
                None,
                agent_hit(a, self.layout.active_squad),
                &[
                    a.pane_id.map(|p| p.to_string()).unwrap_or_default(),
                    card.map(|c| c.id.clone()).unwrap_or_default(),
                    card.map(|c| c.slug.clone()).unwrap_or_default(),
                    a.portal.map(|p| format!("portal:{p}")).unwrap_or_default(),
                ],
            ));
        }
        // Work-queue cards: goto opens the dispatch confirm / focuses the worker
        // (card_hit), no squad switch. A blocked/in-flight card reads as
        // Blocked/Working so the state filter surfaces stuck work uniformly.
        for c in &self.layout.backlog {
            let label = if c.slug.is_empty() { &c.id } else { &c.slug };
            out.push(NavRow::new(
                format!("{label} {}", c.priority),
                card_state(c),
                None,
                None,
                self.card_hit(c),
                &[
                    c.id.clone(),
                    c.slug.clone(),
                    c.pane_id.map(|p| p.to_string()).unwrap_or_default(),
                ],
            ));
        }
        out
    }

    /// The navigator rows matching the current text + state filter (x-653d),
    /// recomputed per keypress (no cache): case-insensitive substring on the
    /// match key (label + identity, x-e10f) AND the state chip when one is set.
    /// Text and state compose (both must match); letters only ever edit the
    /// query (Locked 5).
    fn nav_filtered(&self, nav: &NavView) -> Vec<NavRow> {
        let q = nav.query.to_lowercase();
        self.nav_rows()
            .into_iter()
            .filter(|r| nav.state_filter.is_none_or(|s| r.state == s))
            .filter(|r| q.is_empty() || r.match_key.contains(&q))
            .collect()
    }

    /// Move the navigator cursor by `delta`, clamped to the filtered row count
    /// (no wrap). Rows are recomputed to know the current ceiling.
    fn nav_move_cursor(&mut self, delta: isize) {
        let len = match self.nav.as_ref() {
            Some(n) => self.nav_filtered(n).len(),
            None => return,
        };
        if len == 0 {
            return;
        }
        if let Some(n) = self.nav.as_mut() {
            let cur = n.cursor.min(len - 1) as isize;
            n.cursor = (cur + delta).clamp(0, len as isize - 1) as usize;
        }
    }

    /// Advance the state chip on `Tab`: all -> Blocked -> Working -> DoneUnseen
    /// -> Idle -> all. Resets the cursor to the top of the re-filtered set.
    fn nav_cycle_state(&mut self) {
        if let Some(n) = self.nav.as_mut() {
            n.state_filter = match n.state_filter {
                None => Some(PaneState::Blocked),
                Some(PaneState::Blocked) => Some(PaneState::Working),
                Some(PaneState::Working) => Some(PaneState::DoneUnseen),
                Some(PaneState::DoneUnseen) => Some(PaneState::Unmeasured),
                Some(PaneState::Unmeasured) => Some(PaneState::Idle),
                Some(PaneState::Idle) => Some(PaneState::Empty),
                Some(PaneState::Empty) => None,
            };
            n.cursor = 0;
        }
    }

    /// Reverse the state chip on `Shift-Tab`: all -> Idle -> DoneUnseen ->
    /// Working -> Blocked -> all (the exact reverse of [`nav_cycle_state`]).
    /// Resets the cursor to the top of the re-filtered set.
    fn nav_cycle_state_rev(&mut self) {
        if let Some(n) = self.nav.as_mut() {
            n.state_filter = match n.state_filter {
                None => Some(PaneState::Empty),
                Some(PaneState::Empty) => Some(PaneState::Idle),
                Some(PaneState::Idle) => Some(PaneState::Unmeasured),
                Some(PaneState::Unmeasured) => Some(PaneState::DoneUnseen),
                Some(PaneState::DoneUnseen) => Some(PaneState::Working),
                Some(PaneState::Working) => Some(PaneState::Blocked),
                Some(PaneState::Blocked) => None,
            };
            n.cursor = 0;
        }
    }

    /// BEL when the current filter excludes every row (AC2-ERR/AC3-ERR): a query
    /// or state that matches nothing is audible, never a silent empty overlay.
    fn nav_ring_if_empty(&self) {
        if let Some(n) = self.nav.as_ref() {
            if self.nav_filtered(n).is_empty() {
                let _ = raw_out(b"\x07");
            }
        }
    }

    /// The `display_rows()` index a hover cell falls on in the sideline, or
    /// `None` when the cell is not a sideline text cell - a pane, the divider
    /// column, the tab bar, or the bottom chrome row. Mirrors [`chrome_hit`]'s
    /// sideline geometry exactly so the highlight lands where a click would
    /// (x-a496).
    fn sideline_row_at(&self, row: u16, col: u16) -> Option<usize> {
        let panel_w = self.panel_w();
        // (x-cd67 US1) The sideline now owns row 0 (the strip moved right of the
        // divider), so the `row < TAB_BAR_ROWS` exclusion is gone and display
        // row `i` maps directly from `row` (no TAB_BAR_ROWS offset). A cell on
        // the divider or in the strip's content columns still returns None.
        if panel_w == 0 || col >= panel_w - 1 {
            return None;
        }
        if row as usize == (self.term.0 as usize).saturating_sub(1) && self.bottom_row_is_chrome() {
            return None;
        }
        let i = row as usize + self.sideline_offset;
        (i < self.display_rows().len()).then_some(i)
    }

    /// Fold one bare-motion (hover) report into the sideline highlight and the
    /// focus-follows-mouse debounce state (x-a496). Does NOT fire focus - it only
    /// records which pane the pointer is settling on and when it first landed
    /// there; the select loop's settle timer commits the focus once the pointer
    /// rests past [`HOVER_DEBOUNCE`]. Firing CANNOT be reactive here: ?1003 stops
    /// reporting the instant the pointer stops, so "land in a pane and rest" (the
    /// primary gesture) emits no further event to fire on - only a timer can.
    /// `now` records the landing instant for that timer's deadline.
    fn on_hover(&mut self, row: u16, col: u16, now: Instant) {
        // Highlight is highlight-only and always on (never switches the view);
        // a cell off the sideline text column clears it.
        self.hover_row = self.sideline_row_at(row, col);

        // (x-f331) Pointer-in-panel ARMS the selector to the hovered actionable
        // row - one regime, so x/X/r/space act on the row under the pointer and
        // a bare verb no longer leaks into the focused pane. Only touch a free or
        // already-hover-armed selector; an explicit prefix+w selector keeps
        // keyboard control. Off an actionable row (a spacer, header label, or the
        // pane), a hover-arm disarms, so a pointer parked off the rows never holds
        // the keys. `selector_anchor(i) == Some(i)` is true only when i itself is
        // an actionable (non-inert) row.
        if self.selector.is_none() || self.sel_hover_armed {
            match self
                .hover_row
                .filter(|&i| self.selector_anchor(i) == Some(i))
            {
                Some(i) => {
                    self.selector = Some(i);
                    self.sel_hover_armed = true;
                }
                None if self.sel_hover_armed => {
                    self.selector = None;
                    self.sel_hover_armed = false;
                }
                None => {}
            }
        }

        // Accent whatever grabbable chrome sits under the pointer (independent
        // of the focus-follow off-switch below).
        self.refresh_hover_affordances(row, col);

        // (hover affordance) The link probe tracks the exact CELL, so every
        // crossed cell restarts its quiet period - unlike focus-follows below,
        // which keeps the pane's first landing time. Chrome/divider/overlay
        // targets clear the probe and the underline immediately, no request.
        // Deliberately ABOVE the focus off-switch: a disabled focus-follow
        // steals no focus, but links still afford hovering.
        self.link_hover.retarget(self.hit_test(row, col), now);

        // Focus-follows-mouse rides the off-switch. hit_test resolves a PANE
        // (chrome/divider/sideline => None), so hovering the sideline never
        // steals focus - only moving over pane content does.
        if !self.hover_focus {
            self.hover_pending = None;
            return;
        }
        match self.hit_test(row, col).map(|(p, _, _)| p) {
            // Over chrome, or already on the focused pane: nothing to settle onto.
            None => self.hover_pending = None,
            Some(p) if p == self.layout.focus => self.hover_pending = None,
            // Keep the original landing instant while the pointer stays on the
            // same pane, so continued motion WITHIN it doesn't keep pushing the
            // settle deadline forward (that would starve a slow drag of focus);
            // only a NEW pane restarts the clock, which also coalesces a fast
            // sweep - each pane crossed replaces the last, so only the pane the
            // pointer rests on survives to the timer.
            Some(p) => {
                if !matches!(self.hover_pending, Some((pending, _)) if pending == p) {
                    self.hover_pending = Some((p, now));
                }
            }
        }
    }

    /// The settle timer fired (x-a496): if a pane is still pending and is not
    /// already the focus, claim it (clearing the pending state) and return it for
    /// the caller to `FocusPane`. `None` when the pointer left the pane before
    /// the deadline or it already became the focus.
    fn take_settled_hover(&mut self) -> Option<u64> {
        let (pane, _) = self.hover_pending.take()?;
        (pane != self.layout.focus).then_some(pane)
    }

    fn set_layout(&mut self, layout: LayoutView) {
        // Frames for panes unknown to the new Layout are dead - drop them
        // (Concurrency: a frame is only ever drawn against the Layout
        // generation it belongs to).
        let live: HashSet<u64> = layout.panes.iter().map(|(id, _)| *id).collect();
        self.frames.retain(|id, _| live.contains(id));
        // (x-1d91) A changed card set is the ONLY confirmation a dispatched
        // reorder verb gets. Checked against the incoming backlog before it is
        // stored, since the comparison is against the dispatch-time snapshot.
        self.confirm_backlog_pending(&layout.backlog);
        // (x-c5ee) No active-squad or mission seed here: the "active squad and
        // missions open by default" rule is computed live in `section_view()`,
        // so it tracks agents exiting mid-session (a majority-exited section
        // downgrades to LiveOnly, AC3-FR) and leaves this map holding only the
        // operator's own persisted choices - which is exactly what
        // `section_view()`'s step-1 precedence reads.
        // Prune squads that vanished server-side so the in-memory map only
        // holds live sections. This never reaches disk on its own: `save`
        // merges rather than replaces, precisely so one session's absent squad
        // cannot delete a sibling session's preference for it.
        self.section_view.retain(|k, _| section_is_live(&layout, k));
        // Capture the selected needs-row identity against the OLD layout, before
        // the swap, so the cursor can re-anchor to the same item afterward.
        let needs_prev = self.answers_selected_id();
        // (x-b2bf) Same capture for the yard spotlight: the crowd recomputes
        // from layout.agents, so the selection must follow the citizen, not
        // the slot.
        let yard_prev = self.yard_selected_name();
        // (x-b186) Same, for the sideline cursor when the table is status-sorted:
        // a scrape tick that flips one badge RE-ORDERS the rows, so preserving
        // only the numeric index would silently move the cursor onto a different
        // agent and point the next Enter / lifecycle key at the wrong worker.
        let agent_prev = (self.density == Density::Extended)
            .then(|| self.selected_agent_name())
            .flatten();
        // (x-4374) Capture the focused pane before the swap so a focus CHANGE can
        // pull the newly-focused row into view once the catalog settles.
        let focus_prev = self.layout.focus;
        self.layout = layout;
        // Selector re-anchors to a live, actionable row on catalog change
        // (AC6-FR): clamp into the unified rows, then step off an inert Header
        // so the cursor never rests on a label (x-260a). A pending J/K reorder
        // (x-96e8) instead re-points the cursor at the moved squad's new row so
        // it visually follows the workspace; the follow persists across repeated
        // presses until a non-reorder key clears sel_follow.
        if self.selector.is_some() {
            let anchored = match self.sel_follow.and_then(|sq| self.squad_row(sq)) {
                Some(row) => Some(row),
                // Identity first when a status re-sort could have moved the row
                // under the cursor; the index clamp is the fallback.
                None => agent_prev
                    .and_then(|name| {
                        self.display_rows()
                            .iter()
                            .position(|r| matches!(r, DisplayRow::Agent(a) if a.name == name))
                    })
                    .or_else(|| self.selector.and_then(|cur| self.selector_anchor(cur))),
            };
            self.selector = anchored;
        }
        // Needs-me overlay re-anchors to the SAME item across a scrape tick
        // (x-feec AC3-UI): a resolved row drops out, the queue re-sorts, and the
        // cursor stays on the item it was on (by identity, not index), clamped
        // in range. An emptied queue keeps the overlay open in its "nothing
        // needs you" state (AC4-EDGE) rather than closing under the user.
        self.reanchor_answers(needs_prev);
        // Yard spotlight re-anchors by citizen name (x-b2bf): identity first,
        // index clamp the fallback, so the sprite never jumps animals when a
        // scrape reorders the roster under an open yard.
        if self.yard.is_some() {
            // crowd borrows the whole view (rows by reference), so the
            // position lookup and the clamp bound resolve before the mut
            // borrow of the selection.
            let (found, last) = {
                let crowd = self.yard_crowd();
                (
                    yard_prev.and_then(|name| crowd.iter().position(|(n, _, _)| *n == name)),
                    crowd.len().saturating_sub(1),
                )
            };
            if let Some(yv) = self.yard.as_mut() {
                let prev = yv.sel;
                yv.sel = found.unwrap_or(prev).min(last);
            }
        }
        // Navigator re-clamps its cursor when a scrape tick reorders/removes rows
        // under it (x-653d, AC1-FR/AC2-EDGE): the rows recompute from self.layout
        // on every access, so after a push a past-the-end cursor would draw no
        // marker and mis-target Enter. Clamp, don't reset - the query and state
        // filter are unchanged, only the underlying catalog moved; resetting to 0
        // on every tick would fight a live badge update (matching the selector's
        // clamp-don't-jump discipline above).
        let nav_count = self.nav.as_ref().map(|n| self.nav_filtered(n).len());
        if let (Some(count), Some(nav)) = (nav_count, self.nav.as_mut()) {
            nav.cursor = count.saturating_sub(1).min(nav.cursor);
        }
        // Hover highlight re-anchors to a live display row on a layout push
        // (x-a496, AC3-FR): a dropped row must not leave the bar on a stale index.
        // Clear (not clamp) - a re-clamp would slide the highlight to an unrelated
        // row the pointer isn't over; the next Move re-establishes it.
        if let Some(hr) = self.hover_row {
            if hr >= self.display_rows().len() {
                self.hover_row = None;
            }
        }
        // Re-clamp the sideline scroll offset against the new catalog so a
        // shrunk row set never leaves the offset past the last row (x-a621).
        self.clamp_sideline_offset();
        // (x-4374) On a focus CHANGE, scroll the focused-row band into view - a
        // band the operator scrolled past is no better than the old gutter. Only
        // on a change, so a plain scrape tick never fights a manual scroll.
        if self.layout.focus != focus_prev {
            self.reveal_focus_row();
        }
        // Drop a pending focus-follow whose target pane vanished, so a settle can
        // never fire `FocusPane` at a dead id (the server would refuse it anyway).
        if let Some((pane, _)) = self.hover_pending {
            if !self.layout.panes.iter().any(|(id, _)| *id == pane) {
                self.hover_pending = None;
            }
        }
        // (x-d807) Same for a seam: a split or close elsewhere can retire the
        // pair that addressed it, and a seam whose panes are gone must not stay
        // lit as draggable. The drag itself re-anchors here too - it survives a
        // layout push that leaves its pair intact, and ends when one does not.
        if self.hover_seam.is_some_and(|s| !self.seam_is_live(s)) {
            self.hover_seam = None;
        }
        if self.seam_drag.is_some_and(|d| !self.seam_is_live(d.seam)) {
            self.seam_drag = None;
            self.set_notice("divider gone: layout changed".into());
        }
        // (x-aa95) Same for a relocation. AC7-FR: the dragged pane's process can
        // exit mid-gesture, and continuing to drag a pane that no longer exists
        // would end in a drop the server refuses for reasons the operator never
        // saw. The grip accent is cleared unannounced (it is only a hover hint);
        // an in-flight drag says why it stopped.
        if self.hover_grip.is_some_and(|p| self.pane_rect(p).is_none()) {
            self.hover_grip = None;
        }
        if self
            .pane_drag
            .is_some_and(|d| self.pane_rect(d.mover).is_none())
        {
            self.pane_drag = None;
            self.set_notice("pane gone: move cancelled".into());
        }
        // A drag whose TARGET retired keeps going - only the candidate is
        // stale, and the next motion recomputes it against the new layout.
        if let Some(zone) = self.pane_drag.and_then(|d| d.zone) {
            if self.pane_rect(zone.target).is_none() {
                if let Some(d) = self.pane_drag.as_mut() {
                    d.zone = None;
                }
            }
        }
        // (x-0f9d US1) Last, after every re-anchor: if a bare NewTab is
        // pending, the layout that just added the tab opens rename on it. Last
        // so `open_rename` clearing the selector/nav is never re-clobbered.
        self.maybe_prompt_new_tab_name();
    }

    /// A section's effective view, resolved live every frame (x-c5ee). The one
    /// authority behind both the caret glyph and the row filter, so they can
    /// never disagree. Order:
    ///   1. An explicit persisted operator choice wins verbatim - it survives a
    ///      restart and outranks every computed default below (Locked 2, AC1-FR).
    ///   2. Else a computed default, recomputed from the layout in hand:
    ///      - the active squad and every mission open `Expanded`, downgrading to
    ///        `LiveOnly` when the section is majority-exited so the dead rows
    ///        fold behind the header's `✗N` while the live agents stay up;
    ///      - an inactive squad stays `Collapsed` - surfacing live rows across
    ///        every idle workspace is the opposite of attention-focus;
    ///      - the two pull-sections `~ elsewhere` / `~ backlog` default
    ///        `Collapsed`, one click from their own header + rollup.
    /// The active-squad/mission default lives HERE, not in a map-seed: a seed is
    /// a one-time snapshot that cannot downgrade to LiveOnly as agents exit
    /// mid-session, and it pollutes the map that should hold only choices.
    fn section_view(&self, key: &SectionKey) -> SectionView {
        if let Some(chosen) = self.section_view.get(key).copied() {
            return chosen;
        }
        match key {
            SectionKey::Mission(_) => self.expanded_or_live_only(key),
            // The `~ missions` band is a progress summary, not a workspace: it
            // opens Expanded (the mission names are the content) and the operator
            // collapses it explicitly. No LiveOnly tier - the names have no
            // exited state, so it is binary.
            SectionKey::Missions => SectionView::Expanded,
            SectionKey::Squad(_) if self.is_active_squad(key) => self.expanded_or_live_only(key),
            SectionKey::Squad(_) | SectionKey::Elsewhere | SectionKey::WorkQueue => {
                SectionView::Collapsed
            }
        }
    }

    /// The Expanded-tier computed default: `Expanded`, or `LiveOnly` when the
    /// section is majority-exited (its dead rows then fold behind the header's
    /// `✗N` while the live rows stay). Only ever downgrades an Expanded default;
    /// never upgrades a Collapsed inactive squad (Locked 3).
    fn expanded_or_live_only(&self, key: &SectionKey) -> SectionView {
        if self.majority_exited(key) {
            SectionView::LiveOnly
        } else {
            SectionView::Expanded
        }
    }

    /// Whether `key` names the currently active squad. Compared through
    /// `squad_matches` (allocation-free) rather than minting a `SectionKey` for
    /// the active id on every call - `section_view` is per-section-per-frame hot.
    fn is_active_squad(&self, key: &SectionKey) -> bool {
        self.layout
            .squads
            .iter()
            .find(|s| s.id == self.layout.active_squad)
            .is_some_and(|s| squad_matches(s, key))
    }

    /// Strict-majority-exited over the section's own rows (`exited * 2 > total`).
    /// Zero rows is never a majority (an empty section keeps Expanded) and a
    /// 50/50 split is not either, so only a real majority downgrades to LiveOnly.
    /// Walks the same membership `section_dead_rows` does, live off the layout
    /// and never cached, so it tracks agents exiting mid-session. Only the
    /// Expanded-tier keys (active squad, mission) reach it; every other key has
    /// no squad match and reads as "not a majority".
    fn majority_exited(&self, key: &SectionKey) -> bool {
        let Some(id) = self
            .layout
            .squads
            .iter()
            .find(|s| squad_matches(s, key))
            .map(|s| s.id)
        else {
            return false;
        };
        let mut total = 0usize;
        let mut exited = 0usize;
        for a in self.layout.agents.iter().filter(|a| a.squad == Some(id)) {
            total += 1;
            exited += a.exited as usize;
        }
        exited * 2 > total
    }

    /// Advance a section one step through the view cycle (x-975a): pure client
    /// state, always visible next frame, then persisted so the choice survives
    /// a restart. `has_dead` and `binary` come from the section's own rows -
    /// see [`next_view`].
    fn cycle_section(&mut self, key: SectionKey) {
        let has_dead = !self.section_dead_rows(&key, None).is_empty();
        let next = next_view(self.section_view(&key), has_dead, &key);
        self.set_section_view(key, next);
    }

    /// (x-b186) One press of the density control: advance the cycle, persist,
    /// re-clamp the scroll. The one mutation point the keybind AND the top-right
    /// button share, so the two inputs cannot diverge on what they persist.
    ///
    /// Every press changes both the panel geometry and the button glyph in the
    /// same frame, so no press is ever visually inert.
    fn cycle_density(&mut self) {
        // (x-2e86, concurrency invariant) A density press during a live border
        // drag would fight the pointer for the width, so the in-flight drag owns
        // it until release: ignore the preset while dragging (AC3-FR).
        if self.sideline_drag.is_some() {
            return;
        }
        let held = self.selected_agent_name();
        self.density = self.density.next();
        // (x-2e86) A preset press picks the mode AND jumps the width to that
        // mode's canonical size, deliberately overwriting any dragged width -
        // that width jump is what makes each press visibly a preset (Locked 2).
        // Mode and width are one choice, so they persist in ONE locked mutation
        // (codex P2): separate writes could interleave with another mux client
        // and pair a mode with a width from a different press.
        self.sideline_width = canonical_width(self.density);
        view_store::save_preset(self.density, self.agent_sort, self.sideline_width);
        // The row set changes with the density (slim suppresses agent rows,
        // extended adds a column header), so a scrolled sideline must re-clamp
        // or it can sit past the new last row (x-a621). Ordering matters: the
        // clamp scrolls TO the selector, so it has to run after the re-anchor
        // has decided where the selector is - `reanchor_selector` owns both.
        self.reanchor_selector(held);
        self.clamp_sideline_offset();
    }

    fn set_agent_sort_column(&mut self, column: AgentSortColumn) {
        let held = self.selected_agent_name();
        self.agent_sort = if self.agent_sort.column == column {
            self.agent_sort.toggle_direction()
        } else {
            AgentSort::default_for(column)
        };
        view_store::save_prefs(self.density, self.agent_sort);
        self.reanchor_selector(held);
    }

    /// (x-b186) One press of the sort control. Persisted even outside Extended
    /// so the choice survives a round trip through another density.
    fn toggle_agent_sort(&mut self) {
        let held = self.selected_agent_name();
        self.agent_sort = self.agent_sort.advance();
        view_store::save_prefs(self.density, self.agent_sort);
        self.reanchor_selector(held);
    }

    /// The name of the agent the selector rests on, if it rests on an agent row.
    /// The re-anchor identity across a re-order: a row INDEX means nothing once
    /// the sort key changes, so the cursor has to follow the agent instead.
    fn selected_agent_name(&self) -> Option<String> {
        match self.display_rows().get(self.selector?) {
            Some(DisplayRow::Agent(a)) => Some(a.name.clone()),
            _ => None,
        }
    }

    /// Put the selector back on `held` after the row set changed under it.
    ///
    /// Identity first (the agent is still there, just elsewhere), then the
    /// existing index-clamp fallback for a cursor that was not on an agent or
    /// whose agent this density no longer emits - so the cursor never dangles
    /// past the end and never rests on an inert label (x-260a).
    fn reanchor_selector(&mut self, held: Option<String>) {
        if self.selector.is_none() {
            return;
        }
        if let Some(name) = held {
            if let Some(i) = self
                .display_rows()
                .iter()
                .position(|r| matches!(r, DisplayRow::Agent(a) if a.name == name))
            {
                self.selector = Some(i);
                // A re-order can move the agent outside the scroll window, and a
                // cursor with no visible row still takes contextual keys - so
                // scroll to it rather than leaving it off-screen.
                self.clamp_sideline_offset();
                return;
            }
        }
        self.selector = self.selector.and_then(|cur| self.selector_anchor(cur));
        self.clamp_sideline_offset();
    }

    /// Put a section in an explicit view state (the selector's `l`/`h`), then
    /// persist. The one write point both operator-initiated paths share, so a
    /// click and a keypress can never diverge on what gets saved.
    fn set_section_view(&mut self, key: SectionKey, view: SectionView) {
        self.section_view.insert(key.clone(), view);
        // Only an operator gesture reaches this method, so this is exactly the
        // explicit-choice set `save` persists.
        self.section_chosen.insert(key, view);
        view_store::save(&self.section_chosen);
        // Hiding rows shrinks the row set; re-clamp so a scrolled sideline never
        // skips past the new last row (x-a621).
        self.clamp_sideline_offset();
    }

    /// (x-c5ee) Toggle a squad's top-K idle expansion: a squad in the set shows
    /// all its idle rows, one absent folds the overflow behind `+N more`. A pure
    /// local flip, never persisted (the durable layer is [`SectionView`]), then
    /// re-anchor the selector and re-clamp - the row set just changed size under
    /// the cursor, exactly like a section cycle. Idempotent per press.
    fn toggle_idle(&mut self, key: SectionKey) {
        let held = self.selected_agent_name();
        if !self.idle_expanded.remove(&key) {
            self.idle_expanded.insert(key);
        }
        // `reanchor_selector` owns the clamp too, so the cursor follows its agent
        // when the fold/unfold moves it and never dangles past the new last row.
        self.reanchor_selector(held);
    }

    /// A section's exited rows. ONE predicate behind both `LiveOnly`'s hiding
    /// and the header menu's clear-dead, so the count the caret implies and the
    /// set the menu removes cannot drift apart. Folded live off the layout,
    /// never cached (the x-df4c drift posture), so a section whose last dead
    /// row was reaped elsewhere reports honestly on the very next click.
    /// `squad` is the caller's RUNTIME identity for a squad section, and it wins
    /// when present. `SectionKey::Squad` carries the canonical cwd because it is
    /// persisted and must survive a restart - but two squads can share an origin
    /// (identity is the id, not the path), so resolving a destructive action
    /// through the key alone could clear the sibling workspace's rows. Every
    /// header knows its own squad (`display_rows` emits one per squad), so the
    /// collision is structurally impossible on the paths that matter; `None`
    /// keeps the by-key lookup for display-only callers.
    fn section_dead_rows(&self, key: &SectionKey, squad: Option<u64>) -> Vec<&AgentRow> {
        match key {
            SectionKey::Squad(_) | SectionKey::Mission(_) => {
                let id = squad.or_else(|| {
                    self.layout
                        .squads
                        .iter()
                        .find(|s| squad_matches(s, key))
                        .map(|s| s.id)
                });
                let Some(id) = id else {
                    return Vec::new();
                };
                self.layout
                    .agents
                    .iter()
                    .filter(|a| a.squad == Some(id) && a.exited)
                    .collect()
            }
            SectionKey::Elsewhere => self.orphans().into_iter().filter(|a| a.exited).collect(),
            // Cards have no exited state, so the Backlog section is always binary.
            SectionKey::WorkQueue => Vec::new(),
            // The `~ missions` band holds progress names, not agents.
            SectionKey::Missions => Vec::new(),
        }
    }

    /// A squad's view state by id (test convenience: the production paths all
    /// hold the `&Squad` and key by name directly).
    #[cfg(test)]
    fn squad_view(&self, id: u64) -> SectionView {
        match squad_key(&self.layout, id) {
            Some(key) => self.section_view(&key),
            None => SectionView::Collapsed,
        }
    }

    /// Cycle a squad's section by id (test convenience for [`Self::cycle_section`]).
    #[cfg(test)]
    fn cycle_squad(&mut self, id: u64) {
        if let Some(key) = squad_key(&self.layout, id) {
            self.cycle_section(key);
        }
    }

    /// Force a squad's view state by id WITHOUT persisting - tests set up
    /// state, they do not simulate an operator gesture.
    #[cfg(test)]
    fn set_squad_view(&mut self, id: u64, view: SectionView) {
        if let Some(key) = squad_key(&self.layout, id) {
            self.section_view.insert(key, view);
        }
    }

    /// (x-c5ee) Force both pull-sections open so a test that exercises orphan
    /// (`~ elsewhere`) or backlog (`~ backlog`) rows renders them past their new
    /// Collapsed defaults. The collapse itself has dedicated AC tests; a test
    /// about card actions or orphan rows should not silently lose them.
    #[cfg(test)]
    fn expand_pull_sections(&mut self) {
        self.section_view
            .insert(SectionKey::Elsewhere, SectionView::Expanded);
        self.section_view
            .insert(SectionKey::WorkQueue, SectionView::Expanded);
    }

    /// Agents matched to no live squad - the `~ elsewhere` section's membership.
    /// One predicate so `display_rows` and the dead-row fold never diverge.
    fn orphans(&self) -> Vec<&AgentRow> {
        self.layout
            .agents
            .iter()
            .filter(
                |a| !matches!(a.squad, Some(id) if self.layout.squads.iter().any(|s| s.id == id)),
            )
            .collect()
    }

    /// Clamp a selector cursor into the current [`View::display_rows`] and
    /// step it off an inert Header row (forward first, else backward), so the
    /// cursor never rests on a label (x-260a invariant). `None` only for an
    /// empty list, unreachable in practice: the `+ new workspace` footer keeps
    /// the rows non-empty.
    fn selector_anchor(&self, cur: usize) -> Option<usize> {
        let rows = self.display_rows();
        if rows.is_empty() {
            return None;
        }
        let cur = cur.min(rows.len() - 1);
        if !row_is_inert(&rows[cur]) {
            return Some(cur);
        }
        (cur + 1..rows.len())
            .chain((0..cur).rev())
            .find(|&i| !row_is_inert(&rows[i]))
    }

    /// The next selector stop below `cur`: the nearest following display row
    /// that is not a Header. Clamps at the end (no wrap) - `cur` itself when
    /// nothing actionable follows.
    fn selector_down(&self, cur: usize) -> usize {
        let rows = self.display_rows();
        (cur + 1..rows.len())
            .find(|&i| !row_is_inert(&rows[i]))
            .unwrap_or(cur)
    }

    /// The next selector stop above `cur` (nearest first); `cur` at the top.
    fn selector_up(&self, cur: usize) -> usize {
        let rows = self.display_rows();
        (0..cur.min(rows.len()))
            .rev()
            .find(|&i| !row_is_inert(&rows[i]))
            .unwrap_or(cur)
    }

    /// The nearest `DisplayRow::Agent` index past `from` in `dir` (+1 down, -1
    /// up), skipping every non-agent row (x-c376 j/k peek). `None` when there is
    /// no agent row that way (the caller BELs and stays put). Re-reads the live
    /// catalog per call, so a scrape tick between keys never chases a stale row.
    fn peek_next_agent(&self, from: usize, dir: isize) -> Option<usize> {
        let rows = self.display_rows();
        let mut i = from as isize + dir;
        while i >= 0 && (i as usize) < rows.len() {
            if matches!(rows[i as usize], DisplayRow::Agent(_)) {
                return Some(i as usize);
            }
            i += dir;
        }
        None
    }

    /// Re-anchor or close the peek overlay after a catalog change (x-c376): if
    /// the peeked index no longer lands on an agent row, snap to the nearest
    /// agent row (down first, then up); close peek when none remain. Returns the
    /// name to re-fetch when it re-anchored, `None` when it held or closed.
    fn peek_reanchor(&mut self) -> Option<(usize, String)> {
        let (cursor, peeked) = self.peek.as_ref().map(|p| (p.cursor, p.name.clone()))?;
        // (x-10ec) A workspace peek holds while its squad's row still sits at
        // the cursor, refreshing the local body from the live layout; the
        // squad gone, it closes. Never re-anchors onto an agent row - that
        // would silently swap a workspace summary for a transcript fetch.
        if let Some(sid) = self.peek.as_ref().and_then(|p| p.squad) {
            let holds = matches!(
                self.display_rows().get(cursor),
                Some(DisplayRow::Sel(r)) if r.tab.is_none() && r.squad == sid
            );
            if holds {
                if let Some(p) = self.peek.as_mut() {
                    p.last_fetch = Instant::now();
                    p.body = Some(squad_peek_lines(&self.layout, sid));
                }
            } else {
                self.clear_peek();
            }
            return None;
        }
        // One `display_rows()` snapshot for the whole check: the identity test,
        // both direction scans, and the re-anchored name all read it (gemini
        // review).
        let rows = self.display_rows();
        if let Some(DisplayRow::Agent(a)) = rows.get(cursor) {
            // The SAME agent still sits here: hold. A DIFFERENT agent (a layout
            // shift reindexed the rows) refetches so the header and transcript
            // never disagree (codex review) - the seq guard alone can't catch
            // this, since the stale body already applied under the old identity.
            return (a.name != peeked).then(|| (cursor, a.name.clone()));
        }
        let scan = |dir: isize| {
            let mut i = cursor as isize + dir;
            while i >= 0 && (i as usize) < rows.len() {
                if matches!(rows[i as usize], DisplayRow::Agent(_)) {
                    return Some(i as usize);
                }
                i += dir;
            }
            None
        };
        let anchored = scan(1)
            .or_else(|| scan(-1))
            .and_then(|i| match rows.get(i) {
                Some(DisplayRow::Agent(a)) => Some((i, a.name.clone())),
                _ => None,
            });
        if anchored.is_none() {
            drop(rows);
            self.clear_peek();
        }
        anchored
    }

    /// (x-9c5f US9) Arm a transcript auto-refresh for the peeked row when its
    /// last fetch is older than [`PEEK_REFRESH_INTERVAL`]: bump the request seq +
    /// reset the fetch timer but KEEP the current body, so an active row follows
    /// without the "loading…" flicker a full [`View::open_peek`] would cause. The
    /// seq bump makes the seq guard drop any out-of-order body. Returns (seq,
    /// name) to send the fresh `PeekAgent`, or `None` when peek is closed / not
    /// yet due.
    fn peek_refresh_due(&mut self) -> Option<(u64, String)> {
        // (x-10ec) A workspace peek has no transcript to refresh - its body
        // re-renders locally on each Layout push in `peek_reanchor`. Arming a
        // request here would send a `PeekAgent` carrying a workspace label.
        if self.peek.as_ref().is_some_and(|p| p.squad.is_some()) {
            return None;
        }
        // Skip while a prior refresh is still in flight: stacking a new request
        // every push would supersede each response before it lands on a slow peek
        // read, so the transcript would never settle (never re-arm mid-flight).
        let due = self
            .peek
            .as_ref()
            .is_some_and(|p| !p.refresh_pending && p.last_fetch.elapsed() >= PEEK_REFRESH_INTERVAL);
        if !due {
            return None;
        }
        self.peek_seq = self.peek_seq.wrapping_add(1);
        let seq = self.peek_seq;
        let peek = self.peek.as_mut()?;
        peek.seq = seq;
        peek.last_fetch = Instant::now();
        peek.refresh_pending = true;
        Some((seq, peek.name.clone()))
    }

    /// Sideline rows the cursor can occupy: the full terminal height (the
    /// sideline owns row 0 since x-cd67 US1) minus the bottom chrome row,
    /// minus the court block's rows at the bottom. The block is the
    /// subtraction point's only second customer, so `clamp_sideline_offset`
    /// and `reveal_focus_row` inherit the shrunk window without a second
    /// fix.
    fn sideline_visible_rows(&self) -> usize {
        (self.term.0 as usize)
            .saturating_sub(self.bottom_row_is_chrome() as usize)
            .saturating_sub(self.court_block_rows())
    }

    /// Follow-the-cursor sideline scroll (x-a621): move [`View::sideline_offset`]
    /// the least it takes to keep the selector (or hover) row on screen, then
    /// clamp into `[0, rows - visible]` so a shrunk catalog never scrolls past the
    /// last row. Everything-fits (or an empty window) resets the offset to 0, so
    /// the common case renders byte-identically to a non-scrolling sideline.
    fn clamp_sideline_offset(&mut self) {
        let total = self.display_rows().len();
        let visible = self.sideline_visible_rows();
        if total <= visible || visible == 0 {
            self.sideline_offset = 0;
            return;
        }
        if let Some(cur) = self.selector.or(self.hover_row) {
            if cur < self.sideline_offset {
                self.sideline_offset = cur;
            } else if cur >= self.sideline_offset + visible {
                self.sideline_offset = cur + 1 - visible;
            }
        }
        self.sideline_offset = self.sideline_offset.min(total - visible);
    }

    /// (x-4374) Scroll the focused pane's sideline row into the visible window,
    /// moving [`View::sideline_offset`] the least it takes - the focused-row band
    /// is useless if it scrolled off. Deliberately narrow: a focused pane with no
    /// visible row (a bare shell pane, or a row inside a folded/LiveOnly section)
    /// scrolls nothing and NEVER auto-expands a fold - fold state is the
    /// operator's. Mirrors the cursor logic in [`View::clamp_sideline_offset`],
    /// keyed on the focus row instead of the selector.
    fn reveal_focus_row(&mut self) {
        // A live selector owns the scroll: `clamp_sideline_offset` already keeps
        // the actionable cursor on-screen, and stealing that to reveal a focus
        // band (which a background focus-follows-mouse can move independently of
        // the cursor) would leave Enter/lifecycle keys acting on a scrolled-off
        // row. The selector-visibility invariant wins (x-a621).
        if self.selector.is_some() {
            return;
        }
        let focus = self.layout.focus;
        let visible = self.sideline_visible_rows();
        let total = self.display_rows().len();
        if visible == 0 || total <= visible {
            return;
        }
        let Some(idx) = self.agent_row_index_for_pane(focus) else {
            return;
        };
        if idx < self.sideline_offset {
            self.sideline_offset = idx;
        } else if idx >= self.sideline_offset + visible {
            self.sideline_offset = idx + 1 - visible;
        }
        self.sideline_offset = self.sideline_offset.min(total - visible);
    }

    /// Wheel-scroll the sideline list by one row. With an EXPLICIT selector open
    /// it walks the cursor (reusing the j/k path so the highlight and offset stay
    /// coherent); otherwise it nudges the scroll offset directly, bounded to the
    /// catalog. A sideline that already fits its height is a no-op.
    ///
    /// (x-f331) A HOVER-armed selector is a transient pointer-follow, not a modal
    /// cursor, so the wheel must scroll the list rather than walk it - otherwise
    /// the wheel moves the selector away from the pointer, leaving `hover_row` and
    /// `selector` on two different rows (codex P2). Scrolling shifts the rows out
    /// from under the pointer, so the arm is disarmed here; the next pointer Move
    /// re-hit-tests and re-arms.
    fn scroll_sideline(&mut self, down: bool) {
        let total = self.display_rows().len();
        let visible = self.sideline_visible_rows();
        if total <= visible || visible == 0 {
            return;
        }
        match self.selector {
            Some(cur) if !self.sel_hover_armed => {
                self.selector = Some(if down {
                    self.selector_down(cur)
                } else {
                    self.selector_up(cur)
                });
                self.clamp_sideline_offset();
            }
            _ => {
                if self.sel_hover_armed {
                    self.selector = None;
                    self.sel_hover_armed = false;
                }
                self.sideline_offset = if down {
                    (self.sideline_offset + 1).min(total - visible)
                } else {
                    self.sideline_offset.saturating_sub(1)
                };
            }
        }
    }

    /// Compose the full-terminal frame: tab bar, sideline, dividers, panes.
    /// Pure - all the drawing machinery (row diff, styles, wide-spacer
    /// handling) stays in [`Compositor`].
    fn compose(&self) -> Frame {
        self.compose_at(Instant::now())
    }

    fn compose_at(&self, now: Instant) -> Frame {
        let (rows, cols) = self.term;
        let (rows, cols) = (rows.max(1) as usize, cols.max(1) as usize);
        let mut cells = vec![Cell::default(); rows * cols];
        let panel_w = self.panel_w() as usize;

        self.draw_tab_bar(&mut cells, cols);
        if panel_w > 0 {
            self.draw_sideline(&mut cells, rows, cols, panel_w);
        }

        // Content area: dividers first (uncovered cells), panes blitted over.
        let origin_r = TAB_BAR_ROWS as usize;
        let origin_c = panel_w;
        let mut covered = vec![false; rows * cols];
        // x-5a52: cells owned by the focused pane, so the divider pass can accent
        // the seams that bound it (a standing "you are here" outline).
        let mut focused = vec![false; rows * cols];
        for (pid, rect) in &self.layout.panes {
            let frame = self.frames.get(pid);
            for fr in 0..rect.rows as usize {
                let r = origin_r + rect.y as usize + fr;
                if r >= rows {
                    break;
                }
                for fc in 0..rect.cols as usize {
                    let c = origin_c + rect.x as usize + fc;
                    if c >= cols {
                        break;
                    }
                    covered[r * cols + c] = true;
                    if *pid == self.layout.focus {
                        focused[r * cols + c] = true;
                    }
                    if let Some(f) = frame {
                        if fr < f.rows as usize && fc < f.cols as usize {
                            cells[r * cols + c] = f.cells[fr * f.cols as usize + fc];
                        }
                    }
                }
            }
        }
        // (hover affordance) Underline the accepted link span, client-local:
        // OR UNDERLINE into exactly the pane-local cells the server named,
        // after the blit (content is in place) and before the grip/indicator/
        // overlay passes (chrome and overlays still win their cells). The
        // cached server `Frame` is untouched, so the span clears on the next
        // compose the moment it is dropped or invalidated. Suppressed while
        // ANY modal is open: `menu_usurping_open` names the full set, which
        // closes the keyboard-opened and overlay-blind cases the pointer-event
        // clear cannot see (an overlay opened with no pointer motion paints no
        // hover affordance beneath or around itself).
        if !self.menu_usurping_open() {
            if let Some((pid, span)) = self.link_hover.accepted.as_ref() {
                if let Some((_, rect)) = self.layout.panes.iter().find(|(p, _)| p == pid) {
                    for &(fr, fc) in span {
                        let r = origin_r + rect.y as usize + fr as usize;
                        let c = origin_c + rect.x as usize + fc as usize;
                        if r < rows && c < cols {
                            cells[r * cols + c].flags |= cell_flags::UNDERLINE;
                        }
                    }
                }
            }
        }
        // x-aa95: pane grips, drawn on cells the pane owns, hence after the blit
        // - but BEFORE the scroll indicator, which is state rather than an
        // affordance and so wins the cells they contend for on a narrow pane. The dragged pane's own grip stays lit for the
        // whole gesture, which is what keeps the origin marked (AC3-UI) once
        // the pointer has run off to a zone somewhere else.
        let dragged = self.pane_drag.map(|d| d.mover);
        // Hidden on a single-pane tab, matching `grip_at`: no grip is drawn
        // where none can be pressed.
        for (pid, rect) in self
            .layout
            .panes
            .iter()
            .filter(|_| self.layout.panes.len() >= 2)
        {
            let Some((grow, gcols)) = self.grip_span(*rect) else {
                continue;
            };
            let lit = dragged == Some(*pid) || (dragged.is_none() && self.hover_grip == Some(*pid));
            let (fg, flags) = if lit {
                (self.theme.accent, cell_flags::BOLD)
            } else {
                (Color::Default, cell_flags::DIM)
            };
            let (r, start, end) = (grow as usize, gcols.start as usize, gcols.end as usize);
            if r >= rows {
                continue;
            }
            blank_straddling_pair(&mut cells, cols, r, start, end);
            for (i, ch) in GRIP.chars().enumerate() {
                let c = start + i;
                if c < cols {
                    cells[r * cols + c] = Cell {
                        c: ch,
                        fg,
                        bg: Color::Default,
                        flags,
                    };
                }
            }
        }

        // Scroll indicator (US1, AC1-UI): a minimal `[+N]` at a scrolled pane's
        // top-right, inverse-video so it reads over content. Present iff the
        // pane's frame reports a non-zero offset (group 2's status row becomes
        // its canonical home). A pane too narrow to fit the label skips it.
        for (pid, rect) in &self.layout.panes {
            let Some(f) = self.frames.get(pid) else {
                continue;
            };
            if f.scroll_offset == 0 {
                continue;
            }
            let label = format!("[+{}]", f.scroll_offset);
            let w = label.chars().count();
            let r = origin_r + rect.y as usize;
            if (rect.cols as usize) < w || r >= rows {
                continue;
            }
            let start_c = origin_c + rect.x as usize + rect.cols as usize - w;
            for (k, ch) in label.chars().enumerate() {
                let c = start_c + k;
                if c < cols {
                    cells[r * cols + c] = Cell {
                        c: ch,
                        fg: Color::Default,
                        bg: Color::Default,
                        flags: cell_flags::INVERSE,
                    };
                }
            }
        }
        // Letterbox (AC1-UI): the server tiled its rects into `Layout.area`
        // (the view-scoped clamp); content anchors top-left and everything
        // beyond `area` up to the local content edge is visibly-inert dim
        // filler, never divider glyphs. `(0, 0)` is the pre-Layout
        // placeholder: no filler until the first real Layout names a bound.
        let (a_rows, a_cols) = self.layout.area;
        let boxed = self.layout.area != (0, 0);
        // A drag keeps the accent on the seam it grabbed even as the pointer
        // runs ahead of it, so the thing being moved stays the thing lit.
        let active_seam = self.seam_drag.map(|d| d.seam).or(self.hover_seam);
        // x-aa95: the candidate drop zone, lit while a relocation drag is live.
        // (x-d6a8) The tab-cell and sideline-row drags reuse the SAME content-edge
        // zone vocabulary, so one of the three lights the seam/band identically.
        let drop_zone = self
            .pane_drag
            .and_then(|d| d.zone)
            .or_else(|| self.tab_drag.and_then(|d| d.zone))
            .or_else(|| self.row_drag.as_ref().and_then(|d| d.zone));
        // Divider glyphs for in-area content cells no pane covers: pick by
        // which neighbors are panes so vertical strips read '│', horizontal
        // '─', crossings '┼'. Dim so chrome never shouts over content.
        for r in origin_r..rows {
            for c in origin_c..cols {
                if covered[r * cols + c] {
                    continue;
                }
                if boxed && (r - origin_r >= a_rows as usize || c - origin_c >= a_cols as usize) {
                    cells[r * cols + c] = Cell {
                        c: '·',
                        fg: Color::Default,
                        bg: Color::Default,
                        flags: cell_flags::DIM,
                    };
                    continue;
                }
                let horiz = c > origin_c && covered[r * cols + c - 1]
                    || c + 1 < cols && covered[r * cols + c + 1];
                let vert = r > origin_r && covered[(r - 1) * cols + c]
                    || r + 1 < rows && covered[(r + 1) * cols + c];
                // x-5a52: a divider cell that borders the focused pane paints in
                // the lattice accent at full brightness (not the DIM chrome), so
                // the focused pane wears a standing outline that moves with focus.
                // Interior seams only - an edge pane has no divider on that side.
                // Orthogonal neighbours suffice: a `┼` is emitted only when a cell
                // has a covered horizontal AND vertical neighbour, so every
                // visible junction is already orthogonally adjacent to its pane.
                // The lone diagonal-only cell is the 1-wide crossing where four
                // dividers meet, which renders blank (no covered ortho neighbour)
                // - accenting a space would be invisible, so we don't.
                let outline = c > origin_c && focused[r * cols + c - 1]
                    || c + 1 < cols && focused[r * cols + c + 1]
                    || r > origin_r && focused[(r - 1) * cols + c]
                    || r + 1 < rows && focused[(r + 1) * cols + c];
                // x-d807: the seam under the pointer (or held in a drag) reads
                // BOLD, distinct from both idle DIM chrome and the focus
                // outline's plain accent. A terminal cannot portably change the
                // cursor shape, so this is the only signal a divider is
                // draggable before the press.
                let grabbable =
                    active_seam.is_some_and(|s| self.seam_at(r as u16, c as u16) == Some(s));
                // x-aa95: the candidate zone outranks the hover accent - during
                // a drag the only question on screen is where the pane lands.
                let dropping =
                    drop_zone.is_some_and(|z| self.drop_zone_at(r as u16, c as u16) == Some(z));
                let (fg, flags) = if dropping {
                    (self.theme.accent, cell_flags::INVERSE)
                } else if grabbable {
                    (self.theme.accent, cell_flags::BOLD)
                } else if outline {
                    (self.theme.accent, 0)
                } else {
                    (Color::Default, cell_flags::DIM)
                };
                cells[r * cols + c] = Cell {
                    c: match (horiz, vert) {
                        (true, true) => '┼',
                        (true, false) => '│',
                        (false, true) => '─',
                        (false, false) => ' ',
                    },
                    fg,
                    bg: Color::Default,
                    flags,
                };
            }
        }

        // x-aa95: an edge drop zone lands on cells a PANE owns, which the
        // divider pass above skips by construction. Lit here, after the blit,
        // so the rim reads as a candidate the same way a seam does.
        if let Some((band_rows, band_cols)) = drop_zone.and_then(|z| self.drop_band(z)) {
            for r in band_rows.start as usize..(band_rows.end as usize).min(rows) {
                for c in band_cols.start as usize..(band_cols.end as usize).min(cols) {
                    if covered[r * cols + c] {
                        let cell = &mut cells[r * cols + c];
                        cell.fg = self.theme.accent;
                        cell.flags |= cell_flags::INVERSE;
                    }
                }
            }
        }

        if self.pane_ids_until.is_some_and(|until| now < until) {
            for (pid, rect) in &self.layout.panes {
                let label = format!("pane {pid}");
                let width = label.chars().count();
                let rect_cols = rect.cols as usize;
                let row = origin_r + rect.y as usize;
                if rect_cols < width || row >= rows {
                    continue;
                }
                let start = origin_c + rect.x as usize + rect_cols - width;
                for (offset, ch) in label.chars().enumerate() {
                    let col = start + offset;
                    if col < cols {
                        cells[row * cols + col] = Cell {
                            c: ch,
                            fg: Color::Default,
                            bg: Color::Default,
                            flags: cell_flags::INVERSE | cell_flags::DIM,
                        };
                    }
                }
            }
        }

        self.draw_bottom_row(&mut cells, rows, cols);
        let (overlay_origin, overlay_dims) = self.overlay_viewport();
        if let Some(lines) = &self.digest {
            // x-4e2d catch-up overlay: any key dismisses (handle_stdin, like the
            // key-table overlay). Framed chrome so it reads as one product with
            // the settings and connections modals.
            let chrome = chrome::Chrome::new("catch up", Anchor::Center).footer("any key closes");
            draw_lines_overlay(
                &mut cells,
                rows,
                cols,
                overlay_origin,
                overlay_dims,
                &chrome,
                lines,
                &self.theme,
                None,
            );
        } else if let Some(m) = &self.keys_modal {
            // x-8ccf US3: the centered which-key modal replaces the old top-left
            // key-table poster (opaque, sectioned, scrollable).
            popup::draw(
                &mut cells,
                rows,
                cols,
                &m.popup.render(self.term),
                &self.theme,
            );
        } else if let Some(m) = &self.row_menu {
            // x-8ccf US2: the anchored row context menu, drawn at the pointer.
            popup::draw(
                &mut cells,
                rows,
                cols,
                &m.popup.render(self.term),
                &self.theme,
            );
        } else if let Some(m) = &self.aux {
            // x-8ccf US4/US5: the sideline MENU popup or settings modal.
            popup::draw(
                &mut cells,
                rows,
                cols,
                &m.popup.render(self.term),
                &self.theme,
            );
        } else if let Some(sel) = self.answers {
            // x-feec needs-me queue (grown from the x-c929 answer overlay,
            // x-f730 folded MINE in as the first lane): MINE then the
            // severity-ranked THEY NEED YOU union, on the shared inverse-video
            // chrome. Always drawn while open - an empty union renders
            // "nothing needs you", a pending/failed fold renders its footer
            // notice (never a blank overlay).
            let projection = self.needs_projection();
            let total = projection.mine_shown + projection.need_shown;
            let sel = sel.min(total.saturating_sub(1));
            let lines =
                needs_overlay_lines(&projection, sel, self.mine_footer(), self.needs_footer());
            let chrome = chrome::Chrome::new("needs me", Anchor::Center).footer("q close");
            draw_lines_overlay(
                &mut cells,
                rows,
                cols,
                overlay_origin,
                overlay_dims,
                &chrome,
                &lines,
                &self.theme,
                // This queue is cursored (`n`/`N`) and unbounded, so it needs
                // the same follow the pickers do: on a short terminal the
                // selected row would otherwise sit below the fold while still
                // being the row Enter acts on. selected_line() accounts for
                // the MINE/THEY NEED YOU heading + footer lines between the
                // two lanes, which a flat `sel + 1` no longer can.
                Some(projection.selected_line(sel)),
            );
        } else if let Some(yv) = &self.yard {
            // (x-b2bf) The yard: the fleet as f[no]nimals. The
            // crowd is one eye glyph per roster citizen (each glyph computed
            // from that row's own badge/need values); the spotlight is ONE
            // 12-column sprite for the selected citizen, its eye from the
            // same reading, its species/rarity/crown/first-sighting from the
            // identity fold. A failed fold degrades to readings-only, never
            // blocks, never guesses a species.
            let crowd = self.yard_crowd();
            let sel = yv.sel.min(crowd.len().saturating_sub(1));
            let identity = crowd
                .get(sel)
                .and_then(|(name, _, _)| self.yard_identity(name));
            let frame = (yv.opened_at.elapsed().as_millis() / YARD_FRAME_MS)
                % crate::sprites::FRAME_COUNT as u128;
            let lines =
                yard_overlay_lines(&crowd, sel, identity, frame as usize, self.yard_footer());
            let chrome =
                chrome::Chrome::new("the yard", Anchor::Center).footer("n/N pick · q close");
            draw_lines_overlay(
                &mut cells,
                rows,
                cols,
                overlay_origin,
                overlay_dims,
                &chrome,
                &lines,
                &self.theme,
                None,
            );
        } else if let Some(picker) = &self.move_pick {
            // x-96e8 move picker: `move tab to:` / `move pane to:` + one
            // numbered line per candidate squad.
            let lines = self.move_pick_lines(picker);
            let chrome = chrome::Chrome::new("move to", Anchor::Center).footer("esc cancel");
            draw_lines_overlay(
                &mut cells,
                rows,
                cols,
                overlay_origin,
                overlay_dims,
                &chrome,
                &lines,
                &self.theme,
                // +1 for the header line: an uncapped list must not park its
                // cursor behind the fold (same reason as the attach picker).
                Some(picker.cursor + 1),
            );
        } else if let Some(picker) = &self.attach_place {
            let lines = self.attach_place_lines(picker);
            let chrome = chrome::Chrome::new("attach", Anchor::Center).footer("esc cancel");
            draw_lines_overlay(
                &mut cells,
                rows,
                cols,
                overlay_origin,
                overlay_dims,
                &chrome,
                &lines,
                &self.theme,
                // +1 for the header line: keep the CURSOR row on screen. Without
                // this an uncapped list on a short terminal would put the 10th
                // workspace behind the fold while still selecting it, which is
                // the same unreachability the .take(9) caused.
                Some(picker.cursor + 1),
            );
        } else if let Some(pick) = &self.portal_pick {
            // (x-9fd0) The portal-placement picker, the attach picker's sibling:
            // same cursor-on-screen discipline (+1 for the header line).
            let lines = self.portal_pick_lines(pick);
            let chrome = chrome::Chrome::new("portal", Anchor::Center).footer("esc cancel");
            draw_lines_overlay(
                &mut cells,
                rows,
                cols,
                overlay_origin,
                overlay_dims,
                &chrome,
                &lines,
                &self.theme,
                Some(pick.cursor + 1),
            );
        } else if let Some(conn) = &self.connections {
            // x-84d7 Connections modal: accounts + combos lists. Drawn from the
            // modal's own render (pure). This and the settings modal are the
            // pair from the operator's screenshot - they now share one chrome.
            let chrome = chrome::Chrome::new("connections", Anchor::Center).footer("esc close");
            draw_lines_overlay(
                &mut cells,
                rows,
                cols,
                overlay_origin,
                overlay_dims,
                &chrome,
                &conn.render(),
                &self.theme,
                None,
            );
        } else if let Some(peek) = &self.peek {
            // x-c376 peek overlay: the peeked agent row (re-read LIVE from the
            // layout, navigator-style) header + transcript. Drawn above nav
            // (mutually exclusive modes).
            let drows = self.display_rows();
            let agent = drows.get(peek.cursor).and_then(|r| match r {
                DisplayRow::Agent(a) => Some(*a),
                _ => None,
            });
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let reply = self.peek_input.as_ref().map(|(_, buf)| buf.as_str());
            let lines = peek_overlay_lines(agent, peek, reply, now_secs);
            let title = agent.map(|a| a.name.as_str()).unwrap_or("peek");
            let chrome = chrome::Chrome::new(title, Anchor::Center).footer("esc close");
            draw_lines_overlay(
                &mut cells,
                rows,
                cols,
                overlay_origin,
                overlay_dims,
                &chrome,
                &lines,
                &self.theme,
                None,
            );
        } else if let Some(nav) = &self.nav {
            // x-653d navigator: the filtered flat catalog + query/chip line. Rows
            // recompute per frame from the live layout (no cache), so a push
            // repopulates it.
            let filtered = self.nav_filtered(nav);
            let lines = nav_overlay_lines(&filtered, nav);
            let chrome = chrome::Chrome::new("navigator", Anchor::Center)
                .footer("type to filter · esc close");
            draw_lines_overlay(
                &mut cells,
                rows,
                cols,
                overlay_origin,
                overlay_dims,
                &chrome,
                &lines,
                &self.theme,
                // The navigator is the uncapped selector this whole change
                // points operators at, so its cursor must stay on screen too:
                // a session with more rows than the viewport would otherwise
                // drive the selection below the fold. +1 for the query line.
                Some(nav.cursor + 1),
            );
        }

        // Terminal cursor: the FOCUSED pane's, offset into its rect - the
        // one place the cursor may sit (AC1-UI/AC5-UI).
        let (mut cur_r, mut cur_c, mut cur_vis) = (0u16, 0u16, false);
        if self.selector.is_none()
            && self.answers.is_none()
            && self.yard.is_none()
            && self.digest.is_none()
            && self.move_pick.is_none()
            && self.attach_place.is_none()
            && self.portal_pick.is_none()
            && self.nav.is_none()
            && self.peek.is_none()
            && self.connections.is_none()
            && self.keys_modal.is_none()
            && self.row_menu.is_none()
            && self.aux.is_none()
        {
            if let Some((_, rect)) = self
                .layout
                .panes
                .iter()
                .find(|(id, _)| *id == self.layout.focus)
            {
                if let Some(f) = self.frames.get(&self.layout.focus) {
                    cur_r = TAB_BAR_ROWS + rect.y + f.cursor_row.min(rect.rows.saturating_sub(1));
                    cur_c = self.panel_w() + rect.x + f.cursor_col.min(rect.cols.saturating_sub(1));
                    if boxed {
                        // Never in the filler (AC1-UI), even mid-race when a
                        // stale rect exceeds the just-shrunk area.
                        cur_r = cur_r.min(TAB_BAR_ROWS + a_rows.saturating_sub(1));
                        cur_c = cur_c.min(self.panel_w() + a_cols.saturating_sub(1));
                    }
                    cur_vis = f.cursor_visible;
                }
            }
        }
        Frame {
            rows: rows as u16,
            cols: cols as u16,
            cells,
            cursor_row: cur_r,
            cursor_col: cur_c,
            cursor_visible: cur_vis,
            // The composed full-terminal frame is not itself scrolled; the
            // per-pane indicator is drawn INTO the cells above from each pane
            // frame's own scroll_offset.
            scroll_offset: 0,
        }
    }

    /// The bottom chrome line (US4). While a prefix chord is pending past
    /// [`HINT_DELAY`] it is the which-key hint (painted over whatever the row
    /// held - even with the status row toggled off, discoverability does not
    /// die with the toggle; tmux's message-line behavior). Otherwise it is
    /// the status row (AC4-UI): session name, focused pane cwd, the focused
    /// pane's scroll offset (the canonical `[+N]` home; the per-pane inline
    /// indicator stays so a scrolled UNFOCUSED pane is still observable),
    /// and `? for keys`. Too-short terminals draw neither (AC4-ERR).
    /// The bottom terminal row is chrome (search line / which-key hint / status
    /// row, painted last by `draw_bottom_row`) rather than content or a sideline
    /// row drawn underneath. Below minimum geometry both auto-hide (AC4-ERR) and
    /// the row is content (`content_dims` handed the server the full height, a
    /// pane tiled into it, so blanking would erase it). The single truth shared
    /// by the renderer and `chrome_hit` so a click matches what's painted
    /// (codex P2).
    fn bottom_row_is_chrome(&self) -> bool {
        self.term.0 >= MIN_ROWS_FOR_STATUS
            && (self.confirm.is_some()
                || self.create.is_some()
                || self.rename.is_some()
                || self.move_to.is_some()
                || self.recruit.is_some()
                || self.search.is_some()
                || self.hint
                || self.status_on)
    }

    /// A centered, inverse-video name-entry modal for the create / rename /
    /// recruit inputs. Those used to paint the bottom-left chrome row, where they
    /// sat outside the operator's field of view and read as "nothing happened";
    /// centering on a mid-screen inverse-video line puts the prompt where the
    /// operator is looking and names its target. The bottom chrome row stays
    /// blanked so a stale bottom row never shows under the modal.
    ///
    /// Reported as "I can barely see the prompt", and the fix is the BLOCK: a
    /// one-row strip hugging its own glyphs is hard to find in busy pane
    /// content, which is a different complaint from hard to read. It already
    /// measured 9.9:1 on the reporter's scheme.
    ///
    /// It must not stack `BOLD` on the inversion: bold brightens the foreground,
    /// which reverse has made the background. The shared framer handles that.
    ///
    /// (x-b465) It wears the SHARED chrome now, the same `Chrome` + `frame` +
    /// `blit` path the settings, connections and catch-up modals take, with the
    /// target as the title and the blank-clears rule as the footer. It used to
    /// hand-paint a bare three-row inverse block with no border, no title bar
    /// and no esc chip, which under a named theme read as a different
    /// application dropped into the middle of the screen. That was the last
    /// modal still inventing its own look.
    fn name_modal_layout(&self, label: &str, name: &str, hint: Option<&str>) -> OverlayLayout {
        let (origin, dims) = self.overlay_viewport();
        // The typed name plus its cursor IS the body; the target and the
        // blank-clears rule move into the chrome, where every other modal puts
        // them.
        let mut chrome = chrome::Chrome::new(label, Anchor::Center);
        if let Some(h) = hint {
            chrome = chrome.footer(h);
        }
        // The footer widens the frame to fit, so on a narrow viewport it has to
        // be clamped or the right border leaves the screen.
        chrome = chrome.fit_to(dims.1.saturating_sub(chrome::Chrome::FRAME_COLS));
        // A name longer than the body scrolls, keeping the CURSOR end visible.
        // Stamping the head instead cuts the `_` off the right edge, so on a
        // narrow terminal the operator types a name they cannot see - the one
        // thing a name prompt has to get right. The shared framer truncates from
        // the head, so the tail-keeping happens HERE, before it is handed over.
        let body_w = dims.1.saturating_sub(chrome::Chrome::FRAME_COLS).max(1);
        let text = format!("{name}_");
        let text = if text.chars().count() > body_w {
            let drop = text.chars().count() - body_w;
            let kept: String = text.chars().skip(drop + 1).collect();
            format!("…{kept}")
        } else {
            text
        };
        layout_lines_overlay(origin, dims, &chrome, &[text], None, OverlayAnchor::Center)
    }

    fn draw_name_modal(
        &self,
        cells: &mut [Cell],
        rows: usize,
        cols: usize,
        label: &str,
        name: &str,
        hint: Option<&str>,
    ) {
        if rows > 0 {
            for c in 0..cols {
                cells[(rows - 1) * cols + c] = Cell::default();
            }
        }
        let layout = self.name_modal_layout(label, name, hint);
        draw_overlay_layout(cells, rows, cols, &layout, &self.theme);
    }

    fn draw_bottom_row(&self, cells: &mut [Cell], rows: usize, cols: usize) {
        if !self.bottom_row_is_chrome() {
            return;
        }
        // A card-dispatch confirm is modal - it owns the row above everything
        // else while the operator decides (x-a496).
        if let Some(c) = &self.confirm {
            self.draw_confirm_line(cells, rows, cols, c);
            return;
        }
        // The new-workspace name input is a centered modal (x-9e5e); the operator
        // is mid-entry, so it sits above search/hint/status.
        if let Some(name) = &self.create {
            self.draw_name_modal(cells, rows, cols, "new workspace", name, None);
            return;
        }
        // The rename input (x-c150 tab; widened x-96e8 to squads): the noun tracks
        // the target so the operator sees what they are renaming, and the hint
        // spells out the blank-clears semantics.
        if let Some((target, name)) = &self.rename {
            let noun = match target {
                RenameTarget::Tab(_) => "tab",
                RenameTarget::Squad(_) => "workspace",
                RenameTarget::Agent(_) => "row",
            };
            let hint = match target {
                RenameTarget::Agent(_) => Some("a-z 0-9 - _ (1-64 chars)"),
                _ => Some("empty resets to auto"),
            };
            self.draw_name_modal(cells, rows, cols, &format!("rename {noun}"), name, hint);
            return;
        }
        // The move-to prompt (x-cf97): the typed number IS the body; the hint
        // names the grammar so a `4` never reads as "move 4 left".
        if let Some((_, buf)) = &self.move_to {
            self.draw_name_modal(
                cells,
                rows,
                cols,
                "move tab to position",
                buf,
                Some("1-based; Enter moves"),
            );
            return;
        }
        // The recruit workspace-name input (x-8f11): the hint names how many
        // marked agents will join (create-if-absent).
        if let Some(name) = &self.recruit {
            let n = self.marks.len();
            self.draw_name_modal(
                cells,
                rows,
                cols,
                &format!("recruit {n} into"),
                name,
                Some("create-if-absent"),
            );
            return;
        }
        // Search line takes the bottom row when active (precedence: search >
        // which-key hint > status row). It OVERLAYS whatever held the row - no
        // reserved row, so opening search never triggered a Resize/reflow.
        if let Some(sv) = &self.search {
            self.draw_search_line(cells, rows, cols, sv);
            return;
        }
        let r = rows - 1;
        // We own the row: blank it first so the divider-fill pass in `compose`
        // (which treats this uncovered row as content and paints '─' glyphs)
        // cannot bleed through the gaps between the segments below.
        for c in 0..cols {
            cells[r * cols + c] = Cell::default();
        }
        let put = |cells: &mut [Cell], c: usize, ch: char, flags: u8| {
            if c < cols {
                cells[r * cols + c] = Cell {
                    c: ch,
                    fg: Color::Default,
                    bg: Color::Default,
                    flags,
                };
            }
        };
        if self.hint {
            let text = crate::keys::prefix_hint();
            for (i, ch) in text.chars().take(cols).enumerate() {
                put(cells, i, ch, 0);
            }
            return;
        }
        let mut c = 0usize;
        for ch in format!(" {} ", self.session).chars() {
            put(cells, c, ch, cell_flags::BOLD);
            c += 1;
        }
        // Active squad's name, only when there is more than one squad to be
        // ambiguous about (x-2f99) - the always-visible answer to "which
        // squad?" when the sideline is toggled off or auto-hidden. BOLD: it
        // is identity, like the session cell, not context like the cwd.
        if self.layout.squads.len() > 1 {
            if let Some(s) = self
                .layout
                .squads
                .iter()
                .find(|s| s.id == self.layout.active_squad)
            {
                for ch in format!("│ {} ", s.name).chars() {
                    put(cells, c, ch, cell_flags::BOLD);
                    c += 1;
                }
            }
        }
        let cwd = self
            .layout
            .squads
            .iter()
            .find(|s| s.id == self.layout.active_squad)
            .map(|s| abbrev_home(&s.canonical_cwd))
            .unwrap_or_default();
        for ch in format!("│ {cwd} ").chars() {
            put(cells, c, ch, cell_flags::DIM);
            c += 1;
        }
        // Provenance cell for the focused pane (x-66e8): config-free `⚑ <node>`,
        // shown only when the focused pane was node-driven. Absent for an ad-hoc
        // pane, so a plain shell reads clean.
        if let Some(node) = &self.layout.focus_node {
            for ch in format!("⚑ {node} ").chars() {
                put(cells, c, ch, cell_flags::BOLD);
                c += 1;
            }
        }
        if let Some(f) = self.frames.get(&self.layout.focus) {
            if f.scroll_offset != 0 {
                for ch in format!("[+{}] ", f.scroll_offset).chars() {
                    put(cells, c, ch, cell_flags::INVERSE);
                    c += 1;
                }
            }
        }
        // The whole-machine meter, when toggled on: the latest one-line
        // reading, or an explicit "sensor unavailable" until a sample lands.
        // A dark sensor is named - the row never shows a zero or a blank as
        // if it were a reading.
        if self.resource_meter_on {
            let text = self
                .resource_meter_text
                .clone()
                .unwrap_or_else(|| "meter: sensor unavailable".into());
            for ch in format!("│ {text} ").chars() {
                put(cells, c, ch, cell_flags::DIM);
                c += 1;
            }
        }
        let help = "? for keys ";
        let start = cols.saturating_sub(help.chars().count());
        if start > c {
            for (i, ch) in help.chars().enumerate() {
                put(cells, start + i, ch, cell_flags::DIM);
            }
        }
    }

    /// Paint the confirm prompt over the bottom row (x-a496 dispatch; x-96e8
    /// squad removal). Blank first (the x-5041 divider-bleed gotcha), then the
    /// BOLD prompt whose wording tracks the action being confirmed.
    /// (x-f331) The outer row a live confirm prompt paints on: the acted-on
    /// sideline row when it is still in the catalog and visible, else the bottom
    /// row. Anchoring the prompt at the target row is AC2-UI - a bottom-row
    /// prompt far from the row reads as "nothing happened". The target is
    /// resolved by IDENTITY every paint (via [`View::confirm_target_index`]), not
    /// a captured index, so a scrape/layout push that reorders or removes rows
    /// re-anchors to the still-valid row or dismisses to the bottom - it can never
    /// paint beside an unrelated row that drifted under a stale index (AC1-FR,
    /// codex P2). The dispatch is likewise identity-keyed, so it is drift-safe too.
    fn confirm_anchor_row(&self, rows: usize, action: &ConfirmAction) -> usize {
        let bottom = rows.saturating_sub(1);
        match self.confirm_target_index(action) {
            Some(i) if i >= self.sideline_offset && (i - self.sideline_offset) < bottom => {
                i - self.sideline_offset
            }
            _ => bottom,
        }
    }

    /// (x-f331) The display-row index the confirm's target CURRENTLY occupies,
    /// matched by the identity carried in the [`ConfirmAction`] (squad id, agent
    /// name, or external/dismiss attach_id, or a card node) - never a captured
    /// numeric index. A global confirm (reap / clear-dead) has no row, and a
    /// target that vanished returns `None`; both fall back to the bottom row.
    fn confirm_target_index(&self, action: &ConfirmAction) -> Option<usize> {
        self.display_rows()
            .iter()
            .position(|r| match (&action.action, r) {
                (ConfirmKind::RemoveSquad { squad, .. }, DisplayRow::Sel(s)) => {
                    s.tab.is_none() && s.squad == *squad
                }
                (
                    ConfirmKind::StopAgent { name, .. } | ConfirmKind::RemoveAgent { name, .. },
                    DisplayRow::Agent(a),
                ) => a.name == *name,
                (
                    ConfirmKind::StopExternal { attach_id, .. }
                    | ConfirmKind::RemoveExternal { attach_id, .. }
                    | ConfirmKind::DismissMember { attach_id, .. },
                    DisplayRow::Agent(a),
                ) => a.attach_id.as_deref() == Some(attach_id.as_str()),
                (ConfirmKind::Dispatch { node }, DisplayRow::Card(c)) => c.id == *node,
                _ => false,
            })
    }

    fn confirm_text(&self, action: &ConfirmAction) -> String {
        let label = &action.label;
        let text = match &action.action {
            ConfirmKind::Dispatch { .. } => format!("start session on {label}?"),
            ConfirmKind::RemoveSquad {
                panes, last: true, ..
            } => format!(
                "close workspace {label} ({panes} panes) - last workspace, ends the session?"
            ),
            ConfirmKind::RemoveSquad {
                panes, last: false, ..
            } => {
                format!("close workspace {label} ({panes} panes)?")
            }
            ConfirmKind::StopAgent { .. } => format!("stop {label}?"),
            ConfirmKind::RemoveAgent { .. } => format!("remove {label}?"),
            ConfirmKind::ReapAgents => "reap all exited fno agents?".to_string(),
            ConfirmKind::StopExternal { .. } => format!("stop {label}?"),
            ConfirmKind::RemoveExternal { .. } => {
                format!("remove {label} and worktree?")
            }
            ConfirmKind::DismissMember { .. } => format!("dismiss {label}?"),
            ConfirmKind::ClearDead { dead, .. } => {
                format!("clear {dead} dead row(s) in {label}?")
            }
            // A tab close is a GROUP close on the wire: the server reaps every
            // leaf in the tab, not the focused pane. Say the count when there is
            // more than one, so the prompt names what the Enter destroys. Read
            // from the LIVE layout at draw time, never from the capture: the
            // prompt may have sat open while panes came and went, and the
            // commit re-resolves the tab for exactly the same reason.
            ConfirmKind::CloseTab { tab } => match self.find_tab(*tab) {
                Some((_, _, t)) if t.panes.len() > 1 => {
                    format!("close tab {label} and its {} panes?", t.panes.len())
                }
                _ => format!("close tab {label}?"),
            },
        };
        text
    }

    fn confirm_overlay_layout(&self, rows: usize, action: &ConfirmAction) -> OverlayLayout {
        let (origin, dims) = self.overlay_viewport();
        let chrome = chrome::Chrome::new("confirm", Anchor::Center)
            .footer("enter confirm · esc cancel")
            .fit_to(dims.1.saturating_sub(chrome::Chrome::FRAME_COLS));
        let lines = [self.confirm_text(action)];
        let anchor = if matches!(&action.action, ConfirmKind::CloseTab { .. }) {
            OverlayAnchor::Center
        } else {
            OverlayAnchor::At {
                row: self.confirm_anchor_row(rows, action),
                col: origin.1,
            }
        };
        layout_lines_overlay(origin, dims, &chrome, &lines, None, anchor)
    }

    /// The Connections modal's hit-test layout: the draw arm and this build
    /// the SAME `layout_lines_overlay` block from the same chrome and body,
    /// so a click resolves to the cell the operator sees (AC: the footer's
    /// esc close words are a target, not a label).
    fn connections_overlay_layout(
        &self,
        _rows: usize,
        conn: &crate::connections_view::ConnectionsView,
    ) -> OverlayLayout {
        let (origin, dims) = self.overlay_viewport();
        let chrome = chrome::Chrome::new("connections", Anchor::Center).footer("esc close");
        layout_lines_overlay(
            origin,
            dims,
            &chrome,
            &conn.render(),
            None,
            OverlayAnchor::Center,
        )
    }

    /// The peek overlay's hit-test layout, rebuilt from the same LIVE inputs
    /// the draw arm reads (layout rows, reply buffer, wall clock). A second
    /// tick between draw and click can move a width by a character; the hit
    /// spans are wide enough that this never unclicks the footer.
    fn peek_overlay_layout(&self, _rows: usize, peek: &PeekView) -> OverlayLayout {
        let (origin, dims) = self.overlay_viewport();
        let drows = self.display_rows();
        let agent = drows.get(peek.cursor).and_then(|r| match r {
            DisplayRow::Agent(a) => Some(*a),
            _ => None,
        });
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let reply = self.peek_input.as_ref().map(|(_, buf)| buf.as_str());
        let lines = peek_overlay_lines(agent, peek, reply, now_secs);
        let title = agent.map(|a| a.name.as_str()).unwrap_or("peek");
        let chrome = chrome::Chrome::new(title, Anchor::Center).footer("esc close");
        layout_lines_overlay(origin, dims, &chrome, &lines, None, OverlayAnchor::Center)
    }

    fn draw_confirm_line(
        &self,
        cells: &mut [Cell],
        rows: usize,
        cols: usize,
        action: &ConfirmAction,
    ) {
        if rows > 0 {
            for c in 0..cols {
                cells[(rows - 1) * cols + c] = Cell::default();
            }
        }
        let layout = self.confirm_overlay_layout(rows, action);
        draw_overlay_layout(cells, rows, cols, &layout, &self.theme);
    }

    /// Build the move-tab picker overlay lines (x-96e8): a header plus one
    /// numbered line per candidate squad, the number being the digit that
    /// selects it. A candidate that vanished from the catalog since open still
    /// renders (labelled) - the digit is re-validated on press, and the server
    /// refuses a stale id regardless.
    fn move_pick_lines(&self, picker: &MovePick) -> Vec<String> {
        const W: usize = 40;
        let verb = match picker.src {
            MoveSrc::Tab(_) => "move tab to:",
            MoveSrc::Pane(_) => "move pane to:",
        };
        let mut lines = vec![pad_to(&format!(" {verb}"), W)];
        for (i, &sid) in picker.squads.iter().enumerate() {
            let name = self
                .layout
                .squads
                .iter()
                .find(|s| s.id == sid)
                .map(|s| s.name.as_str())
                .unwrap_or("(gone)");
            let marker = if i == picker.cursor { '›' } else { ' ' };
            // Only the first nine are digit-addressable; the rest get a blank
            // gutter, so the drawn numbering never promises a key that does
            // not exist.
            let ord = if i < 9 {
                (i + 1).to_string()
            } else {
                " ".into()
            };
            lines.push(pad_to(&format!(" {marker} {ord} {name}"), W));
        }
        lines.push(pad_to(" hjkl/arrows move · enter move here", W));
        lines.push(pad_to(" 1-9 move to first nine · esc/q cancel", W));
        lines
    }

    /// Paint the in-scrollback search line over the bottom row (v12, x-e780).
    /// Blank first so the divider-fill pass cannot bleed through (x-5041 gotcha).
    /// Typing shows `/query_`; browsing shows `[i/n] /query` or `/query - no
    /// matches` (total 0).
    fn draw_search_line(&self, cells: &mut [Cell], rows: usize, cols: usize, sv: &SearchView) {
        let r = rows - 1;
        for c in 0..cols {
            cells[r * cols + c] = Cell::default();
        }
        let text = match sv.result {
            Some((0, _)) => format!(" /{} - no matches", sv.query),
            Some((total, current)) => format!(" [{current}/{total}] /{}", sv.query),
            // Typing, or submitted but awaiting the first reply: show the input.
            None => format!(" /{}_", sv.query),
        };
        for (i, ch) in text.chars().take(cols).enumerate() {
            cells[r * cols + i] = Cell {
                c: ch,
                fg: Color::Default,
                bg: Color::Default,
                flags: cell_flags::BOLD,
            };
        }
    }

    /// The tab-bar spans in paint order: the active squad's name (inert), its
    /// tabs, then a `+` new-tab affordance. The single source both `draw_tab_bar`
    /// and `chrome_hit` walk, so a click always lands on the glyph under it.
    fn tab_bar_spans(&self) -> Vec<TabSpan> {
        let mut spans = Vec::new();
        let Some(s) = self
            .layout
            .squads
            .iter()
            .find(|s| s.id == self.layout.active_squad)
        else {
            return spans;
        };
        spans.push(TabSpan {
            text: format!(" {} ", brand_label(&s.name)),
            flags: cell_flags::BOLD,
            fg: Color::Default,
            hit: None,
            role: SpanRole::Squad,
        });
        for (i, t) in s.tabs.iter().enumerate() {
            let label = tab_group_label(tab_label_text(&t.name, i, t.named), t.panes.len());
            // x-df4c US4: a leading max-severity rollup glyph so a background
            // tab's blocked/working pane reads at the strip without opening it,
            // carrying the lattice's weight and color. `None` (no live panes:
            // empty or all-exited) prepends nothing, so a stateless tab renders
            // byte-identically to today (AC2-EDGE); a live-idle tab shows `○`; a
            // Blocked rollup paints the amber accent on the span (label preceded
            // by `▲`), and the working/blocked/done glyphs keep their BOLD.
            let (glyph_prefix, fg, glyph_flags) =
                match tab_rollup_state(&self.layout.agents, s.id, t.id) {
                    Some(st) => {
                        let style = lattice_style(st, self.theme.accent);
                        (format!("{} ", style.glyph), style.fg, style.flags)
                    }
                    None => (String::new(), Color::Default, 0),
                };
            let label = format!("{glyph_prefix}{label}");
            let base_flags = if i == s.active_tab {
                cell_flags::INVERSE
            } else {
                0
            };
            let text = if i == s.active_tab {
                format!("[{label}]")
            } else {
                format!(" {label} ")
            };
            spans.push(TabSpan {
                text,
                flags: base_flags | glyph_flags,
                fg,
                hit: Some(TabHit::Tab(t.id)),
                role: SpanRole::Tab,
            });
        }
        spans.push(TabSpan {
            text: " + ".to_string(),
            flags: cell_flags::DIM,
            fg: Color::Default,
            hit: Some(TabHit::NewTab),
            role: SpanRole::NewTab,
        });
        spans
    }

    /// The span run that actually FITS the strip: a scrolling window over
    /// [`tab_bar_spans`] anchored on the active tab, with a clickable overflow
    /// counter at whichever edge is hiding tabs.
    ///
    /// `draw_tab_bar` used to stop painting at the right edge, and `chrome_hit`
    /// walks the same spans, so a tab past it was invisible AND unclickable.
    ///
    /// Anchoring on the active tab means the existing keys already drive the
    /// viewport, so reaching a twentieth tab needs no new binding. The squad
    /// label and the `+` are pinned outside the scroll region: the `+` is the
    /// only mouse route to a new tab. Below overflow this returns
    /// [`tab_bar_spans`] unchanged.
    fn tab_bar_window(&self) -> Vec<TabSpan> {
        let width = (self.term.1 as usize).saturating_sub(self.panel_w() as usize);
        let span_w = |s: &TabSpan| s.text.chars().count();
        let full = self.tab_bar_spans();
        if full.iter().map(span_w).sum::<usize>() <= width {
            return full;
        }
        // [squad label][tab..][+]: the label leads and the `+` trails by
        // construction, so both peel off without searching for them.
        let Some(plus) = full
            .last()
            .filter(|s| matches!(s.hit, Some(TabHit::NewTab)))
        else {
            return full;
        };
        let (plus, label) = (plus.clone(), full[0].clone());
        let tabs: Vec<TabSpan> = full[1..full.len() - 1].to_vec();
        let avail = width
            .saturating_sub(span_w(&label))
            .saturating_sub(span_w(&plus));
        // `‹12 ` / ` 12›`: two glyph columns plus the count's digits.
        let marker_w = |n: usize| n.to_string().chars().count() + 2;
        // Pack forward from `start`, always reserving room for the counter that
        // will be needed at each edge, and return one past the last span that
        // fits. Reserving inside the loop (not after it) is what keeps the right
        // counter from being the thing that overflows the strip.
        let fit_end = |start: usize| -> usize {
            let mut used = if start > 0 { marker_w(start) } else { 0 };
            let mut end = start;
            while end < tabs.len() {
                let rest_after = tabs.len() - end - 1;
                let reserve = if rest_after > 0 {
                    marker_w(rest_after)
                } else {
                    0
                };
                if used + span_w(&tabs[end]) + reserve > avail {
                    break;
                }
                used += span_w(&tabs[end]);
                end += 1;
            }
            end
        };
        let active = self
            .layout
            .squads
            .iter()
            .find(|s| s.id == self.layout.active_squad)
            .map(|s| s.active_tab)
            .unwrap_or(0)
            .min(tabs.len().saturating_sub(1));
        // The smallest window that still contains the active tab AND one tab of
        // lookahead past it. Minimal scrolling means stepping back with
        // `prefix+p` does not yank a strip that already showed the target; the
        // lookahead means stepping forward with `prefix+n` slides the strip by
        // one instead of jumping it on every single press.
        let target = (active + 1).min(tabs.len().saturating_sub(1));
        let mut start = 0usize;
        while start < active && fit_end(start) <= target {
            start += 1;
        }
        let end = fit_end(start).max(start + 1).min(tabs.len());
        let mut out = vec![label];
        if start > 0 {
            out.push(TabSpan {
                text: format!("\u{2039}{start} "),
                flags: cell_flags::DIM,
                fg: Color::Default,
                // Clicking the counter selects the nearest tab it is hiding, so
                // the strip walks toward the hidden end one click at a time.
                hit: tabs[start - 1].hit,
                role: SpanRole::OverflowLeft(start),
            });
        }
        out.extend(tabs[start..end].iter().cloned());
        if end < tabs.len() {
            out.push(TabSpan {
                text: format!(" {}\u{203a}", tabs.len() - end),
                flags: cell_flags::DIM,
                fg: Color::Default,
                hit: tabs[end].hit,
                role: SpanRole::OverflowRight(tabs.len() - end),
            });
        }
        out.push(plus);
        // Narrow mode: `fit_end` can return `start` when not one whole tab fits
        // beside the pinned chrome, and forcing it in would push the counter and
        // the `+` off the edge to be clipped. A truncated label still says which
        // tab you are on and still hit-tests; a missing `+` is unreachable.
        condense_to_width(&mut out, width);
        out
    }

    fn draw_tab_bar(&self, cells: &mut [Cell], cols: usize) {
        // (x-cd67 US1) The strip is scoped to the content area: it begins at
        // the content-column origin (`panel_w`) so the sideline column owns row
        // 0 too and tabs read as owned by the active workspace rather than the
        // reverse. `panel_w == 0` (no sideline) -> origin 0, byte-identical to
        // the pre-scoping full-width strip.
        // (x-d6a8 G1) While a pane is dragged, the whole strip reads as a break
        // target (accent), brightening to INVERSE the moment the pointer is over
        // it (the "drop here to break into a new tab" affordance). (G2) While a
        // tab cell is dragged, that cell dims to read as lifted - the same
        // origin-marking the pane grip uses during a relocation.
        let break_active = self.pane_drag.map(|d| d.on_strip);
        let lifted_tab = self.tab_drag.map(|d| d.src_tab);
        let mut c = self.panel_w() as usize;
        'spans: for span in self.tab_bar_window() {
            let (fg, flags) = if let Some(on_strip) = break_active {
                let f = if on_strip {
                    cell_flags::INVERSE
                } else {
                    cell_flags::BOLD
                };
                (self.theme.accent, f)
            } else if matches!((lifted_tab, span.hit), (Some(t), Some(TabHit::Tab(tid))) if t == tid)
            {
                (span.fg, span.flags | cell_flags::DIM)
            } else {
                (span.fg, span.flags)
            };
            for ch in span.text.chars() {
                if c >= cols {
                    break 'spans;
                }
                cells[c] = Cell {
                    c: ch,
                    fg,
                    bg: Color::Default,
                    flags,
                };
                c += 1;
            }
        }
        // Transient notice, right-aligned, INVERSE (paired with the BEL the
        // event handler already sounded); painted by row_stamp.
        paint_notice_overlay(cells, cols, self.notice_overlay(cols));
    }

    /// The hosting-tab context for an agent row, resolved inside-out (x-0f9d
    /// US3): a NAMED tab supplies its name; an unnamed tab supplies the `·N`
    /// ordinal (today's form). `None` = the row has no tab (a paneless /
    /// watch-only row), so the caller falls back to the squad name.
    fn agent_tab_context(&self, squad: Option<u64>, tab: Option<TabId>) -> Option<TabContext> {
        let sid = squad?;
        let tid = tab?;
        let s = self.layout.squads.iter().find(|s| s.id == sid)?;
        let (i, t) = s.tabs.iter().enumerate().find(|(_, t)| t.id == tid)?;
        Some(if t.named {
            TabContext::Named(t.name.clone())
        } else {
            TabContext::Ordinal(i + 1)
        })
    }

    /// The sideline's display order (4a-G2): each squad's squad/tab rows, that
    /// squad's agent rows, then the `+ new workspace` footer, a catch-all
    /// section for agents matched to no squad, and the work-queue lane. The
    /// ONE row enumeration (x-260a): painting, hover, mouse hit-testing, and
    /// the prefix+w selector all index into it.
    /// The sideline's rows for the CURRENT density - the one enumeration every
    /// consumer indexes into (x-260a: painting, hover, hit-test, and the
    /// selector share this index space in all three densities).
    ///
    /// Slim is a FILTER over the regular rows rather than a second builder, so
    /// it inherits section keys, rollup folding, and ordering for free and
    /// cannot drift from the tree it is a summary of. Extended keeps the same
    /// structural rows and changes only agent-row composition and ordering.
    fn display_rows(&self) -> Vec<DisplayRow<'_>> {
        match self.density {
            Density::Regular => self.tree_rows(),
            // Header bands only: squad name rows (a `Sel` with no tab) and the
            // `~` section headers. Both already carry their rollup counts, which
            // is what keeps the rail legible rather than blind.
            Density::Slim => self
                .tree_rows()
                .into_iter()
                .filter(|r| {
                    matches!(r, DisplayRow::Sel(s) if s.tab.is_none())
                        || matches!(r, DisplayRow::Header { .. })
                })
                .collect(),
            Density::Extended => self.table_rows_with_depths().0,
        }
    }

    /// (x-132c) [`Self::display_rows`] plus the lineage depth of each rendered
    /// row, aligned index-for-index. The depths come from the SAME pass that
    /// builds the rows, so they are computed over exactly the set that paints:
    /// an idle parent the fold removed, or an exited parent a LiveOnly section
    /// hides, is ABSENT, and its child roots instead of indenting under a row
    /// that never rendered. Non-agent rows (headers, sublines, spacers) carry
    /// 0. Slim filters the pair exactly as it filters rows; Extended retains
    /// the same lineage depths inside its agent-name cells.
    fn display_rows_with_depths(&self) -> (Vec<DisplayRow<'_>>, Vec<usize>) {
        match self.density {
            Density::Regular => self.tree_rows_with_depths(),
            Density::Slim => {
                let (rows, depths) = self.tree_rows_with_depths();
                let mut kept_rows = Vec::with_capacity(rows.len());
                let mut kept_depths = Vec::with_capacity(depths.len());
                for (r, d) in rows.into_iter().zip(depths) {
                    let keep = matches!(r, DisplayRow::Sel(s) if s.tab.is_none())
                        || matches!(r, DisplayRow::Header { .. });
                    if keep {
                        kept_rows.push(r);
                        kept_depths.push(d);
                    }
                }
                (kept_rows, kept_depths)
            }
            Density::Extended => self.table_rows_with_depths(),
        }
    }

    /// The extended density keeps the regular structural enumeration. Agent
    /// rows are grouped with their optional sublines and sorted only within
    /// the contiguous group beneath one section header.
    fn table_rows_with_depths(&self) -> (Vec<DisplayRow<'_>>, Vec<usize>) {
        let (rows, depths) = self.tree_rows_with_depths();
        let needs = self.attention_needs();
        let now = crate::digest_overlay::now_secs();
        let mut out: Vec<(DisplayRow<'_>, usize)> = Vec::with_capacity(rows.len() + 1);
        let mut group = Vec::new();
        let mut iter = rows.into_iter().zip(depths).peekable();

        while let Some((row, depth)) = iter.next() {
            match row {
                DisplayRow::Agent(agent) => {
                    let mut item = vec![(DisplayRow::Agent(agent), depth)];
                    while matches!(iter.peek(), Some((DisplayRow::Sub(_), _))) {
                        item.push(iter.next().expect("peeked subline"));
                    }
                    group.push((item, agent));
                    if !matches!(iter.peek(), Some((DisplayRow::Agent(_), _))) {
                        append_sorted_agent_group(
                            &mut out,
                            &mut group,
                            self.agent_sort,
                            &needs,
                            now,
                        );
                    }
                }
                row => {
                    append_sorted_agent_group(&mut out, &mut group, self.agent_sort, &needs, now);
                    out.push((row, depth));
                }
            }
        }
        append_sorted_agent_group(&mut out, &mut group, self.agent_sort, &needs, now);

        let has_agent = out
            .iter()
            .any(|(row, _)| matches!(row, DisplayRow::Agent(_)));
        out.insert(0, (DisplayRow::TableHead, 0));
        if !has_agent {
            out.insert(1, (DisplayRow::TableEmpty, 0));
        }
        out.into_iter().unzip()
    }

    // (x-c5ee) The sideline tree, with the top-K idle cap applied. A PURE
    // function of state: a squad shows its idle overflow only when the operator
    // toggled its `+N more` row open (`idle_expanded`). No per-frame,
    // selector-driven force-expand - that needs to know which row the selector
    // rests on, but the selector is an index INTO this very output, so any
    // attempt to resolve it here is circular: resolving against a rebuilt
    // (capped or uncapped) enumeration mis-identifies the row once a fold sits
    // above the cursor, and navigating onto a newly exposed overflow row then
    // collapses under the next render (codex P1/P2 on #566/#568). The invariant
    // that actually matters - the cursor never rests on a folded row - holds for
    // free: folded rows are absent from this list, so selector navigation only
    // ever lands on rendered rows. Overflow is reached by the fold row's own
    // Enter/click toggle, which persists in `idle_expanded`.
    fn tree_rows(&self) -> Vec<DisplayRow<'_>> {
        self.tree_rows_with_depths().0
    }

    /// [`Self::tree_rows`] with the per-row lineage depth beside it (see
    /// [`Self::display_rows_with_depths`]). The depth is computed over the
    /// EMITTED set after every display filter, never re-derived in the painter.
    fn tree_rows_with_depths(&self) -> (Vec<DisplayRow<'_>>, Vec<usize>) {
        let mut out = Vec::new();
        // Display index -> lineage depth for agent rows, filled at emit time.
        let mut agent_depth_at: HashMap<usize, usize> = HashMap::new();
        // (x-cd67 US3) Section spacing only with more than one workspace: a
        // single squad has no groups to separate (US3 verify: absent with 1
        // squad).
        let multi_squad = self.layout.squads.len() > 1;
        // Real workspaces only: a mission squad renders later under the
        // `~ missions` band, never as a workspace section (it can hold no agent).
        let real_squads: Vec<&SquadMeta> = self
            .layout
            .squads
            .iter()
            .filter(|s| !is_mission_squad(s.id))
            .collect();
        for (idx, s) in real_squads.into_iter().enumerate() {
            // One spacer between consecutive workspace groups (never before the
            // first, so no leading blank and never doubled).
            if multi_squad && idx > 0 {
                out.push(DisplayRow::Blank);
            }
            out.push(DisplayRow::Sel(SelRow {
                squad: s.id,
                tab: None,
            }));
            // Agents-first (x-0090, Locked 4): the caret gates the squad's agent
            // rows; tab rows are gone (tabs live in the top tab bar). A collapsed
            // squad shows only its name row + the x-d140 rollup glyph, so the
            // rollup is the sole signal there - no more agent rows rendering
            // unconditionally under a folded squad.
            // (x-975a) Tri-state: `LiveOnly` drops the exited rows in place
            // while the header's `✗N` rollup keeps them discoverable; live rows
            // keep their original order. Display filtering only - nothing is
            // reaped (that is x-f300).
            let key = section_key(s);
            let view = self.section_view(&key);
            if view != SectionView::Collapsed {
                let section_base = section_project_base(&s.canonical_cwd);
                let mut squad_agents: Vec<&AgentRow> = self
                    .layout
                    .agents
                    .iter()
                    .filter(|a| a.squad == Some(s.id))
                    .filter(|a| view == SectionView::Expanded || !a.exited)
                    .collect();
                // (x-132c) Order by lineage: children render beneath their
                // parent, pre-order. Keyed by row identity (input index), never
                // display name - two panes can share a label. A squad with no
                // parent edges keeps its input order (pane-tree, then
                // name-sorted watch-only), so it paints byte-identical.
                let (order, _) = lineage_layout(
                    &squad_agents,
                    |a| a.harness_session_id.as_deref(),
                    |a| a.spawned_by_session.as_deref(),
                );
                squad_agents = order.into_iter().map(|i| squad_agents[i]).collect();
                // (x-c5ee) Top-K idle cap: attention rows (live, non-idle) always
                // render; idle rows fill to SQUAD_ROW_CAP live rows total; the
                // idle overflow folds into one `+N more` row. Dead rows (present
                // only in Expanded) sit OUTSIDE the budget under the view's
                // control, so `+N more` and the header's `✗N` never double-count.
                let attention = squad_agents
                    .iter()
                    .filter(|&a| !a.exited && !is_idle_row(a))
                    .count();
                let idle_total = squad_agents.iter().filter(|&a| is_idle_row(a)).count();
                let idle_budget = SQUAD_ROW_CAP.saturating_sub(attention);
                let hidden = idle_total.saturating_sub(idle_budget);
                // A squad shows all its idle rows only when the operator toggled
                // its fold open (persisted for the session in `idle_expanded`).
                let show_all_idle = self.idle_expanded.contains(&key);
                let mut idle_shown = 0usize;
                let mut emitted: Vec<&AgentRow> = Vec::with_capacity(squad_agents.len());
                for &a in &squad_agents {
                    if is_idle_row(a) && !show_all_idle {
                        if idle_shown >= idle_budget {
                            continue; // folded into the `+N more` row below
                        }
                        idle_shown += 1;
                    }
                    emitted.push(a);
                }
                // (x-132c) Depth over exactly the emitted set: an idle parent
                // the fold removed is ABSENT, so its visible child roots
                // instead of indenting under a row that never painted.
                let (_, emitted_depths) = lineage_layout(
                    &emitted,
                    |a| a.harness_session_id.as_deref(),
                    |a| a.spawned_by_session.as_deref(),
                );
                for (a, depth) in emitted.iter().zip(emitted_depths) {
                    agent_depth_at.insert(out.len(), depth);
                    out.push(DisplayRow::Agent(a));
                    // (x-6851 US3) Exception-based subline: a Sub row follows the
                    // agent ONLY when its cwd_base differs from the squad's
                    // project basename - the foreign-cwd join worth flagging. A
                    // same-project agent stays one clean row.
                    if agent_is_foreign(a, section_base) {
                        out.push(DisplayRow::Sub(a.cwd_base.clone().unwrap_or_default()));
                    }
                }
                // Emit the fold row whenever there is idle overflow: `+N more`
                // when folded, `- fewer` when shown (so the expansion reverses
                // from the same spot). No row when nothing overflows (no `+0`).
                if hidden > 0 {
                    out.push(DisplayRow::IdleFold {
                        key: key.clone(),
                        hidden,
                        expanded: show_all_idle,
                    });
                }
            }
        }
        // One spacer sets the footer off from the workspace list above so the
        // `+ new workspace` / menu row doesn't read as another workspace. Gated on
        // `multi_squad` like the other US3 spacers (a lone workspace needs no
        // separation, and single-squad layouts stay byte-identical).
        if multi_squad {
            out.push(DisplayRow::Blank);
        }
        // The `+` create-workspace affordance sits directly under the squad list
        // (x-9e5e), above the agents/work-queue sections.
        out.push(DisplayRow::NewSquad);
        // Mission squads are progress indicators, not workspaces (an agent is
        // never assigned a mission id), so they render as one `~ missions` band -
        // the same `~`-prefixed pull-section shape as `~ elsewhere` / `~ backlog` -
        // rather than workspace sections an operator rightly expects to hold
        // sessions. Each mission's name already carries its `done/total` counter.
        // Skip the collect entirely when the band is off (the documented reason
        // for the toggle) - display_rows is hot, called per compose.
        let missions: Vec<&SquadMeta> = if self.show_missions {
            self.layout
                .squads
                .iter()
                .filter(|s| is_mission_squad(s.id))
                .collect()
        } else {
            Vec::new()
        };
        if !missions.is_empty() {
            if multi_squad {
                out.push(DisplayRow::Blank);
            }
            let view = self.section_view(&SectionKey::Missions);
            out.push(DisplayRow::Header {
                label: "~ missions",
                rollup: Vec::new(),
                key: SectionKey::Missions,
                view,
            });
            if view != SectionView::Collapsed {
                // (x-b465) Inert on purpose, and it stays that way. A mission
                // squad is a render-time grouping the server appends to the
                // LAYOUT catalog only (`push_layout`); it is absent from
                // `session.squads`, so `RenameSquad` and `RemoveSquad` both
                // answer `no such squad` for its id. A menu built from those
                // would be entries that all fail, which is worse than none. The
                // hold answers with `no menu on the held row` instead, so the
                // row is inert but never silent.
                for m in missions {
                    out.push(DisplayRow::Sub(m.name.clone()));
                }
            }
        }
        let orphans = self.orphans();
        if !orphans.is_empty() {
            // Orphans (cwd matched no squad) keep one flat section in the same
            // row grammar; the header reads `~ elsewhere` (Locked 6). One spacer
            // precedes the header (x-cd67 US3), not doubled with the group
            // separators above (the `+ new workspace` footer sits between).
            if multi_squad {
                out.push(DisplayRow::Blank);
            }
            let rollup = section_rollup(orphans.iter().map(|&a| agent_lattice_state(a)));
            let view = self.section_view(&SectionKey::Elsewhere);
            out.push(DisplayRow::Header {
                label: "~ elsewhere",
                rollup,
                key: SectionKey::Elsewhere,
                view,
            });
            // Orphans keep their line-1 ` (basename)` suffix (every orphan is
            // foreign by definition); a Sub row would double it, so the
            // `~ elsewhere` section emits no sublines (x-6851 US3).
            if view != SectionView::Collapsed {
                let visible: Vec<&AgentRow> = orphans
                    .iter()
                    .filter(|a| view == SectionView::Expanded || !a.exited)
                    .copied()
                    .collect();
                // (x-132c) Same lineage ordering as the squad sections, over
                // this section's own visible set: an orphan spawned by another
                // orphan nests beneath it. Index-keyed, like the squads.
                let (order, depths) = lineage_layout(
                    &visible,
                    |a| a.harness_session_id.as_deref(),
                    |a| a.spawned_by_session.as_deref(),
                );
                for &i in &order {
                    agent_depth_at.insert(out.len(), depths[i]);
                    out.push(DisplayRow::Agent(visible[i]));
                }
            }
        }
        // The Backlog section (x-6f77, renamed x-1d91): board-ordered
        // ready/blocked/in-flight cards under their own header. Empty
        // (unreadable/no-work graph) renders nothing - the agents section above
        // is unaffected (AC-edge fail-open).
        if !self.layout.backlog.is_empty() && self.show_backlog {
            if multi_squad {
                out.push(DisplayRow::Blank);
            }
            let rollup = section_rollup(
                self.layout
                    .backlog
                    .iter()
                    .map(|c| card_lattice_state(c.state)),
            );
            let view = self.section_view(&SectionKey::WorkQueue);
            out.push(DisplayRow::Header {
                // The cards still render when the graph read is failing - a blank
                // section would be worse - but the header says they are memory
                // rather than fact, so nobody acts on old work believing it fresh.
                label: if self.layout.backlog_stale {
                    "~ backlog · stale"
                } else {
                    "~ backlog"
                },
                rollup,
                key: SectionKey::WorkQueue,
                view,
            });
            // Binary: a card has no exited state, so the queue never enters
            // `LiveOnly` (see [`next_view`]) and only `Collapsed` hides rows.
            if view != SectionView::Collapsed {
                for c in &self.layout.backlog {
                    out.push(DisplayRow::Card(c));
                    // (x-1d91) Line 2: which backlog this row belongs to. Emitted
                    // only when there is something to say - an unscoped, unlaned
                    // card stays one clean row, the same exception-based stance
                    // the agent sublines take.
                    if let Some(attr) = card_attribution(c) {
                        out.push(DisplayRow::Sub(attr));
                    }
                }
                // The reader caps its card set, so the section states the exact
                // remainder rather than implying the backlog ends here.
                let shown = self.layout.backlog.len();
                if self.backlog_total() > shown {
                    out.push(DisplayRow::Sub(format!(
                        "+{} more",
                        self.backlog_total() - shown
                    )));
                }
            }
        }
        // (x-132c) Materialize the aligned depth vec: agent rows carry the
        // depth their emit site recorded (keyed by display index); every other
        // row (headers, sublines, spacers, fold rows) indents nothing.
        let out_depths = (0..out.len())
            .map(|i| agent_depth_at.get(&i).copied().unwrap_or(0))
            .collect();
        (out, out_depths)
    }

    fn draw_sideline(&self, cells: &mut [Cell], rows: usize, cols: usize, panel_w: usize) {
        let text_w = panel_w - 1; // last column is the divider
        let off = self.sideline_offset;
        // (x-b186) Read the clock ONCE per paint, not per row: every extended
        // row's age is relative to the same instant, so a mid-paint tick cannot
        // make one row read older than the row above it.
        let now = crate::digest_overlay::now_secs();
        let table_layout = TableLayout::fitting(text_w as u16);
        // Composition width for the top row: text_w minus the density button.
        let btn_reserved = match self.density_button_range(panel_w) {
            Some(r) => r.start,
            None => text_w,
        };
        // `i` stays the TRUE display index (so the selector/hover highlight and
        // hit-test still match); the painted row subtracts the scroll offset.
        // (x-132c) The depths come from the SAME compose pass as the rows, so
        // an agent row indents by the depth computed over exactly the set that
        // paints - never a re-derivation over a different visibility set.
        let (display, row_depths) = self.display_rows_with_depths();
        // (x-aeab) The reservation is the court block's rendered line count.
        let (block_rows, block_lines) = self.court_block_layout(rows);
        let list_rows = rows - block_rows;
        for (i, drow) in display.into_iter().enumerate().skip(off) {
            // (x-cd67 US1) The sideline owns the full column height including
            // row 0; the tab strip moved right of the divider. Display row `i`
            // paints at outer row `i - off` (was `TAB_BAR_ROWS + (i - off)`).
            let r = i - off;
            if r >= list_rows {
                break;
            }
            // (x-b186) The density button is pinned to the top painted row, so
            // that row COMPOSES its text into a narrower width. Reserving beats
            // overlaying: painting the button over a finished header band ate
            // the always-on rollup counts x-6851 exists to keep visible. The
            // band still FILLS the full width below - only the text yields.
            let text_w = if r == 0 { btn_reserved } else { text_w };
            let is_inert = row_is_inert(&drow);
            // x-5a52: the active-squad header accents its caret (always on,
            // independent of the selector/hover).
            let mark_caret = matches!(
                &drow,
                DisplayRow::Sel(row)
                    if row.tab.is_none() && row.squad == self.layout.active_squad
            );
            // (x-4374) The focused pane's owning row is the sole standing
            // full-width INVERSE band, replacing the near-invisible one-cell
            // gutter x-5a52 painted: the band IS the "you are here" signal now. At
            // most one row matches focus, and a focused shell pane or a focused row
            // inside a folded section matches no painted row - so zero bands,
            // never a stale one on a previously-focused row.
            let is_focus =
                matches!(&drow, DisplayRow::Agent(a) if a.pane_id == Some(self.layout.focus));
            // (x-f331) An EXITED focused row is legibly dead: it drops the bright
            // band for a DIM accent so a dead "you are here" never reads as a live
            // one (the screenshot case - a focus band on an EXITED row was
            // indistinguishable from a selector).
            let focus_exited = matches!(
                &drow,
                DisplayRow::Agent(a) if a.pane_id == Some(self.layout.focus) && a.exited
            );
            // A full-width band fills the panel edge-to-edge: the focused row (its
            // standing band) plus every header (so a SELECTED header inverts
            // edge-to-edge - headers are otherwise demoted to plain/BOLD by
            // `header_band_flags` and carry zero standing INVERSE cells).
            let is_band =
                is_focus || matches!(&drow, DisplayRow::Sel(_) | DisplayRow::Header { .. });
            // (x-f191) Read the row stamp before `drow` moves into the match.
            let row_stamp = self.row_stamp_for(&drow);
            // (x-df4c) The row tuple carries `fg` now: most rows are
            // `Color::Default`, but a needs-attention (Blocked) agent row or card
            // paints the accent, so the color must reach the cells below.
            let (text, mut flags, mut fg) = match drow {
                DisplayRow::Sel(row) => {
                    let squad = self.layout.squads.iter().find(|s| s.id == row.squad);
                    let Some(squad) = squad else { continue };
                    let is_active_squad = squad.id == self.layout.active_squad;
                    let (text, flags, fg) = match row.tab {
                        None => {
                            let caret = view_caret(self.section_view(&section_key(squad)));
                            // `*` after the caret marks the active squad so
                            // activity survives weak-BOLD themes and manual
                            // collapse (x-2f99); replaces the space, so row
                            // width is unchanged. Same vocabulary as the
                            // active-tab marker below.
                            let mark = if is_active_squad { '*' } else { ' ' };
                            let label = format!("{caret}{mark}{}", squad.name);
                            // (x-6851 US2, demoted x-4374) The squad name row
                            // carries always-on per-state rollup counts folded
                            // from THIS squad's live rows every paint (never
                            // cached - the x-df4c drift posture), right-aligned
                            // across the full width. The reverse-video band came
                            // off in x-4374 (active BOLD / inactive plain via
                            // `header_band_flags`); the counts still read in every
                            // view state, so a blocked pane shows `▲N` whether the
                            // squad is folded or open.
                            let rollup = section_rollup(
                                self.layout
                                    .agents
                                    .iter()
                                    .filter(|a| a.squad == Some(squad.id))
                                    .map(agent_lattice_state),
                            );
                            let text = header_band_text(&label, &rollup, text_w);
                            (text, header_band_flags(is_active_squad), Color::Default)
                        }
                        Some(t) => {
                            let marker = if is_active_squad && t == squad.active_tab {
                                '*'
                            } else {
                                ' '
                            };
                            // The same digit-collapse as the tab bar: a
                            // no-signal tab renders its bare ordinal (x-c150).
                            let label = match squad.tabs.get(t) {
                                Some(tm) => tab_label_text(&tm.name, t, tm.named),
                                None => (t + 1).to_string(),
                            };
                            (format!("  {marker}{label}"), 0, Color::Default)
                        }
                    };
                    (text, flags, fg)
                }
                // (x-b186) In Extended an agent row IS a table row: same lattice
                // style and external DIM modifier, different text composition.
                DisplayRow::Agent(a) if self.density == Density::Extended => {
                    let layout =
                        table_layout.expect("extended density has an admitted table layout");
                    let depth = row_depths.get(i).copied().unwrap_or(0);
                    let st = agent_lattice_state(a);
                    let style = lattice_style(st, self.theme.accent);
                    let mut flags = style.flags;
                    if a.external && st != LatticeState::Blocked {
                        flags |= cell_flags::DIM;
                    }
                    // (x-1b35) Same lane-color cascade as the compact arm; the
                    // Blocked accent wins there and here.
                    (
                        table_row_text(a, layout, depth, now),
                        flags,
                        agent_lane_fg(a, st, style.fg),
                    )
                }
                DisplayRow::Agent(a) => {
                    // The unified icon lattice (x-df4c): exit beats badge beats
                    // liveness (row precedence, unchanged), mapped onto the one
                    // state->style mapping. Idle is now the outline `○`, not the
                    // near-invisible `·` this node exists to kill.
                    let st = agent_lattice_state(a);
                    let style = lattice_style(st, self.theme.accent);
                    let glyph = style.glyph;
                    // A recruit mark (x-8f11) replaces the leading space with a
                    // `*`, keeping the row width unchanged (same vocabulary as
                    // the active-squad/tab marker).
                    let mark = if a
                        .attach_id
                        .as_deref()
                        .is_some_and(|id| self.marks.contains(id))
                    {
                        '*'
                    } else {
                        ' '
                    };
                    // (x-1b35) The `@<account>` text prefix is retired from the
                    // row: the account is an incidental stand-in for the LANE,
                    // and the lane now renders as zero-width color (x-c914's
                    // account glyph logic lives in the peek header, which keeps
                    // its own account surfacing).
                    let dnd = if a.dnd { " [DND]" } else { "" };
                    let mut text = format!(" {mark}{glyph}{dnd} {}", a.name);
                    // (x-1b35) The model-deviation token: a dim short prefix on
                    // rows OFF their harness's default lane (claude on glm
                    // renders ` glm`; claude on opus renders nothing). The
                    // textual channel for the lane color - accessibility and
                    // grep-ability in one.
                    if let Some(tok) =
                        sideline_color::deviation_token(a.harness.as_deref(), a.model.as_deref())
                    {
                        text.push_str(&format!(" {tok}"));
                    }
                    // (x-8f9d) The portal index, when this row is shown
                    // through one. The server derives it per frame from the
                    // open portals, so a row moving between portals stays ONE
                    // row whose marker changes - never a second row. Absent
                    // means no portal, never an unknown one.
                    if let Some(idx) = a.portal {
                        text.push_str(&format!(" ◫{idx}"));
                    }
                    // (x-132c) Indent the row under its lineage parent: one
                    // step per depth, read from the compose-pass depth vec.
                    // Zero steps -> no prefix -> a section with no parent
                    // edges stays byte-identical.
                    let steps = row_depths.get(i).copied().unwrap_or(0);
                    if steps > 0 {
                        text = format!("{}{text}", "  ".repeat(steps));
                    }
                    // (x-0090, x-0f9d US3) A pane row names its hosting tab
                    // inside-out: a NAMED tab shows its name (`·reviews`), an
                    // unnamed tab shows the `·N` ordinal. An orphan row (no tab)
                    // instead names its repo with a ` (basename)` suffix. Tab vs
                    // orphan are mutually exclusive, so at most one suffix lands.
                    match self.agent_tab_context(a.squad, a.tab) {
                        // (x-4374) The badge means "this session lives on a tab
                        // you are not looking at": suppress it when the row's pane
                        // is in the viewer's active (squad, tab). A row in a
                        // background tab or another squad keeps it, so quietness is
                        // the "you are here" signal.
                        Some(_)
                            if a.squad == Some(self.layout.active_squad)
                                && a.tab == self.active_squad_active_tab_id() => {}
                        Some(TabContext::Named(name)) => text.push_str(&format!(" ·{name}")),
                        Some(TabContext::Ordinal(ord)) => text.push_str(&format!(" ·{ord}")),
                        None => {
                            // (x-0090) An ORPHAN (no squad) names its repo with a
                            // ` (basename)` line-1 suffix so two same-named
                            // workers in different repos are distinguishable. A
                            // squad-matched paneless row is NOT an orphan - now
                            // that every row carries `cwd_base` (x-6851 US3), the
                            // `squad.is_none()` guard keeps the suffix orphan-only;
                            // a matched row's foreign cwd surfaces as the exception
                            // subline instead.
                            if a.squad.is_none() {
                                if let Some(base) = a.cwd_base.as_deref() {
                                    text.push_str(&format!(" ({base})"));
                                }
                            }
                        }
                    }
                    if let Some(reason) = a.reason.as_deref().filter(|x| !x.is_empty()) {
                        text.push_str(": ");
                        text.push_str(reason);
                    }
                    // (US9 crown) The inline coordinator badge, mesh vocabulary
                    // `L{level} {scope}` (scope `?` when a partial crown carries
                    // none). Absent on an un-crowned row, so no byte changes there.
                    if let Some(level) = a.crown_level {
                        let scope = a.crown_scope.as_deref().unwrap_or("?");
                        text.push_str(&format!(" [L{level} {scope}]"));
                    }
                    // External (roster-surfaced) is a MODIFIER, not a state
                    // (x-df4c AC1-UI): the row keeps its lattice style and ORs
                    // DIM on top - EXCEPT on Blocked, where the accent wins and
                    // DIM is withheld (attention must never be dimmed). Exit's
                    // DIM already rides `style.flags`.
                    let mut flags = style.flags;
                    if a.external && st != LatticeState::Blocked {
                        flags |= cell_flags::DIM;
                    }
                    // (x-1b35) The lane color: zero width, keyed on the ROUTE
                    // through the fixed cascade (routing row > model > route >
                    // harness > built-in). A Blocked row keeps the lattice
                    // accent - attention is never re-colored.
                    (text, flags, agent_lane_fg(a, st, style.fg))
                }
                DisplayRow::Card(c) => {
                    // The same icon lattice as the agent rows (x-df4c US3): a
                    // Ready card IS the hollow waiting state, InFlight IS the
                    // filled running state, so the card vocabulary and the agent
                    // lattice are literally one mapping. Blocked now carries the
                    // accent instead of the old bare DIM (attention, not muted).
                    let style = lattice_style(card_lattice_state(c.state), self.theme.accent);
                    let glyph = style.glyph;
                    let label = if c.slug.is_empty() { &c.id } else { &c.slug };
                    // (x-1d91) The head of the queue is stated, not inferred from
                    // position: the section can be scrolled or the top card
                    // claimed, and either would make "first row" a lie. Labelled
                    // `head` rather than `next` on purpose - it names the board's
                    // head, and the dispatcher's actual pick can differ (see
                    // BacklogCard::head). A dispatched-but-unconfirmed verb shows
                    // `…` instead, so no reorder is ever invisible.
                    let mark = if self.card_pending(&c.id) {
                        " …"
                    } else if c.head {
                        " head"
                    } else {
                        ""
                    };
                    (
                        format!("  {glyph} {label} {}{mark}", c.priority),
                        style.flags,
                        style.fg,
                    )
                }
                DisplayRow::Header {
                    label,
                    rollup,
                    view,
                    ..
                } => (
                    // (x-975a) The caret leads a `~` header exactly as it leads
                    // a squad row, so both read as the same cycleable control.
                    header_band_text(&format!("{}{label}", view_caret(view)), &rollup, text_w),
                    // A section header is never the active squad, so it is the
                    // inactive (plain) header - one grammar with the demoted squad
                    // rows above (x-4374).
                    header_band_flags(false),
                    Color::Default,
                ),
                DisplayRow::NewSquad => {
                    // The recruit-mark footer count rides the create affordance
                    // (x-8f11): `space` marks, `R` recruits the marked set.
                    let base = if self.marks.is_empty() {
                        FOOTER_NEW_LABEL.to_string()
                    } else {
                        format!("{FOOTER_NEW_LABEL}   {} marked ·R", self.marks.len())
                    };
                    // x-8ccf US4: the `☰ menu` button rides the footer's right edge
                    // when the panel is wide enough (footer_menu_range gates it);
                    // the same range routes a click there to the MENU popup.
                    let label = match self.footer_menu_range(panel_w) {
                        Some(range) => format!("{}{FOOTER_MENU}", pad_to(&base, range.start)),
                        None => base,
                    };
                    // DIM is this panel's inert marker; the one actionable row
                    // must not share it.
                    (label, cell_flags::BOLD, Color::Default)
                }
                DisplayRow::Sub(sub) => {
                    // Indented 4 cells to sit under the row's name (` {mark}{glyph} `
                    // is 4 cells wide). The painter truncates to the panel width,
                    // so a long attribution ellipses rather than wrapping.
                    (format!("    {sub}"), cell_flags::DIM, Color::Default)
                }
                // (x-cd67 US3) A blank section spacer paints nothing.
                DisplayRow::Blank => (String::new(), 0, Color::Default),
                // (x-b186) The extended table's column header: DIM like the
                // other inert labels, so it reads as chrome rather than a row.
                DisplayRow::TableHead => (
                    table_head_text(
                        table_layout.expect("extended density has an admitted table layout"),
                        self.agent_sort,
                    ),
                    cell_flags::DIM,
                    Color::Default,
                ),
                DisplayRow::TableEmpty => {
                    ("  no agents".to_string(), cell_flags::DIM, Color::Default)
                }
                // (x-c5ee) The idle fold: `+N more` folded, `- fewer` expanded.
                // Indented 4 cells to sit under the agent rows like a `Sub`, and
                // DIM as a quiet summary - but it is NOT inert, so the selector's
                // INVERSE bar still lifts it when the cursor lands on it.
                DisplayRow::IdleFold {
                    hidden, expanded, ..
                } => {
                    // (x-d401) `+N more`, not `+N idle`. The fold now covers
                    // `Unmeasured` rows as well as `Idle` and `Empty` ones, and
                    // a row with NO reading counted under the word "idle" is
                    // this branch's own defect: a label asserting a measurement
                    // nothing took. `more` states only what is true of every
                    // folded row - that it is hidden - and pairs with the
                    // `- fewer` the expanded form already prints.
                    let label = if expanded {
                        "    - fewer".to_string()
                    } else {
                        format!("    +{hidden} more")
                    };
                    (label, cell_flags::DIM, Color::Default)
                }
            };
            // (x-4374) The focused row wears the band via INVERSE in its base
            // flags, set BEFORE the selector/hover XOR below so the two compose:
            // a parked selector leaves the row as the sole standing band; a
            // selector ON the focused row XOR-de-inverts it under the cursor, the
            // same grammar the old header bands used, so the selection still reads.
            // (x-f331) Three distinct treatments so "you are here" reads apart
            // from the selector's "about to act here": the focus band wears the
            // ACCENT colour (accent fg -> accent bg under INVERSE) vs the
            // selector's plain-INVERSE bar. An EXITED focus row is DIM accent, no
            // bright band. The accent survives weak-BOLD themes (it is a colour,
            // not a weight), which the highlight-distinctness AC requires.
            if is_focus {
                if focus_exited {
                    flags |= cell_flags::DIM;
                } else {
                    flags |= cell_flags::INVERSE;
                }
                fg = self.theme.accent;
            }
            // The selector cursor OR the mouse hover paints the INVERSE bar
            // (x-a496); both are display indices now (x-260a), so the bar can
            // never drift from the painted row. Hover is highlight-only, and
            // neither bar lands on an inert Header (the cursor skips them; the
            // hover check here keeps a label from reading as actionable -
            // gemini review).
            let highlit = !is_inert && (self.selector == Some(i) || self.hover_row == Some(i));
            // Selection/hover TOGGLES the INVERSE bit: an agent row (no INVERSE)
            // gains the cursor bar exactly as before, while a header band (which
            // already carries INVERSE) de-inverts under the cursor so the
            // selection still reads instead of vanishing into the band (x-6851
            // US1; the you-are-here highlight proper lands in x-5a52).
            if highlit {
                flags ^= cell_flags::INVERSE;
            }
            // Advance by DISPLAY columns, not char index: a double-width glyph
            // (the menu trigram) claims two columns and marks its right half a
            // WIDE_SPACER so the compositor keeps the row in sync instead of
            // shoving the divider (and every cell after it) past the panel.
            let mut col = 0usize;
            for ch in text.chars() {
                let w = glyph_cols(ch);
                if col + w > text_w {
                    break;
                }
                cells[r * cols + col] = Cell {
                    c: ch,
                    fg,
                    bg: Color::Default,
                    flags,
                };
                if w == 2 {
                    cells[r * cols + col + 1] = Cell {
                        c: ' ',
                        fg: Color::Default,
                        bg: Color::Default,
                        flags: flags | cell_flags::WIDE_SPACER,
                    };
                }
                col += w;
            }
            // Fill the row remainder so a band spans the full panel width and a
            // (non-band) highlight reads as a bar. A band pads with its own
            // final flags (INVERSE band, de-inverted under the cursor); a plain
            // highlight pads INVERSE only, the legacy cursor-bar look.
            if is_band {
                for j in col..text_w {
                    cells[r * cols + j].flags = flags;
                    // (x-f331) Carry the accent across the focus band's padding so
                    // the whole band is one colour, not accent-under-text +
                    // default-under-pad.
                    if is_focus {
                        cells[r * cols + j].fg = fg;
                    }
                }
            } else if highlit {
                for j in col..text_w {
                    cells[r * cols + j].flags |= cell_flags::INVERSE;
                }
            }
            // x-5a52 (US4): the active-squad caret rides column 0 in the accent,
            // recolored in place so the header's flags (BOLD, or INVERSE when
            // selected) survive and the caret glyph stays. The focused row's
            // signal is the band above, not a gutter, so nothing is painted here
            // for it (x-4374).
            if mark_caret && text_w >= 1 {
                cells[r * cols].fg = self.theme.accent;
            }
            // (x-f191) A row-scoped outcome stamp renders AT the row the
            // operator acted on; the paint lives in row_stamp.
            paint_row_stamp(cells, r, cols, text_w, row_stamp);
        }
        // (x-b186) The density button, painted LAST over the sideline's top row.
        // Overlaying is what keeps it pinned to row 0 while the rows beneath it
        // scroll, and it costs no display row - so the x-260a invariant (every
        // painted line is exactly one display row) still holds and
        // `sideline_row_at` needs no special case. The header band underneath
        // already right-aligns a droppable rollup strip, so the two columns this
        // takes cost at worst the least-severe rollup pair, never the label.
        //
        // (x-2e86) Layout is [inverse glyph][plain pad]: the glyph leads and the
        // divider-adjacent cell is a NON-inverse space, so the button reads one
        // column in from the border (the operator's padding ask) without shifting
        // `range.start` - the header band keeps every column it had, so a tight
        // slim rail never loses its rollup to the pad (AC1-HP).
        if rows > 0 {
            if let Some(range) = self.density_button_range(panel_w) {
                let glyph = density_glyph(self.density);
                let start = range.start;
                for (n, c) in range.clone().enumerate() {
                    let is_pad = n + 1 == DENSITY_BTN_W; // the trailing cell is the pad
                    cells[c] = Cell {
                        c: if c == start { glyph } else { ' ' },
                        fg: Color::Default,
                        bg: Color::Default,
                        flags: if is_pad { 0 } else { cell_flags::INVERSE },
                    };
                }
            }
        }
        // (x-aeab) The court block: three glance lines minimized, the full
        // reading expanded, pinned to the bottom rows of the column. The row
        // list already stopped above it; the block renders DIM so it reads as
        // chrome beside the live rows, and the painter truncates to the panel
        // width - the same rule every sideline row follows.
        court_block::paint_court_block(cells, block_lines, list_rows, rows, cols, text_w);
        // The divider column, now full terminal height (the sideline owns row
        // 0 too; the strip sits right of the divider) - x-cd67 US1.
        //
        // x-9e33: accent it while hovered or dragged, the same signal a pane
        // seam wears (x-d807 shipped the drag but never rendered this, leaving
        // the border a draggable-but-invisible 1-cell target). A terminal
        // cannot change the cursor shape, so this accent IS the affordance.
        let border_active = self.hover_sideline_border || self.sideline_drag.is_some();
        let (border_fg, border_flags) = if border_active {
            (self.theme.accent, cell_flags::BOLD)
        } else {
            (Color::Default, cell_flags::DIM)
        };
        for r in 0..rows {
            cells[r * cols + (panel_w - 1)] = Cell {
                c: '│',
                fg: border_fg,
                bg: Color::Default,
                flags: border_flags,
            };
        }
    }
}

/// One rendered sideline line. The actionable variants (`Sel`, `Agent`, `Card`,
/// `NewSquad`) resolve through [`View::row_action`] via the selector's Enter or a
/// mouse click (x-260a); the inert variants (`Header`, `Sub`, `Blank`) are
/// skipped by the selector, never hover-highlighted, and return `None` from
/// `row_action` - see [`row_is_inert`].
enum DisplayRow<'a> {
    Sel(SelRow),
    Agent(&'a AgentRow),
    /// A work-queue backlog card (x-6f77); a Ready card dispatches via the
    /// confirm (x-a496), by click or selector Enter (x-260a).
    Card(&'a BacklogCard),
    /// (x-6851 US1+US2) A section header: a full-width INVERSE band with a
    /// right-aligned per-state rollup strip. `rollup` is folded at
    /// `display_rows` time from the section's own rows (orphans / cards), so the
    /// painter renders it without re-deriving section membership.
    Header {
        label: &'static str,
        rollup: Vec<(LatticeState, usize)>,
        /// (x-975a) Which section this header owns, so a click cycles it
        /// without the action path re-deriving the section from `label`.
        key: SectionKey,
        /// The section's view state at fold time, for the caret glyph.
        view: SectionView,
    },
    /// The `+` create-workspace affordance (x-9e5e), a footer under the squad
    /// list. A click opens the name-input overlay.
    NewSquad,
    /// (x-cd67 US2) The dim, 4-cell-indented line-2 under a row: an agent's
    /// foreign `cwd_base`, or (x-1d91) a Backlog card's `project · lane`
    /// attribution and the section's `+N more` remainder. Owns its text so any
    /// section can emit one without the painter learning a new row type. Inert:
    /// every painted line stays one display row (the x-260a single-enumeration
    /// invariant), so scroll, hover, and hit-test index math are untouched.
    Sub(String),
    /// (x-cd67 US3) A one-line spacer between workspace groups and before the
    /// trailing sections. Inert, like `Sub`.
    Blank,
    /// (x-b186) The extended table's column-header line, carrying the current
    /// sort label so a toggle is never invisible - even when the two orders
    /// happen to coincide (one agent, or all rows in one band), the label
    /// changes. Inert like `Sub`: one painted line, one display row, so the
    /// x-260a hit-test math is untouched.
    TableHead,
    /// (x-b186) The extended table's zero-agent line. Inert, like `TableHead`:
    /// a header with nothing under it reads as a stalled table, so the empty
    /// state is stated rather than implied.
    TableEmpty,
    /// (x-c5ee) The top-K idle fold: a squad with more live idle rows than the
    /// cap folds the overflow behind this one row. `hidden` is the foldable
    /// count (the idle rows past the cap, always > 0 when this row is emitted).
    /// Unlike the inert rows above it is ACTIONABLE - a click / selector Enter
    /// toggles the squad's idle expansion (the idle sibling of a header's
    /// `CycleSection`), so it is NOT in `row_is_inert` and the cursor can rest
    /// on it. Folded it paints `+N more`; expanded it paints `- fewer`.
    IdleFold {
        key: SectionKey,
        hidden: usize,
        expanded: bool,
    },
}

/// (x-1d91) A dispatched Backlog reorder verb awaiting confirmation from the feed.
///
/// There is no optimistic reorder: the rendered order changes only when the graph
/// reader republishes, so between dispatch and that republish the card wears a `…`
/// marker. The card set AT DISPATCH is the confirm signal - layouts push on every
/// scrape tick, so "any layout arrived" would clear the marker instantly and prove
/// nothing. `deadline` bounds the wait: a verb whose effect never lands (it failed
/// silently, or was a server-side no-op like floating an already-top card) must
/// clear with a visible notice rather than leave the row spinning forever.
struct BacklogPending {
    node: String,
    verb: BacklogVerb,
    /// What the target card looked like at dispatch; a different mark means the
    /// feed confirmed THIS verb (see [`card_mark`]).
    was: Option<(usize, CardState, Option<String>)>,
    deadline: Instant,
}

/// How long a dispatched reorder verb may sit unconfirmed before the marker
/// clears with a notice. The graph reader ticks about once a second, so this is
/// many refreshes' worth of grace - long enough that a slow verb is not called
/// lost, short enough that a stuck row is never mistaken for a live one.
const BACKLOG_PENDING_TTL: Duration = Duration::from_secs(10);

/// (x-1d91) What a Backlog card looks like for confirmation purposes: its
/// position, state, and lane, or `None` when it is not in the feed at all.
///
/// Position covers a float (the card moves), lane covers a cross-column move,
/// state covers a claim, and absence covers a defer (which takes the node off
/// the board). Everything a v1 verb can do shows up here, and nothing another
/// card's churn can do does.
fn card_mark(cards: &[BacklogCard], node: &str) -> Option<(usize, CardState, Option<String>)> {
    cards
        .iter()
        .position(|c| c.id == node)
        .map(|i| (i, cards[i].state, cards[i].lane.clone()))
}

/// (x-1d91) A Backlog card's `project · lane` attribution subline, or `None` when
/// the card carries neither (an unscoped, unlaned node says nothing worth a
/// second row). Either half alone renders alone - the separator only appears
/// between two present values.
fn card_attribution(c: &BacklogCard) -> Option<String> {
    match (c.project.as_deref(), c.lane.as_deref()) {
        (Some(p), Some(l)) => Some(format!("{p} · {l}")),
        (Some(p), None) => Some(p.to_string()),
        (None, Some(l)) => Some(l.to_string()),
        (None, None) => None,
    }
}

/// (x-cd67) True for a non-actionable sideline row: the selector skips it, it is
/// never hover/selection-highlighted, and [`View::row_action`] returns `None`.
/// Header (a section label), Sub (an agent's dim subline), and Blank (a section
/// spacer). One predicate so paint, hit-test, and the selector never diverge.
/// Display columns a sideline glyph occupies. The client draws chrome one glyph
/// per cell, so a double-width glyph must claim two columns (plus a WIDE_SPACER)
/// or it desyncs the rest of the row against a standards-compliant terminal.
/// ponytail: only the menu trigram block (U+2630..U+2637) is wide in the
/// sideline today; widen this if a CJK/emoji glyph ever lands here.
fn glyph_cols(ch: char) -> usize {
    if ('\u{2630}'..='\u{2637}').contains(&ch) {
        2
    } else {
        1
    }
}

/// A squad's [`SectionKey`]. Deliberately NOT keyed on `name`: a mission
/// header's name carries its live `done/total` counters and a derived squad
/// label is rewritten the moment a sibling collides, so either would orphan
/// the operator's choice on an unrelated event. The synthetic mission id and
/// the canonical repo root are the stable identities. A squad with neither
/// (no cwd, not a mission) falls back to its name - degenerate, and better
/// than dropping its state entirely.
fn section_key(s: &SquadMeta) -> SectionKey {
    if is_mission_squad(s.id) {
        SectionKey::Mission(s.id)
    } else if !s.canonical_cwd.is_empty() {
        SectionKey::Squad(s.canonical_cwd.clone())
    } else {
        SectionKey::Squad(s.name.clone())
    }
}

/// The [`SectionKey`] for a squad id against a given layout. `None` for an id
/// the layout does not carry - the caller then has no section to act on, which
/// is the correct no-op rather than minting a key for a dead squad.
fn squad_key(layout: &LayoutView, id: u64) -> Option<SectionKey> {
    layout.squads.iter().find(|s| s.id == id).map(section_key)
}

/// Whether `s` is the squad `key` names. The allocation-free twin of
/// [`section_key`] - the prune below runs it per squad on every scrape tick,
/// where building a throwaway key would clone a `String` each time.
/// `section_key_matches_resolver` pins the two to the same answer.
fn squad_matches(s: &SquadMeta, key: &SectionKey) -> bool {
    match key {
        SectionKey::Mission(id) => is_mission_squad(s.id) && s.id == *id,
        SectionKey::Squad(_) if is_mission_squad(s.id) => false,
        SectionKey::Squad(ident) if !s.canonical_cwd.is_empty() => &s.canonical_cwd == ident,
        SectionKey::Squad(ident) => &s.name == ident,
        SectionKey::Elsewhere | SectionKey::WorkQueue | SectionKey::Missions => false,
    }
}

/// Whether `layout` still carries the section `key` names. The prune predicate,
/// shared by every layout push so painting and pruning can never disagree about
/// what counts as a live section.
fn section_is_live(layout: &LayoutView, key: &SectionKey) -> bool {
    match key {
        SectionKey::Squad(_) | SectionKey::Mission(_) => {
            layout.squads.iter().any(|s| squad_matches(s, key))
        }
        // The `~ missions` band is live while any mission squad exists; the two
        // pull-sections are always considered live (their rows come and go).
        SectionKey::Missions => layout.squads.iter().any(|s| is_mission_squad(s.id)),
        SectionKey::Elsewhere | SectionKey::WorkQueue => true,
    }
}

/// The caret glyph per view state. `LiveOnly` is the HOLLOW triangle against
/// `Expanded`'s filled one - the same hollow/filled discriminator the icon
/// lattice already uses for `○` idle vs `●` working, so the middle state is
/// legible without a new indicator element. The header's `✗N` rollup says HOW
/// MANY rows are hidden - but it is the first pair `header_band_text` drops on
/// a narrow panel, so the caret, not the count, is what always distinguishes
/// the state.
fn view_caret(v: SectionView) -> char {
    match v {
        SectionView::Expanded => '▾',
        SectionView::LiveOnly => '▿',
        SectionView::Collapsed => '▸',
    }
}

/// (x-f331) The action-verb keys a HOVER-ARMED selector captures on the
/// pointed-at row: remove/stop (`x`), bulk reap (`X`), rename (`r`), peek
/// (space), recruit-mark (tab). Everything else - navigation, and any typing -
/// disarms the hover-arm and forwards to the focused pane, so a parked pointer
/// never swallows shell input (AC2-EDGE). Enter is deliberately absent: a lone
/// Enter is far likelier to be shell input than an attach gesture. Verbs that
/// mutate are already confirm-gated at the row, so a stray leading verb at most
/// opens a dismissable prompt.
fn is_sideline_verb(b: u8) -> bool {
    matches!(b, b'x' | b'X' | b'r' | b' ' | b'\t')
}

fn row_is_inert(drow: &DisplayRow) -> bool {
    matches!(
        drow,
        DisplayRow::Header { .. }
            | DisplayRow::Sub(_)
            | DisplayRow::Blank
            | DisplayRow::TableHead
            | DisplayRow::TableEmpty
    )
}

#[allow(clippy::type_complexity)]
fn append_sorted_agent_group<'a>(
    out: &mut Vec<(DisplayRow<'a>, usize)>,
    group: &mut Vec<(Vec<(DisplayRow<'a>, usize)>, &'a AgentRow)>,
    sort: AgentSort,
    needs: &HashMap<String, NeedKind>,
    now_secs: u64,
) {
    let mut subtrees: Vec<(
        Vec<(Vec<(DisplayRow<'a>, usize)>, &'a AgentRow)>,
        &'a AgentRow,
    )> = Vec::new();
    for item in group.drain(..) {
        let depth = item.0.first().map(|(_, depth)| *depth).unwrap_or_default();
        if depth == 0 || subtrees.is_empty() {
            let root = item.1;
            subtrees.push((vec![item], root));
        } else {
            subtrees.last_mut().unwrap().0.push(item);
        }
    }
    subtrees.sort_by(|(_, a), (_, b)| {
        compare_agent_rows(
            a,
            b,
            sort,
            needs.get(a.name.as_str()).copied(),
            needs.get(b.name.as_str()).copied(),
            now_secs,
        )
    });
    for (items, _) in subtrees {
        for (rows, _) in items {
            out.extend(rows);
        }
    }
}

fn compare_agent_rows(
    a: &AgentRow,
    b: &AgentRow,
    sort: AgentSort,
    need_a: Option<NeedKind>,
    need_b: Option<NeedKind>,
    now_secs: u64,
) -> Ordering {
    let order = match sort.column {
        AgentSortColumn::Status => {
            let a_key = attention_key(a, need_a);
            let b_key = attention_key(b, need_b);
            let a_state = if a.exited {
                u8::MAX
            } else {
                pane_state(a.badge, a.seen, a.pane_activity) as u8
            };
            let b_state = if b.exited {
                u8::MAX
            } else {
                pane_state(b.badge, b.seen, b.pane_activity) as u8
            };
            apply_direction(
                a_state
                    .cmp(&b_state)
                    .then_with(|| a_key.0.cmp(&b_key.0))
                    .then_with(|| a_key.1.cmp(&b_key.1))
                    .then_with(|| a_key.2.cmp(&b_key.2)),
                sort.direction,
            )
        }
        AgentSortColumn::Agent => apply_direction(a.name.cmp(&b.name), sort.direction),
        AgentSortColumn::LastMessage => cmp_optional(
            a.tail.as_deref().filter(|value| !value.is_empty()),
            b.tail.as_deref().filter(|value| !value.is_empty()),
            sort.direction,
        ),
        AgentSortColumn::Pr => cmp_optional(a.pr, b.pr, sort.direction),
        AgentSortColumn::Age => {
            cmp_optional(row_age(a, now_secs), row_age(b, now_secs), sort.direction)
        }
    };
    order
}

fn cmp_optional<T: Ord>(a: Option<T>, b: Option<T>, direction: SortDirection) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => apply_direction(a.cmp(&b), direction),
    }
}

fn apply_direction(order: Ordering, direction: SortDirection) -> Ordering {
    match direction {
        SortDirection::Ascending => order,
        SortDirection::Descending => order.reverse(),
    }
}

fn row_age(a: &AgentRow, now_secs: u64) -> Option<u64> {
    a.last_activity_age_s
        .or_else(|| a.updated_at.map(|updated| now_secs.saturating_sub(updated)))
}

/// (x-6851 US3) The project basename a section is keyed by (the squad's
/// canonical repo root), for the foreign-cwd subline comparison. `None` for a
/// squad whose canonical cwd has no final component (degenerate; no subline).
fn section_project_base(canonical_cwd: &str) -> Option<&str> {
    Path::new(canonical_cwd)
        .file_name()
        .and_then(|b| b.to_str())
}

/// (x-6851 US3) Whether an agent's cwd is FOREIGN to its section: its `cwd_base`
/// is present AND differs from the section's project basename. A missing
/// `cwd_base` (absent on the wire, AC4-EDGE) or a match yields false - no
/// subline. This is the exception predicate that replaced x-cd67's
/// always-on server subline.
fn agent_is_foreign(a: &AgentRow, section_base: Option<&str>) -> bool {
    match (a.cwd_base.as_deref(), section_base) {
        (Some(cwd), Some(base)) => cwd != base,
        _ => false,
    }
}

/// Blank whichever half of a double-width pair survives at the edges of the
/// half-open column range `[start, end)` on `row`.
///
/// Chrome cells are one column wide, but they get stamped over arbitrary program
/// output that may hold a DOUBLE-width glyph. `Compositor::draw_row` SKIPS any
/// `WIDE_SPACER` cell, so overwriting one half of such a pair leaves either a
/// lead with no spacer or a spacer with no lead, and the row then emits the
/// wrong number of columns - shifting or wrapping everything after it. Only the
/// edges can strand a half; the interior is fully overwritten.
///
/// Call this before stamping, from every overlay that writes a sub-range of a
/// row. An overlay that clears whole rows does not need it.
fn blank_straddling_pair(cells: &mut [Cell], cols: usize, row: usize, start: usize, end: usize) {
    let blank = Cell {
        c: ' ',
        fg: Color::Default,
        bg: Color::Default,
        flags: 0,
    };
    if start > 0 && start < cols && cells[row * cols + start].flags & cell_flags::WIDE_SPACER != 0 {
        cells[row * cols + start - 1] = blank;
    }
    if end < cols && cells[row * cols + end].flags & cell_flags::WIDE_SPACER != 0 {
        cells[row * cols + end] = blank;
    }
}

/// Shrink `spans` until they fit `width`.
///
/// Columns come off in order of what the operator loses by it:
///
/// 1. the widest tab label, down to one character plus its brackets;
/// 2. then the squad label, the only span nothing navigates by;
/// 3. only then does a tab stop being shown at all.
///
/// Having 2 and 3 the wrong way round was not a matter of taste: on a supported
/// 40-column strip a long enough workspace name dropped the ACTIVE tab while its
/// own name sat there at full length.
///
/// The counters and the `+` are never shortened or shed. A counter squeezed into
/// `‹1 ` states a wrong count as confidently as a right one, and the `+` is the
/// only mouse route to a new tab. When step 3 does hide a tab the right-hand
/// counter takes it on, because a count computed before the hiding is precisely
/// the confidently-wrong number the rest of this function exists to avoid.
fn condense_to_width(spans: &mut Vec<TabSpan>, width: usize) {
    let w = |s: &TabSpan| s.text.chars().count();
    let total = |v: &Vec<TabSpan>| v.iter().map(w).sum::<usize>();
    // Keeps the first and last character: the brackets or padding spaces that
    // mark the active tab, and the squad label's own surrounding spaces.
    let shrink = |s: &mut TabSpan| {
        let mut chars: Vec<char> = s.text.chars().collect();
        // (x-b465) A group marker must not outlive the name it marks. Columns
        // come off the right, so the `·N` count goes early - correct, it is the
        // least of the three. The leading glyph would otherwise survive to the
        // floor and leave a strip of identical nameless cells, which is worse
        // than no marker at all: before the marker existed each tab still kept
        // its first character. Once the count is gone and the name is down to
        // its last couple of characters, the glyph goes instead.
        let glyph_at = chars.iter().position(|&c| c == TAB_GROUP_GLYPH);
        let count_gone = !chars.contains(&TAB_GROUP_SEP);
        match glyph_at {
            Some(g) if count_gone && chars.len() <= GROUP_GLYPH_FLOOR => {
                // The glyph and the space after it.
                if chars.get(g + 1) == Some(&' ') {
                    chars.remove(g + 1);
                }
                chars.remove(g);
            }
            _ => {
                chars.remove(chars.len() - 2);
            }
        }
        s.text = chars.into_iter().collect();
    };
    while total(spans) > width {
        let widest_tab = spans
            .iter()
            .enumerate()
            .filter(|(_, s)| s.role == SpanRole::Tab && w(s) > 3)
            .max_by_key(|(_, s)| w(s))
            .map(|(i, _)| i);
        if let Some(i) = widest_tab {
            shrink(&mut spans[i]);
            continue;
        }
        if let Some(squad) = spans
            .iter_mut()
            .find(|s| s.role == SpanRole::Squad && s.text.chars().count() > 3)
        {
            shrink(squad);
            continue;
        }
        let Some(i) = spans.iter().rposition(|s| s.role == SpanRole::Tab) else {
            break;
        };
        // The tab did not disappear, it became hidden, so it has to start being
        // counted. Its hit comes along: it is now the nearest hidden tab, and
        // the counter is what walks the strip back to it.
        let dropped = spans.remove(i);
        match spans.get_mut(i) {
            Some(counter) if matches!(counter.role, SpanRole::OverflowRight(_)) => {
                let SpanRole::OverflowRight(n) = counter.role else {
                    unreachable!()
                };
                counter.role = SpanRole::OverflowRight(n + 1);
                counter.text = format!(" {}\u{203a}", n + 1);
                counter.hit = dropped.hit;
            }
            _ => spans.insert(
                i,
                TabSpan {
                    text: " 1\u{203a}".to_string(),
                    flags: cell_flags::DIM,
                    fg: Color::Default,
                    hit: dropped.hit,
                    role: SpanRole::OverflowRight(1),
                },
            ),
        }
    }
}

/// One clickable span in the tab bar: label, render flags, and what a click does
/// (`None` = inert, e.g. the squad-name label).
#[derive(Clone)]
struct TabSpan {
    text: String,
    flags: u8,
    /// (x-df4c US4) The span's foreground: the accent when the tab's rollup is a
    /// Blocked pane, else `Color::Default`.
    fg: Color,
    hit: Option<TabHit>,
    role: SpanRole,
}

/// What a strip span IS, rather than what its click happens to do.
///
/// Two review findings in a row came from reading a span's role off `hit` and
/// its length: an overflow counter carries a `Tab` hit, because clicking it
/// walks the strip, so by behaviour it is indistinguishable from a short tab
/// label. It is not, and [`condense_to_width`] treats the two very differently.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SpanRole {
    /// The workspace name. Inert, and the only span nothing navigates by, which
    /// is what makes it the right thing to shorten under pressure.
    Squad,
    /// A tab's own label.
    Tab,
    /// `‹N `: N tabs hidden to the left of the window.
    OverflowLeft(usize),
    /// ` N›`: N tabs hidden to the right. Carries its count so a strip that has
    /// to hide one more tab can say so, instead of reporting a number that was
    /// computed before the hiding happened.
    OverflowRight(usize),
    /// The `+`: the only mouse route to a new tab.
    NewTab,
}

#[derive(Clone, Copy)]
enum TabHit {
    Tab(TabId),
    NewTab,
}

/// What a left-click on chrome resolves to: server commands to send, a local
/// one-line hint for a row that isn't directly actionable, or a pending confirm
/// (a work-queue card, x-a496 - dispatch is too costly for a silent tap).
enum ChromeHit {
    Cmds(Vec<Command>),
    /// Owned, not `&'static`: an in-flight card's notice carries the
    /// server-computed `where_hint` (v18), which is per-card data.
    Notice(String),
    Confirm(ConfirmAction),
    /// Open the new-workspace name-input overlay (x-9e5e); the `+` footer.
    OpenCreate,
    /// Flip the active squad row's caret locally (x-2f99); no socket write.
    /// (x-975a) Advance one sideline section through the view cycle.
    CycleSection(SectionKey),
    /// Sort the extended table by the clicked header column.
    SortColumn(AgentSortColumn),
    /// (x-c5ee) Toggle a squad's top-K idle expansion - the idle sibling of
    /// `CycleSection`. A pure local set flip, no socket write.
    ToggleIdle(SectionKey),
    /// (x-b186) Advance the sideline density: the top-right button's click,
    /// routed to the same mutation the keybind runs so the two cannot diverge.
    CycleDensity,
    /// Open the sideline MENU popup anchored at the footer's menu cell (x-8ccf
    /// US4). Carries the click cell so the popup anchors under the pointer.
    OpenSidelineMenu {
        row: u16,
        col: u16,
    },
}

/// The [`ChromeHit`] for an agent row: focus its pane, else reach a paneless
/// live row through the ONE dedicated thread pane, else resume a resumable row
/// through its harness, else say it has no pane here. Shared by a sideline
/// click ([`View::row_action`]) and the navigator's goto ([`View::nav_rows`])
/// so the two inputs resolve the same entity identically (x-653d). Pure - the
/// agent's own fields decide, so no `&self` needed.
fn agent_hit(a: &AgentRow, _active_squad: u64) -> ChromeHit {
    match a.pane_id {
        Some(pid) => ChromeHit::Cmds(vec![Command::FocusPane(pid)]),
        None if !a.exited => {
            // (x-07c2) Every paneless live row reaches a portal: Drive
            // attaches, Follow tails the transcript, Locate renders the
            // explanation screen. One command, no placement dialog - the
            // server owns the tier (the row set lives there). The command's id
            // is the attach id for a claude row and the row NAME for every
            // other harness (Follow/Locate rows carry no attach id); the
            // x-e10f refusal invariant holds by construction because the
            // client never chooses between attach and focus.
            //
            // (x-8f9d) The default gesture names portal 0, which is exactly
            // where the single thread pane always landed. `P` opens the next
            // free portal beside it; nothing about this path changed.
            let id = a.attach_id.clone().unwrap_or_else(|| a.name.clone());
            ChromeHit::Cmds(vec![Command::AttachAgent {
                id,
                placement: PanePlacement {
                    portal: Some(0),
                    ..PanePlacement::default()
                },
            }])
        }
        None => {
            // (x-5f7f) A paneless row whose harness owns a resume form resumes
            // through that form: the server spawns `claude --resume <sid>` /
            // `codex resume <sid>` as a pane in the recorded cwd. The
            // operator's explicit gesture - restore never sends this.
            if a.resumable {
                ChromeHit::Cmds(vec![Command::ResumeAgent {
                    name: a.name.clone(),
                }])
            } else {
                ChromeHit::Notice(no_pane_notice(a))
            }
        }
    }
}

/// The rollup state of a pane/agent, worst-first. The navigator's state filter
/// (x-653d), the squad-row rollup (x-d140), and seen/unseen surfacing (x-4328)
/// all consume it. Derived, never wire-serialized - computed from [`AgentBadge`]
/// + the seen bit at render time. The derive orders it `Blocked < Working <
/// DoneUnseen < Unmeasured < Idle < Empty`, so a squad rollup is
/// `agents.map(pane_state).min()` (the worst state wins - x-d140's `min` and
/// this filter agree on the ordering). `Unmeasured` (x-d401) ranks worse than
/// every settled state: a no-reading row deserves a look before an idle one
/// does. `Empty` (x-d401) is a pristine shell - nothing running, nothing done,
/// the least severe reading there is.
///
/// NOT the same ordering as [`SEVERITY_ORDER`], and neither is "the single
/// ordering authority" the two comments used to each claim. They answer
/// different questions and have disagreed since before x-d401: this one
/// answers WHICH ROW IS WORST, for a `min()` rollup and the navigator filter,
/// so `Working` outranks `DoneUnseen`. `SEVERITY_ORDER` answers IN WHAT ORDER
/// A SECTION'S COUNTS ARE LISTED AND TRUNCATED, where a finished-but-unseen
/// row leads because it is the one asking for a human. Read whichever answers
/// the question at hand, and change neither to "match" the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PaneState {
    Blocked,
    Working,
    DoneUnseen,
    Unmeasured,
    Idle,
    Empty,
}

/// Derive a [`PaneState`] from an agent's badge, whether its output has been
/// seen (x-4328's `AgentRow.seen`, server-owned), and the pane's own OSC 133
/// activity reading (x-d401's `AgentRow.pane_activity`). A present badge wins
/// (the registry worker's own in-TTL report); a `Done` badge folds to `Idle`
/// once seen, else `DoneUnseen`. With NO badge the vt reading decides, and an
/// absent/unmeasured reading renders `Unmeasured` - never a blind `Idle`, the
/// fold that drew four working panes and thirty empty shells as one circle.
fn pane_state(badge: Option<AgentBadge>, seen: bool, activity: Option<ShellActivity>) -> PaneState {
    match badge {
        Some(AgentBadge::Blocked) => PaneState::Blocked,
        Some(AgentBadge::Working) => PaneState::Working,
        Some(AgentBadge::Done) if seen => PaneState::Idle,
        Some(AgentBadge::Done) => PaneState::DoneUnseen,
        None => match activity {
            // ACCEPTED COST, ruled 2026-08-26, do not relitigate: a bare shell
            // running `less`, `nvim` or a long build also lands here, so it
            // reads `Working` and holds an attention row it does not need.
            // That is the cheaper error by design. This epic's founding
            // measurement was four codex panes carrying `fno_id=-` while
            // running `cargo test`, `fno doctor test` and `fno do pr wait`.
            // Over-surfacing a shell costs one row; under-surfacing a working
            // agent is the defect this branch ships to delete.
            Some(ShellActivity::Running) => PaneState::Working,
            Some(ShellActivity::Idle) => PaneState::Idle,
            Some(ShellActivity::Empty) => PaneState::Empty,
            Some(ShellActivity::Unmeasured) | None => PaneState::Unmeasured,
        },
    }
}

/// Attention display window: a transcript-backed row silent longer than this
/// reads as neglected and floats to the top of the agents table. Deliberately
/// much tighter than session-truth's two-hour stall window - that one is the
/// reap-safety window and 7200s is correct for it, but its verdict reads
/// `reachable` for anything dead inside it, and that gap is exactly where a
/// stale-live worker hides. Ten minutes catches a twenty-minute floor with
/// headroom. Display and ordering only: no verdict, falsifier, or reap
/// decision keys off this constant.
const STALE_ATTENTION_S: u64 = 600;

/// Where a row sits on the evidence-of-neglect scale. Built ONLY from fields
/// that carry their evidence with them (`basis`, `last_activity_age_s`,
/// `exited`, `unmeasured`) - never from `status` or the reachability verdict,
/// both of which read healthy for a worker dead under two hours, and never
/// from the in-TTL badge, which is a scraped report rather than a fact.
fn evidence_rank(a: &AgentRow) -> u8 {
    let basis = a.basis.as_deref();
    // A fired falsifier, or an exit with positive corroboration (a confirmed
    // dead pid / gone pane): the sunk tier. Archived, snoozed and pane-dead
    // all collapse here - none of them needs the operator's attention now.
    // `exit-recorded` fires when reconcile already nulled the pid, which can
    // leave the mux's OWN `exited`/`unmeasured` pair reading dormant (x-9239
    // review: the probe saw the tombstone the mux's liveness derivation
    // cannot see); the basis string is the more complete reading here and
    // must win. Mirrors the falsifier set `fno agents list` and the daemon
    // projection both sink to their tier 5.
    if matches!(
        basis,
        Some("process-gone") | Some("pane-gone") | Some("exit-recorded")
    ) || (a.exited && !a.unmeasured)
    {
        return 5;
    }
    // Claiming to work, silent past the attention window: the row that most
    // needs a human is the row that says it is fine and is not.
    if basis == Some("transcript")
        && a.last_activity_age_s
            .is_some_and(|s| s >= STALE_ATTENTION_S)
    {
        return 0;
    }
    if basis == Some("silent") {
        return 1;
    }
    if basis == Some("no-evidence") {
        return 2;
    }
    // Dormant but resumable, with no corroboration either way: the operator
    // has to look before acting, which outranks a healthy worker.
    if a.exited && a.unmeasured {
        return 3;
    }
    // Genuinely working, no probe answer yet, or fresh transcript: needs
    // nothing. `basis: None` (an old server that never sends the field)
    // deliberately lands here too - absence of a reading is not urgency.
    4
}

/// The attention key: needs-me fold rank, then evidence of neglect, then
/// longest-silent first, then name so the table never shuffles on a scrape
/// tick. Term 3 treats an absent age as 0 (youngest): an absent reading has
/// two explanations and a sort cannot tell them apart, so it must never float
/// a row to the top.
fn attention_key(a: &AgentRow, need: Option<NeedKind>) -> (u8, u8, std::cmp::Reverse<u64>, &str) {
    // `NeedKind`'s declaration order IS the severity contract (pinned by
    // test); `as u8` reads it without re-declaring a second authority.
    let need_rank = need.map_or(7, |k| k as u8);
    (
        need_rank,
        evidence_rank(a),
        std::cmp::Reverse(a.last_activity_age_s.unwrap_or(0)),
        a.name.as_str(),
    )
}

/// (x-c5ee) A LIVE idle row - the top-K cap's fold target. Exited is checked
/// first, exactly as [`agent_lattice_state`] does, so a dead worker (whose
/// `pane_state` also reads `Idle`) is never mistaken for live idle and swept
/// into `+N more`: dead rows are the section view's business, not the cap's.
/// (x-d401) The fold takes every NON-ATTENTION state, which after this branch
/// means three of them, not one. `pane_state` used to answer `Idle` for any
/// badgeless row; the split sends a pristine shell to `Empty` and a row with
/// no reading to `Unmeasured`, and `server.rs` hard-codes `pane_activity:
/// None` on watch-only paneless rows - so a bg `/target` worker between turns
/// is `Unmeasured`. Testing `== Idle` alone therefore counted every shell AND
/// every bg worker as `attention`, drove `idle_budget` to zero and killed the
/// fold outright on any real fleet.
///
/// Folding an unmeasured row is safe only because the fold row says `+N more`
/// rather than `+N idle`. An earlier revision of this comment claimed the `?`
/// glyph carried the honesty "wherever the row renders", which is exactly
/// wrong for a row the fold REMOVED: the count would then have stood in for a
/// measurement nothing took, on the surface this branch exists to fix. The
/// label carries it instead, and it asserts only that the rows are hidden.
///
/// This predicate is display density, not truth. Attention states (`Blocked`,
/// `Working`, `DoneUnseen`) are enumerated by exclusion on purpose - a state
/// added later does not fold, so a new signal fails VISIBLE, never hidden.
fn is_idle_row(a: &AgentRow) -> bool {
    !a.exited
        && matches!(
            pane_state(a.badge, a.seen, a.pane_activity),
            PaneState::Idle | PaneState::Empty | PaneState::Unmeasured
        )
}

/// Why a session needs a human, worst-first (x-feec). Declaration order IS the
/// severity contract: the needs-me queue is `(kind, ts, name)`-sorted, so the
/// worst band leads and the longest-waiting fold item tops its band (a live
/// badge row carries no ts, so it degenerates to name order within its band -
/// leg-1 and leg-2 never share a band, so the two orderings never mix). Same
/// declaration-order `Ord` trick as [`PaneState`]. `Decision` (x-e3be) is fed
/// by two `needs.rs` fold arms - `carveout_stale` (an aged unharvested
/// carve-out pile), `stale_claims` (an aged orphaned `node:` claim pile) -
/// read from durable on-disk state rather than a recent event, so neither is
/// windowed by the fold's 24h `since` bound. `Question` (x-f730) is the third
/// arm, `operator_question`; it is split out from `Decision` because the
/// overlay renders it from a richer, separately-folded leg
/// ([`crate::needs_overlay::QuestionItem`]) instead - `Question` on a
/// `NeedRow` exists only so a blocked worker's roster badge still reflects an
/// open question; `View::needs_operator_queue` filters `Question` out of the
/// overlay for exactly that reason (the richer row replaces it there).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NeedKind {
    /// Leads the order: an open operator question outranks everything
    /// downstream of it, including a mail-blocked worker.
    Question,
    /// An aged carve-out/claims pile gone stale - a decision with no asker to
    /// answer back to, so it stays a plain `NeedRow` even in the overlay.
    Decision,
    // A human-addressed mail escalation (question, or a live-miss to an
    // operator-attended recipient). Sits above the worker blocked-states: a
    // human the mail is waiting on outranks a worker awaiting focus.
    MailQuestion,
    BlockedAnswerable,
    BlockedFocusOnly,
    ReviewWedged,
    BudgetStop,
    DoneUnseen,
}

/// The leading glyph per need kind, matching the sideline's [`nav_glyph`]
/// vocabulary where they overlap (blocked `▲`, done `✓`).
fn need_glyph(k: NeedKind) -> char {
    match k {
        NeedKind::Question | NeedKind::Decision => '⁉',
        NeedKind::MailQuestion => '✉',
        NeedKind::BlockedAnswerable | NeedKind::BlockedFocusOnly => '▲',
        NeedKind::ReviewWedged => '⏳',
        NeedKind::BudgetStop => '⏹',
        NeedKind::DoneUnseen => '✓',
    }
}

/// One unified needs-me-queue row: a live badge row (leg 1) or an event-fold
/// item joined to the roster (leg 2), reduced to what the overlay renders and
/// routes on. Identity for cursor re-anchor is `(kind, name)`.
#[derive(Clone)]
struct NeedRow {
    kind: NeedKind,
    name: String,
    reason: String,
    /// The deciding event ts for a fold row (oldest-first tie-break); `""` for a
    /// live badge row (name-ordered within its band).
    ts: String,
    /// A STABLE re-anchor key: a fold row's session id (survives a joined <->
    /// squadless flip, where `name` changes), a badge row's name. Not shown.
    id_key: String,
    /// Present only on a `BlockedAnswerable` row - the digit-answer payload.
    answerable: Option<AnswerablePrompt>,
    pane_id: Option<u64>,
    attach_id: Option<String>,
    squad: Option<u64>,
    tab: Option<u64>,
}

impl NeedRow {
    /// The re-anchor identity: a scrape tick / fold merge keeps the cursor on
    /// the same item, not the same index (AC3-UI). Keyed on `(kind, id_key)` -
    /// a stable session id for fold rows so a joined<->squadless transition (its
    /// `name` flips) does not drop the cursor (codex P2).
    fn id(&self) -> (NeedKind, String) {
        (self.kind, self.id_key.clone())
    }
}

#[derive(Clone)]
enum NeedsOverlayRow {
    Mine(crate::needs_overlay::MineItem),
    Question(crate::needs_overlay::QuestionItem),
    Need(NeedRow),
}

impl NeedsOverlayRow {
    #[cfg(test)]
    fn label(&self) -> &str {
        match self {
            Self::Mine(item) => &item.text,
            Self::Question(q) => q.ask.as_deref().unwrap_or(&q.question),
            Self::Need(row) => &row.name,
        }
    }

    fn id(&self) -> NeedsOverlayId {
        match self {
            Self::Mine(item) => NeedsOverlayId::Mine(item.n),
            Self::Question(q) => NeedsOverlayId::Question(q.id.clone()),
            Self::Need(row) => {
                let (kind, key) = row.id();
                NeedsOverlayId::Need(kind, key)
            }
        }
    }

    fn need(&self) -> Option<&NeedRow> {
        match self {
            Self::Need(row) => Some(row),
            Self::Mine(_) | Self::Question(_) => None,
        }
    }

    /// The rich question payload, when this row is one - `answer_keys`' digit
    /// and Enter arms read this rather than `.need()` (a question is not a
    /// `NeedRow`; it has no pane, an asker instead).
    fn question(&self) -> Option<&crate::needs_overlay::QuestionItem> {
        match self {
            Self::Question(q) => Some(q),
            Self::Mine(_) | Self::Need(_) => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum NeedsOverlayId {
    Mine(usize),
    Question(String),
    Need(NeedKind, String),
}

struct NeedsProjection {
    rows: Vec<NeedsOverlayRow>,
    mine_shown: usize,
    mine_total: usize,
    need_shown: usize,
    need_total: usize,
}

impl NeedsProjection {
    fn selected_line(&self, sel: usize) -> usize {
        if sel < self.mine_shown {
            // instruction + MINE heading
            2 + sel
        } else {
            // instruction + MINE heading/rows/footer + THEY NEED YOU heading
            4 + self.mine_shown + sel.saturating_sub(self.mine_shown)
        }
    }
}

/// Cap on rendered rows (worst-first, so the cap drops only the least severe);
/// the footer states the drop count. Matches the sideline card cap.
const NEEDS_CAP: usize = 10;
const MINE_CAP: usize = 10;
/// Re-open cache: a fold younger than this is reused instantly (mashing
/// `prefix+a` never re-shells - Perspective B).
const NEEDS_CACHE_TTL: Duration = Duration::from_secs(5);
/// (x-b2bf) The yard's re-open cache. Longer than the needs fold's because
/// the identity leg pays a full Python interpreter start plus an archive
/// parse for data that is nearly static (species and album history change
/// on merge cadence, not keystroke cadence).
const YARD_CACHE_TTL: Duration = Duration::from_secs(60);

/// Default fold window: the last 24h (the fold also windows server-side).
const NEEDS_WINDOW_SECS: u64 = 24 * 60 * 60;

/// One row of the navigator's flat catalog (x-653d). Fully owned (no layout
/// borrow) so goto can mutate the view after the catalog is built; recomputed
/// per keypress, never cached.
struct NavRow {
    /// The displayed label (e.g. `nairobi › build` or `nairobi › claude#3`).
    /// Rendering only - the text filter matches [`NavRow::match_key`].
    label: String,
    /// (x-e10f) The searchable identity, composed at build time and NEVER
    /// displayed: `label` plus the mux pane id, the bound node id, the
    /// title-slug and the workspace name, lowercased and space-joined. The
    /// text filter matches HERE, so a row is findable by what it IS (pane
    /// `307`, node `x-6233`, a slug) not just by its label text. A row whose
    /// hit is invisible in the label shows the matched token
    /// ([`nav_overlay_lines`]) - a hit with no visible reason reads as a bug.
    match_key: String,
    /// Derived rollup state, for the state filter + the leading glyph.
    state: PaneState,
    /// Switch to this squad before applying `hit`, when it is not already the
    /// active one (an agent's or pane's row lives in another squad). `None` for a
    /// squad/tab row (the switch is in `hit`) or a card (never switches).
    goto_squad: Option<u64>,
    /// Switch to this tab (after `goto_squad`) before applying `hit`, when it is
    /// not the active view's tab. `Some` only for a pane row (a pane lives in a
    /// specific tab, which `FocusPane` alone does not select); `None` for every
    /// other row. Together the two prefixes give a pane goto the full
    /// SelectSquad -> SelectTab -> FocusPane sequence.
    goto_tab: Option<u64>,
    /// The terminal action: SelectSquad/SelectTab for a container row,
    /// FocusPane/AttachAgent for an agent, the dispatch confirm / focus for a
    /// card, or a [`ChromeHit::Notice`] that keeps the navigator open.
    hit: ChromeHit,
}

impl NavRow {
    /// (x-e10f fix) The ONLY construction path: `match_key` is composed from
    /// `label` + `tokens` here, so a row can never exist with an unset key.
    /// The first cut left the field as `String::new()` at every literal plus
    /// a chained `with_match_key` call - a row class that forgot the chain
    /// compiled fine and rendered fine but was unfindable by EVERY query,
    /// including its own label (F4 on PR 1194). Construction now carries the
    /// invariant.
    fn new(
        label: String,
        state: PaneState,
        goto_squad: Option<u64>,
        goto_tab: Option<u64>,
        hit: ChromeHit,
        tokens: &[String],
    ) -> Self {
        Self {
            match_key: nav_match_key(&label, tokens),
            label,
            state,
            goto_squad,
            goto_tab,
            hit,
        }
    }
}

/// (x-e10f) Compose a nav row's searchable identity: the label plus every
/// non-empty token, lowercased and space-joined. An unresolved field
/// contributes NO token, so a squad row without a node binding is not made
/// matchable by the empty string.
fn nav_match_key(label: &str, tokens: &[String]) -> String {
    let mut key = label.to_lowercase();
    for t in tokens {
        if !t.is_empty() {
            key.push(' ');
            key.push_str(&t.to_lowercase());
        }
    }
    key
}

/// (x-e10f) Whether a nav row's goto lands on pane `pid`: every pane-hosted
/// row class (a plain pane, a pane-hosted agent) applies `FocusPane(pid)`.
/// The cursor-seed predicate for opening the navigator on the focused pane.
fn nav_row_targets_pane(r: &NavRow, pid: u64) -> bool {
    matches!(&r.hit, ChromeHit::Cmds(cs) if cs.iter().any(|c| matches!(c, Command::FocusPane(p) if *p == pid)))
}

/// The navigator state of an agent row: an exited pane reads `Idle` (finished,
/// nothing to act on); otherwise derive from the badge + the server-owned
/// seen bit (x-4328): a looked-at `Done` reads `Idle`, an unseen one
/// `DoneUnseen`.
fn nav_agent_state(a: &AgentRow) -> PaneState {
    if a.exited {
        PaneState::Idle
    } else {
        pane_state(a.badge, a.seen, a.pane_activity)
    }
}

/// An agent row's icon-lattice state (x-df4c US2): exit beats badge beats
/// liveness. Unlike [`nav_agent_state`] this keeps `Exited` distinct (`✗`)
/// rather than folding it to `Idle` - a row shows its own exit, but a squad/tab
/// rollup ignores it. Within the exited case, `unmeasured` (x-9de7) further
/// splits `Exited` (`✗`, confirmed dead, respawn is safe) from `Unmeasured`
/// (`?`, no corroboration, look before you spawn) - the operator's routing
/// decision turns on telling the two apart at a glance. The non-exit case goes
/// through `pane_state`, so the row respects the `seen` bit (x-4328) exactly
/// as the nav/tab rollups do: a Done pane the operator has already viewed
/// folds to `Idle` (`○`) instead of holding a stale bold `✓` needs-attention
/// marker - one system across every surface.
fn agent_lattice_state(a: &AgentRow) -> LatticeState {
    if a.exited {
        if a.unmeasured {
            LatticeState::Unmeasured
        } else {
            LatticeState::Exited
        }
    } else {
        pane_to_lattice(pane_state(a.badge, a.seen, a.pane_activity))
    }
}

/// A queue card's icon-lattice state (x-df4c US3): Ready unifies with `Idle`
/// (hollow waiting), InFlight with `Working` (filled running), Blocked stays
/// the accent state - so cards and agent rows render the identical vocabulary.
fn card_lattice_state(s: CardState) -> LatticeState {
    match s {
        CardState::Ready => LatticeState::Idle,
        CardState::InFlight => LatticeState::Working,
        CardState::Blocked => LatticeState::Blocked,
    }
}

/// Fold a tab's LIVE panes to their worst lattice state for the tab-strip
/// rollup (x-df4c US4). Exited panes are filtered BEFORE the fold, so `None`
/// (no glyph) means "no live panes" - an empty tab or an all-exited tab, which
/// both render byte-identically to a stateless tab (AC2-EDGE). A tab with live
/// panes always rolls up, so a live-idle tab yields `Some(Idle)` -> the outline
/// `○` (the tab state machine distinguishes a live-idle tab from a dead one).
/// Severity is `PaneState`'s Ord (Blocked < Working < DoneUnseen < Idle), so
/// `.min()` is the worst pane.
fn tab_rollup_state(agents: &[AgentRow], squad: u64, tab: TabId) -> Option<LatticeState> {
    let worst = agents
        .iter()
        .filter(|a| a.squad == Some(squad) && a.tab == Some(tab) && !a.exited)
        .map(nav_agent_state)
        .min()?;
    Some(pane_to_lattice(worst))
}

/// The navigator state of a work-queue card: blocked/in-flight map onto
/// `Blocked`/`Working` so the state filter surfaces stuck and running work
/// uniformly with agents; a ready card is neutral (`Idle`).
fn card_state(c: &BacklogCard) -> PaneState {
    match c.state {
        CardState::Blocked => PaneState::Blocked,
        CardState::InFlight => PaneState::Working,
        CardState::Ready => PaneState::Idle,
    }
}

/// A named tab's visible label width in the tab bar / sideline (x-c150);
/// keeps ~4 labeled tabs visible at 100 cols.
const TAB_LABEL_W: usize = 14;

/// A tab span's label body (x-c150): the bare 1-based ordinal when the
/// server-derived name carries no signal (the name IS the ordinal -
/// byte-for-byte today's render for a plain shell tab, AC1-EDGE), else
/// `{ordinal}:{name}` with the name truncated to [`TAB_LABEL_W`] chars. The
/// ordinal stays visible in every span because the `1-9 select tab` keys
/// key off it (Locked 5).
/// The mux's home workspace surfaces the bare brand `fno` in the tab strip
/// (x-597f: derived from the cwd basename); render it bracketed as `f[no]`.
/// Any other workspace name passes through unchanged.
fn brand_label(name: &str) -> String {
    if name == "fno" {
        "f[no]".to_string()
    } else {
        name.to_string()
    }
}

fn tab_label_text(name: &str, i: usize, named: bool) -> String {
    let ordinal = (i + 1).to_string();
    // Collapse (x-0f9d AC7, x-c150): a name equal to its own ordinal renders as
    // the bare digit, byte-identical to an unnamed ordinal - even a chosen one.
    if name == ordinal {
        return ordinal;
    }
    let short: String = name.chars().take(TAB_LABEL_W).collect();
    if named {
        // (x-0f9d US2, supersedes x-c150 Locked 5) A chosen name renders alone,
        // never with a forced `{ordinal}:` prefix.
        short
    } else {
        // A pane-derived or ordinal fallback keeps today's `{ordinal}:{label}`.
        format!("{ordinal}:{short}")
    }
}

/// Mark a tab that holds more than one pane: the stacked glyph in front, the
/// pane count behind. A tab holding four panes rendered identically to one
/// holding a single pane, so an operator could not tell what a tab-level close
/// would destroy before pressing it.
///
/// Separate from [`tab_label_text`] rather than a parameter on it: that helper
/// is layout-free and its tests pin the ordinal-collapse rules, while a pane
/// count is a layout fact. One implementation, shared by the strip and the tab
/// menu's header, so the menu can never describe a different tab than the cell
/// the operator pressed.
///
/// A single-pane tab passes through unchanged, byte-identical to the render
/// before this existed.
fn tab_group_label(label: String, panes: usize) -> String {
    if panes > 1 {
        format!("{TAB_GROUP_GLYPH} {label}{TAB_GROUP_SEP}{panes}")
    } else {
        label
    }
}

/// The stacked glyph a grouped tab leads with, and the separator before its
/// count. Named because [`condense_to_width`] has to recognize them: a marker
/// that outlives the name it marks turns a crowded strip into identical
/// nameless cells.
const TAB_GROUP_GLYPH: char = '▤';
const TAB_GROUP_SEP: char = '·';

/// Total span width at or below which [`condense_to_width`] drops a group glyph
/// rather than another name character: one pad, the glyph and its space, two
/// characters of name, one pad.
const GROUP_GLYPH_FLOOR: usize = 6;

/// Abbreviate `$HOME` to `~` for the status row; only at a path-component
/// boundary so `/home/user2/...` never reads as `~2/...`.
fn abbrev_home(p: &str) -> String {
    // var_os, not var: HOME is a path, and the idiomatic read for a path env
    // var avoids assuming UTF-8 up front. A non-UTF-8 HOME yields None and the
    // path renders unabbreviated. Cached in a OnceLock: this runs on every
    // frame compose (a hot path during output floods) and HOME is fixed for
    // the process lifetime, so the env lookup + global env lock happens once
    // (gemini).
    static HOME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| std::env::var_os("HOME").and_then(|s| s.into_string().ok()));
    abbrev_home_in(p, home.as_deref())
}

fn abbrev_home_in(p: &str, home: Option<&str>) -> String {
    if let Some(h) = home.filter(|h| !h.is_empty()) {
        if let Some(rest) = p.strip_prefix(h) {
            if rest.is_empty() || rest.starts_with('/') {
                return format!("~{rest}");
            }
        }
    }
    p.to_string()
}

#[derive(Debug, Clone, Copy)]
enum OverlayAnchor {
    Center,
    At { row: usize, col: usize },
}

/// One family-B overlay layout. Drawing and mouse hit-testing consume this same
/// framed block and origin, so a close chip cannot drift away from the glyph it
/// paints.
#[derive(Debug, Clone)]
struct OverlayLayout {
    origin: (usize, usize),
    framed: chrome::Framed,
}

impl OverlayLayout {
    fn hit_at(&self, row: u16, col: u16) -> Option<usize> {
        chrome::framed_hit_at(&self.framed, self.origin, row as usize, col as usize)
    }
}

fn family_b_origin(
    anchor: OverlayAnchor,
    block_w: usize,
    block_h: usize,
    content_origin: (usize, usize),
    content_dims: (usize, usize),
) -> (usize, usize) {
    let (base_r, base_c) = content_origin;
    let (content_rows, content_cols) = content_dims;
    let max_r = base_r + content_rows.saturating_sub(block_h);
    let max_c = base_c + content_cols.saturating_sub(block_w);
    match anchor {
        OverlayAnchor::Center => (
            base_r + content_rows.saturating_sub(block_h) / 2,
            base_c + content_cols.saturating_sub(block_w) / 2,
        ),
        OverlayAnchor::At { row, col } => {
            let origin_r = if row.saturating_add(block_h) <= base_r + content_rows {
                row.max(base_r).min(max_r)
            } else {
                row.saturating_sub(block_h).max(base_r).min(max_r)
            };
            (origin_r, col.max(base_c).min(max_c))
        }
    }
}

/// Lay out family-B overlay lines in the content viewport. The body window,
/// frame, origin, and hit spans are calculated once for both drawing and input.
#[allow(clippy::too_many_arguments)]
fn layout_lines_overlay<S: AsRef<str>>(
    content_origin: (usize, usize),
    content_dims: (usize, usize),
    chrome: &chrome::Chrome,
    lines: &[S],
    follow: Option<usize>,
    anchor: OverlayAnchor,
) -> OverlayLayout {
    let (content_rows, content_cols) = content_dims;
    // Body width: the widest line (across the whole body, windowed-out rows
    // included), capped to the viewport minus the side borders.
    let body_w = lines
        .iter()
        .map(|l| l.as_ref().chars().count())
        .max()
        .unwrap_or(0)
        .min(content_cols.saturating_sub(chrome::Chrome::FRAME_COLS));
    // Reserve the chrome overhead and window the body to the rows that remain.
    // Before chrome the body had the whole viewport; the frame borrows `overhead`
    // rows for its border/footer, so without windowing a body that filled the
    // viewport loses its tail off-screen while those rows stay selectable. Top-
    // pin matches the pre-chrome posture (centered when it fits, clipped at the
    // top when it does not); the scrollbar marks the cut.
    let overhead = chrome.rows_overhead();
    let body_budget = content_rows.saturating_sub(overhead);
    let total = lines.len();
    let (start, take, scroll) = if total > body_budget {
        // Covers body_budget == 0 (a viewport shorter than the chrome
        // overhead): windows to zero body rows instead of painting the whole
        // body plus its border past the content viewport.
        //
        // `follow` is the body index that MUST stay visible - a cursor. Without
        // it the window is top-pinned, which is right for a static body and
        // wrong for one the operator drives: the tenth row of a fourteen-row
        // picker on a short terminal would be selectable and invisible, which is
        // the same "you cannot reach it" defect as truncating the list. The
        // window scrolls by the minimum needed to contain the cursor, so it only
        // moves at the edges. `pos` then reports where the window really is,
        // making the scrollbar thumb truthful rather than always parked at 0.
        let start = match follow.filter(|_| body_budget > 0) {
            Some(f) => f.saturating_sub(body_budget - 1).min(total - body_budget),
            None => 0,
        };
        (
            start,
            body_budget,
            Some(chrome::Scroll {
                pos: start,
                total,
                visible: body_budget,
            }),
        )
    } else {
        (0, total, None)
    };
    let body: Vec<chrome::BodyLine> = lines[start..start + take]
        .iter()
        .map(|l| chrome::BodyLine::plain(l.as_ref()))
        .collect();
    let framed = chrome::frame(&body, chrome, body_w, scroll);
    let box_h = framed.lines.len().min(content_rows);
    let box_w = framed.width.min(content_cols);
    let origin = family_b_origin(anchor, box_w, box_h, content_origin, content_dims);
    OverlayLayout { origin, framed }
}

fn draw_overlay_layout(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    layout: &OverlayLayout,
    theme: &Theme,
) {
    let (origin_r, origin_c) = layout.origin;
    // (x-b465) A framed block stamps a SUB-RANGE of each row, so a double-width
    // glyph in the pane content underneath can straddle either edge, leaving one
    // half painted and the row corrupted. The name modal carried this guard when
    // it hand-painted its own block; every family-B overlay needs it for the same
    // reason, so it lives here, once, rather than travelling with one caller.
    for i in 0..layout.framed.lines.len() {
        let r = origin_r + i;
        if r >= rows {
            break;
        }
        // `framed.width`, not `box_w`: `blit` paints the FULL framed width, and
        // `box_w` is that width clamped to the viewport. When the chrome's own
        // minimum (a long title) pushes the frame past the viewport the two
        // differ, and clamping here would leave the real right edge unchecked -
        // stranding a spacer on exactly the overflow this guard exists for.
        blank_straddling_pair(
            cells,
            cols,
            r,
            origin_c,
            (origin_c + layout.framed.width).min(cols),
        );
    }
    chrome::blit(cells, rows, cols, layout.origin, &layout.framed, theme);
}

/// Draw overlay lines centered in the content viewport (right of the sideline,
/// above any splits), framed with `chrome` and colored by `theme`. The seven
/// family-B overlays (catch-up, needs-me, move-pick, attach-place, connections,
/// peek, navigator) all route through here, so framing them all is this one
/// change - the point of chrome being a frame function rather than a field on
/// `Popup`. Cell-bounds-checked (a tiny terminal clips rather than panics).
///
/// `content_origin` is `(TAB_BAR_ROWS, panel_w)`; `content_dims` is the content
/// viewport's `(rows, cols)` (status row excluded). The framed block is centered
/// on its FRAMED dimensions (x-e9c3 placement; x-9f75 policy).
#[allow(clippy::too_many_arguments)]
fn draw_lines_overlay<S: AsRef<str>>(
    cells: &mut [Cell],
    rows: usize,
    cols: usize,
    content_origin: (usize, usize),
    content_dims: (usize, usize),
    chrome: &chrome::Chrome,
    lines: &[S],
    theme: &Theme,
    follow: Option<usize>,
) {
    let layout = layout_lines_overlay(
        content_origin,
        content_dims,
        chrome,
        lines,
        follow,
        OverlayAnchor::Center,
    );
    draw_overlay_layout(cells, rows, cols, &layout, theme);
}

/// The answer-overlay content width; lines truncate to it (AC3-UI: a long
/// option label truncates for display while the daemon fingerprints the full
/// region text) and pad to it so the inverse block is a clean rectangle.
const ANSWER_OVERLAY_W: usize = 54;

/// The footer state of the needs-me overlay: whether the event-fold leg is
/// still in flight, failed (loud degrade, AC2-ERR), or landed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NeedsFooter {
    Folding,
    Degraded,
    AsOf,
}

/// Build the needs-me overlay lines (x-feec, two-laned by x-f730): MINE (the
/// operator's own priorities) above THEY NEED YOU (the severity-ranked union
/// + the selected row's answer options), each with its own state footer, on
/// the shared inverse-video chrome. `sel` is pre-clamped by the caller and
/// indexes `projection.rows` (MINE rows first, then need rows) - a `▸` marks
/// the selected row wherever it falls. An answerable row lists its numbered
/// options only when it is the selected NEED row; a focus-only row is tagged.
/// Always renders both headings - an empty NEED lane shows "nothing needs
/// you", so the overlay never opens blank. Layout is pinned 1:1 with
/// [`NeedsProjection::selected_line`]: MINE rows occupy exactly
/// `mine_shown` lines and need rows exactly `need_shown` (or one "nothing
/// needs you" line), with no extra divider row between them.
fn needs_overlay_lines(
    projection: &NeedsProjection,
    sel: usize,
    mine_footer: NeedsFooter,
    need_footer: NeedsFooter,
) -> Vec<String> {
    let mine_rows = &projection.rows[..projection.mine_shown];
    let need_rows = &projection.rows[projection.mine_shown..];

    let mut lines = vec![pad_to(
        " needs me · digit answers · n/N cycle · ⏎ goto · q close",
        ANSWER_OVERLAY_W,
    )];

    lines.push(pad_to(" MINE", ANSWER_OVERLAY_W));
    for (i, row) in mine_rows.iter().enumerate() {
        let NeedsOverlayRow::Mine(item) = row else {
            continue;
        };
        let marker = if i == sel { '▸' } else { ' ' };
        let check = if item.done { '✓' } else { ' ' };
        lines.push(pad_to(
            &format!(" {marker} [{check}] {}", item.text),
            ANSWER_OVERLAY_W,
        ));
    }
    let mine_footer_line = match mine_footer {
        NeedsFooter::Folding => "   folding...".to_string(),
        NeedsFooter::Degraded => "   MINE unavailable".to_string(),
        NeedsFooter::AsOf if projection.mine_total > projection.mine_shown => format!(
            "   {} of {} shown",
            projection.mine_shown, projection.mine_total
        ),
        NeedsFooter::AsOf => String::new(),
    };
    lines.push(pad_to(&mine_footer_line, ANSWER_OVERLAY_W));

    lines.push(pad_to(" THEY NEED YOU", ANSWER_OVERLAY_W));
    if need_rows.is_empty() {
        lines.push(pad_to("   nothing needs you", ANSWER_OVERLAY_W));
    } else {
        for (i, row) in need_rows.iter().enumerate() {
            let idx = projection.mine_shown + i;
            let marker = if idx == sel { '▸' } else { ' ' };
            match row {
                NeedsOverlayRow::Question(q) => {
                    // Render `ask` as the headline (falls back to the prose
                    // question when the asker gave no one-liner); the prose
                    // itself appears only when selected, below.
                    let stale = if q.live == Some(false) { "  STALE" } else { "" };
                    lines.push(pad_to(
                        &format!(
                            " {marker} {} {}{stale}",
                            need_glyph(NeedKind::Question),
                            q.ask.as_deref().unwrap_or(&q.question)
                        ),
                        ANSWER_OVERLAY_W,
                    ));
                }
                NeedsOverlayRow::Need(r) => {
                    let tag = match r.kind {
                        NeedKind::BlockedFocusOnly => "  ⚠ focus",
                        _ => "",
                    };
                    lines.push(pad_to(
                        &format!(
                            " {marker} {} {}  {}{tag}",
                            need_glyph(r.kind),
                            r.name,
                            r.reason
                        ),
                        ANSWER_OVERLAY_W,
                    ));
                }
                NeedsOverlayRow::Mine(_) => {}
            }
        }
        let selected_need = sel
            .checked_sub(projection.mine_shown)
            .and_then(|i| need_rows.get(i));
        match selected_need {
            Some(NeedsOverlayRow::Need(r)) => {
                if let Some(ans) = r.answerable.as_ref() {
                    if !ans.prompt.is_empty() {
                        lines.push(pad_to(
                            &format!("   {}", ans.prompt.replace('\n', " ")),
                            ANSWER_OVERLAY_W,
                        ));
                    }
                    for o in &ans.options {
                        lines.push(pad_to(
                            &format!("     {}. {}", o.idx, o.label),
                            ANSWER_OVERLAY_W,
                        ));
                    }
                }
            }
            Some(NeedsOverlayRow::Question(q)) => {
                // The prose beneath the headline - only when `ask` was used
                // as the headline above; if there was no `ask`, the headline
                // already IS the question and repeating it would be noise.
                if q.ask.is_some() && !q.question.is_empty() {
                    lines.push(pad_to(
                        &format!("   {}", q.question.replace('\n', " ")),
                        ANSWER_OVERLAY_W,
                    ));
                }
                for (i, opt) in q.options.iter().enumerate() {
                    lines.push(pad_to(&format!("     {}. {opt}", i + 1), ANSWER_OVERLAY_W));
                }
                if q.live == Some(false) {
                    lines.push(pad_to(
                        "   the answer is recorded but reaches no session",
                        ANSWER_OVERLAY_W,
                    ));
                }
            }
            _ => {}
        }
    }
    let need_footer_line = match need_footer {
        NeedsFooter::Folding => "   folding events...".to_string(),
        NeedsFooter::Degraded => "   events fold unavailable - live badges only".to_string(),
        NeedsFooter::AsOf if projection.need_total > projection.need_shown => format!(
            "   {} of {} shown",
            projection.need_shown, projection.need_total
        ),
        NeedsFooter::AsOf => "   as of now".to_string(),
    };
    lines.push(pad_to(&need_footer_line, ANSWER_OVERLAY_W));
    lines
}

/// (x-b2bf) The yard overlay's open state: the crowd cursor plus the open
/// timestamp that drives frame cycling.
struct YardSel {
    sel: usize,
    opened_at: Instant,
}

/// The yard's line builder moved into the module named for it; the alias
/// keeps the one call site and its tests reading the same as before.
use crate::yard_overlay::overlay_lines as yard_overlay_lines;

/// The yard overlay body width: the 12-column sprite plus a label margin,
/// padded to a block rectangle like the needs overlay.
pub(crate) const YARD_OVERLAY_W: usize = 48;
/// Frame period for the flavour leg: frames advance on a timer, never on a
/// state change (a pose carries no reading, which is exactly why cycling it
/// is legal where cycling the eye would not be).
const YARD_FRAME_MS: u128 = 800;

/// The sprite's eye for a roster row (x-b2bf) - the binding that keeps a
/// sprite honest. Computed at render time from the same values the row
/// itself renders (badge, joined need kind, the open-PR fact); there is no
/// stored eye field anywhere. A gift outranks every mood: the PR is open
/// and waiting on the operator. `DoneUnseen` without a PR reads as
/// attention for the same reason it sits in the needs-me queue - the worker
/// finished and nobody has looked. No badge and no need is NO READING:
/// [`crate::sprites::Eye::Reserved`], rendered dim, never as content.
fn yard_eye(a: &AgentRow, need: Option<NeedKind>) -> crate::sprites::Eye {
    use crate::sprites::Eye;
    if a.pr.is_some() {
        return Eye::Gift;
    }
    match need {
        Some(
            NeedKind::Question
            | NeedKind::Decision
            | NeedKind::MailQuestion
            | NeedKind::BlockedAnswerable
            | NeedKind::BlockedFocusOnly
            | NeedKind::DoneUnseen,
        ) => Eye::Attention,
        Some(NeedKind::ReviewWedged | NeedKind::BudgetStop) => Eye::Faded,
        None if !a.exited && a.badge == Some(AgentBadge::Working) => Eye::Working,
        None => Eye::Reserved,
    }
}

/// The navigator overlay content width (x-653d): labels truncate to it and pad
/// to it so the inverse block is a clean rectangle, like the answer overlay.
const NAV_OVERLAY_W: usize = 54;

/// The unified icon lattice (x-df4c): ONE state->style mapping every renderer
/// (sideline rows, queue cards, tab rollups, overlays) calls, so glyph, weight,
/// and accent read as one system. Outline `○` = waiting/idle, filled `●` =
/// active, `▲` = needs-attention (the sole accent state). Exhaustive by design:
/// a new variant is a compile error at every call site, never a silent glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LatticeState {
    Working,
    Idle,
    Blocked,
    DoneUnseen,
    Exited,
    /// (x-9de7) A terminal row with no positive corroboration: no confirmed-
    /// dead pid, no confirmed-gone pane. Distinct from `Exited` because the
    /// operator's routing decision turns on it - `Exited` means respawn is
    /// safe, `Unmeasured` means look before you spawn.
    Unmeasured,
    /// (x-d401) A live pane that positively read as nothing-running-yet: OSC
    /// 133 markers active, no command open, no completed block. Distinct from
    /// `Idle` (a completed block, the prompt back) and from `Unmeasured` (no
    /// reading at all): a pristine shell is an honest zero, not a waiting
    /// worker and not an unknown.
    Empty,
}

/// The terminal theme's accent (index 3 = the emulator's own amber/yellow), kept
/// as the reference value the lattice tests assert against. Production reads the
/// live theme's accent (`self.theme.accent`), so this is test-only - under the
/// default `terminal` theme the two are the same `Indexed(3)` (x-f75e).
#[cfg(test)]
const LATTICE_ACCENT: Color = Color::Indexed(3);

struct LatticeStyle {
    glyph: char,
    flags: u8,
    fg: Color,
}

/// The single source of glyph/weight/color per state. Every state differs from
/// every other by GLYPH alone (BOLD/DIM/accent are reinforcement, never the
/// sole discriminator), so a weak-BOLD or monochrome terminal still reads.
///
/// `accent` is the needs-attention color, now the active theme's accent rather
/// than a hardcoded yellow (x-f75e): under `terminal` it is `Indexed(3)` (the
/// emulator's own amber, preserved exactly), under a named theme it is the
/// palette's pick. Only the one caller that reads `.fg` supplies it; callers
/// that want only the glyph/flags use [`lattice_glyph`] and stay out of color.
/// (x-1b35) The lane fg for one agent row, shared by both sideline arms:
/// the fixed cascade over the row's axes, with the lattice accent standing
/// on Blocked (attention is never re-colored) and the lattice fg as the
/// fallback when nothing declares the lane.
fn agent_lane_fg(a: &AgentRow, st: LatticeState, fallback: Color) -> Color {
    if st == LatticeState::Blocked {
        return fallback;
    }
    sideline_color::resolve_lane_color(
        a.harness.as_deref(),
        a.model.as_deref(),
        a.route.as_deref(),
        a.account.as_deref(),
    )
    .unwrap_or(fallback)
}

fn lattice_style(s: LatticeState, accent: Color) -> LatticeStyle {
    match s {
        LatticeState::Working => LatticeStyle {
            glyph: '●',
            flags: cell_flags::BOLD,
            fg: Color::Default,
        },
        LatticeState::Idle => LatticeStyle {
            glyph: '○',
            flags: 0,
            fg: Color::Default,
        },
        LatticeState::Blocked => LatticeStyle {
            glyph: '▲',
            flags: cell_flags::BOLD,
            fg: accent,
        },
        LatticeState::DoneUnseen => LatticeStyle {
            glyph: '✓',
            flags: cell_flags::BOLD,
            fg: Color::Default,
        },
        LatticeState::Exited => LatticeStyle {
            glyph: '✗',
            flags: cell_flags::DIM,
            fg: Color::Default,
        },
        LatticeState::Unmeasured => LatticeStyle {
            glyph: '?',
            flags: cell_flags::DIM,
            fg: Color::Default,
        },
        LatticeState::Empty => LatticeStyle {
            glyph: '∅',
            flags: cell_flags::DIM,
            fg: Color::Default,
        },
    }
}

/// The glyph + flags for a state, with no color. For every caller that does not
/// read `.fg` (i.e. every caller except the one accent-colored span), so they
/// do not have to thread a theme accent they never use.
fn lattice_glyph(s: LatticeState) -> (char, u8) {
    let st = lattice_style(s, Color::Default);
    (st.glyph, st.flags)
}

/// (x-6851 US2) Severity order for the header rollup strip: most-severe first,
/// so the strip reads `▲ ✓ ● ○ ∅ ✗ ?` and narrow-panel truncation drops from
/// the least-severe (`?`) end. `Unmeasured` (x-9de7) sits after `Exited`: it is a
/// sub-case of the same terminal bucket, just less certain, so it never
/// outranks a live state. `Empty` (x-d401) sits after `Idle` and before the
/// terminal pair: a pristine shell is less severe than any worker state but
/// still a live pane, not a terminal one. The one ordering the fold and the
/// truncation share.
///
/// NOT the same ordering as [`PaneState`]'s derive, and this comment used to
/// claim to be "the single ordering authority" beside a sibling claiming the
/// same thing, which cannot both be true. They answer different questions and
/// have disagreed since before x-d401, on `Working` versus `DoneUnseen`. This
/// one answers IN WHAT ORDER A SECTION'S COUNTS ARE LISTED AND TRUNCATED;
/// `PaneState` answers WHICH ROW IS WORST for a `min()` rollup. A known
/// consequence of the split, left as is: `Unmeasured` is now reachable for
/// LIVE rows, and truncation drops from that end, so a narrow panel can keep
/// a dead `✗` count and drop live `?` rows. Changing that is a display-policy
/// decision, not a correctness fix - do not "align" the two lists to make it
/// go away.
const SEVERITY_ORDER: [LatticeState; 7] = [
    LatticeState::Blocked,
    LatticeState::DoneUnseen,
    LatticeState::Working,
    LatticeState::Idle,
    LatticeState::Empty,
    LatticeState::Exited,
    LatticeState::Unmeasured,
];

/// (x-6851 US2) Fold a section's rows into per-state counts, nonzero only, in
/// severity order. Exhaustive over `LatticeState` (the x-df4c lock-3 posture):
/// a new state is a compile error here, never a silently uncounted glyph.
fn section_rollup(states: impl Iterator<Item = LatticeState>) -> Vec<(LatticeState, usize)> {
    let mut counts = [0usize; SEVERITY_ORDER.len()];
    for st in states {
        let idx = match st {
            LatticeState::Blocked => 0,
            LatticeState::DoneUnseen => 1,
            LatticeState::Working => 2,
            LatticeState::Idle => 3,
            LatticeState::Empty => 4,
            LatticeState::Exited => 5,
            LatticeState::Unmeasured => 6,
        };
        // The match is exhaustive (a new state breaks the build), but the index
        // mapping is coupled by hand to SEVERITY_ORDER's order; this catches a
        // reorder that would silently miscount (gemini review).
        debug_assert_eq!(
            SEVERITY_ORDER[idx], st,
            "SEVERITY_ORDER and section_rollup indices are out of sync"
        );
        counts[idx] += 1;
    }
    SEVERITY_ORDER
        .iter()
        .zip(counts)
        .filter(|&(_, n)| n > 0)
        .map(|(&s, n)| (s, n))
        .collect()
}

/// (x-4374) The flag set for a section header, demoted from the old always-on
/// INVERSE band: the full-width INVERSE is now the focused-row signal, not the
/// header's, so a header carries zero standing INVERSE cells. The rollup counts
/// (`header_band_text`) are unchanged and still fill the full width.
///
/// EVERY header is BOLD, active or not: the earlier split left an inactive
/// header at exactly the weight of the agent rows beneath it. Active stays
/// legible through the `*` marker and the accented caret. Weight alone does not
/// separate a section though - see [`section_rule`].
fn header_band_flags(_active: bool) -> u8 {
    cell_flags::BOLD
}

/// (x-6851 US1+US2) Compose one section header band: the label at the left, the
/// rollup counts right-aligned, spaces between so the whole string is exactly
/// the panel width `w` (the caller paints it as one INVERSE band). Counts are
/// compact `{glyph}{n}` pairs; when the panel is too narrow, whole pairs drop
/// from the least-severe (`✗`) end - a glyph never renders without its count
/// (AC11) - and the label truncates (via `pad_to`) only after every pair is
/// gone. Widths are measured in DISPLAY columns via `glyph_cols` (matching the
/// painter), so a double-width char in a squad name aligns the band instead of
/// overflowing it.
/// The `gap` columns between a header's label and its rollup counts, drawn as a
/// horizontal rule with a space of breathing room at each end (`gap < 3` stays
/// blank - a one-cell dash reads as debris, not a rule).
///
/// The section separator, and the only one available. A terminal grid has one
/// font at one size, and the rest of the vocabulary is already spoken for: BOLD
/// is agent liveness (`lattice_style` bolds working, blocked and done rows, so a
/// header cannot out-weigh a busy workspace), full-width INVERSE is the focused
/// row, DIM is dead, amber is needs-attention. A rule spends none of them and
/// costs no rows, filling space the header already padded with blanks.
fn section_rule(gap: usize) -> String {
    match gap {
        0..=2 => " ".repeat(gap),
        _ => format!(" {} ", "\u{2500}".repeat(gap - 2)),
    }
}

fn header_band_text(label: &str, rollup: &[(LatticeState, usize)], w: usize) -> String {
    let mut pairs: Vec<String> = rollup
        .iter()
        .map(|(s, n)| format!("{}{}", lattice_glyph(*s).0, n))
        .collect();
    loop {
        if pairs.is_empty() {
            let label_w: usize = label.chars().map(glyph_cols).sum();
            return match w.checked_sub(label_w) {
                Some(gap) => format!("{label}{}", section_rule(gap)),
                None => pad_to(label, w),
            };
        }
        let counts = pairs.join(" ");
        let label_w: usize = label.chars().map(glyph_cols).sum();
        let counts_w: usize = counts.chars().map(glyph_cols).sum();
        if label_w + 1 + counts_w <= w {
            let gap = w - label_w - counts_w;
            return format!("{label}{}{counts}", section_rule(gap));
        }
        pairs.pop(); // drop the least-severe pair and retry
    }
}

/// The sideline/nav fold state maps 1:1 onto the lattice (no `Exited` - a folded
/// pane's exit is already flattened to `Idle` by `nav_agent_state`).
fn pane_to_lattice(s: PaneState) -> LatticeState {
    match s {
        PaneState::Blocked => LatticeState::Blocked,
        PaneState::Working => LatticeState::Working,
        PaneState::DoneUnseen => LatticeState::DoneUnseen,
        PaneState::Unmeasured => LatticeState::Unmeasured,
        PaneState::Idle => LatticeState::Idle,
        PaneState::Empty => LatticeState::Empty,
    }
}

/// The leading state glyph for a navigator row (x-653d), sourced from the one
/// icon lattice (x-df4c) so nav and sideline read identically: blocked `▲`,
/// working `●`, done `✓`, idle `○`.
fn nav_glyph(s: PaneState) -> char {
    lattice_glyph(pane_to_lattice(s)).0
}

/// Build the navigator overlay lines (x-653d): a top `find › <query>  [chip]`
/// line, then one line per FILTERED row with a leading state glyph and the
/// cursor row marked `▸`. A row that matched an identity token invisible in
/// its label appends that token as a `·<token>` suffix (x-e10f), so a hit
/// always shows WHY it hit - a row that appears arbitrary reads as a bug. An
/// empty result renders a single `no matches` line (the key handler BELs).
/// `rows` is pre-filtered; `cursor` is pre-clamped.
fn nav_overlay_lines(rows: &[NavRow], nav: &NavView) -> Vec<String> {
    let chip = match nav.state_filter {
        None => "all",
        Some(PaneState::Blocked) => "blocked",
        Some(PaneState::Working) => "working",
        Some(PaneState::DoneUnseen) => "done",
        Some(PaneState::Unmeasured) => "unmeasured",
        Some(PaneState::Idle) => "idle",
        Some(PaneState::Empty) => "empty",
    };
    let mut lines = vec![pad_to(
        &format!(" find › {}   [{chip}]", nav.query),
        NAV_OVERLAY_W,
    )];
    if rows.is_empty() {
        lines.push(pad_to("   no matches", NAV_OVERLAY_W));
        return lines;
    }
    let q = nav.query.to_lowercase();
    for (i, r) in rows.iter().enumerate() {
        let marker = if i == nav.cursor { '▸' } else { ' ' };
        // The reason token: a match-key token that contains the query while
        // the label does not. A query that hits the visible label appends
        // nothing (AC4-UI: the row renders exactly as before).
        let label_lower = r.label.to_lowercase();
        let label = if q.is_empty() || label_lower.contains(&q) {
            r.label.clone()
        } else {
            match r
                .match_key
                .split(' ')
                .find(|t| t.contains(&q) && !label_lower.contains(t))
            {
                Some(token) => format!("{} ·{token}", r.label),
                None => r.label.clone(),
            }
        };
        lines.push(pad_to(
            &format!(" {marker} {} {}", nav_glyph(r.state), label),
            NAV_OVERLAY_W,
        ));
    }
    lines
}

/// The peek overlay content width (x-c376): wider than the navigator/answer
/// overlays because it renders transcript lines, clamped to the terminal by
/// `draw_lines_overlay`.
const PEEK_OVERLAY_W: usize = 72;

/// (x-9c5f US9) Minimum gap between transcript auto-refreshes while peek is open:
/// a Layout push arriving sooner than this since the last fetch for the same row
/// is ignored. Working rows push at the 1s registry cadence, so an active
/// transcript still follows within ~3s; a silent row stops refetching entirely.
const PEEK_REFRESH_INTERVAL: Duration = Duration::from_secs(3);

/// (x-9c5f) Humanize an age in seconds to `Ns`/`Nm`/`Nh`/`Nd` for the peek
/// header's `changed Ns ago` line (Discretion 3). A future stamp (clock skew)
/// is clamped by the caller to 0 before this, so `0s` is the floor.
fn humanize_ago(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// The table's last-activity cell: the same buckets as [`humanize_ago`],
/// right-justified to a fixed width of 4 so the column never reflows when a
/// value rolls from `59m` to `1h`. An absent reading renders EMPTY, like the
/// tail cell - the table never fabricates a placeholder value, and `0s` would
/// be a fabricated one. (`fno agents list` prints `?` for the same absence;
/// that lane's rows are one line each, where a blank reads as a bug.)
fn humanize_age(secs: Option<u64>) -> String {
    let body = match secs {
        None => String::new(),
        Some(s) if s < 60 => format!("{s}s"),
        Some(s) if s < 3600 => format!("{}m", s / 60),
        Some(s) if s < 86_400 => format!("{}h", s / 3600),
        // Capped at 999d (review finding: an uncapped day count breaks the
        // fixed-width-4 invariant once a row is silent for 1000+ days). A row
        // that old is a display curiosity, not a case worth a 5th column.
        Some(s) => format!("{}d", (s / 86_400).min(999)),
    };
    format!("{body:>4}")
}

/// One extended-table row: status glyph, agent, last message, PR, and relative
/// last-update age. Every cell is padded and truncated to its shared layout span
/// so a long name or message stays on one display row.
///
/// Missing PR is rendered as an explicit neutral value; missing message and age
/// remain empty because no honest value exists for those cells.
fn table_row_text(a: &AgentRow, layout: TableLayout, depth: usize, now_secs: u64) -> String {
    let glyph = lattice_glyph(agent_lattice_state(a)).0;
    // (x-1b35) The deviation token rides the agent cell (pad absorbs the few
    // chars), matching the compact arm's vocabulary.
    let token = sideline_color::deviation_token(a.harness.as_deref(), a.model.as_deref())
        .map(|t| format!(" {t}"))
        .unwrap_or_default();
    let name = if depth == 0 {
        format!("{}{token}", a.name)
    } else {
        format!("{}{name}{token}", "  ".repeat(depth), name = a.name)
    };
    let mut out = pad_cols(&format!("{glyph} "), layout.status.width as usize);
    out.push_str(&pad_cols(&name, layout.agent.width as usize));
    if let Some(tail) = layout.tail {
        out.push_str(&pad_cols(
            a.tail.as_deref().unwrap_or(""),
            tail.width as usize,
        ));
    }
    let pr = a.pr.map(|n| format!("#{n}")).unwrap_or_else(|| "—".into());
    out.push_str(&pad_cols(&pr, layout.pr.width as usize));
    let age = match (a.last_activity_age_s, a.updated_at) {
        (Some(s), _) => humanize_age(Some(s)),
        (None, Some(u)) => humanize_age(Some(now_secs.saturating_sub(u))),
        (None, None) => humanize_age(None),
    };
    out.push_str(&pad_cols(&age, layout.age.width as usize));
    debug_assert_eq!(
        out.chars().map(glyph_cols).sum::<usize>(),
        layout.text_w as usize
    );
    out
}

/// (x-b186) The extended table's column-header line.
///
/// Carries the active sort label, which is what makes the sort toggle visible
/// even when the two orders coincide (one agent, or every row in one band): the
/// rows may not move, but this line always changes, so no press is inert.
fn table_head_text(layout: TableLayout, sort: AgentSort) -> String {
    let marker = |column| {
        if sort.column == column {
            match sort.direction {
                SortDirection::Ascending => " ↑",
                SortDirection::Descending => " ↓",
            }
        } else {
            ""
        }
    };
    let mut out = pad_cols(
        &format!("st{}", marker(AgentSortColumn::Status)),
        layout.status.width as usize,
    );
    out.push_str(&pad_cols(
        &format!("agent{}", marker(AgentSortColumn::Agent)),
        layout.agent.width as usize,
    ));
    if let Some(tail) = layout.tail {
        out.push_str(&pad_cols(
            &format!("last msg{}", marker(AgentSortColumn::LastMessage)),
            tail.width as usize,
        ));
    }
    out.push_str(&pad_cols(
        &format!("pr{}", marker(AgentSortColumn::Pr)),
        layout.pr.width as usize,
    ));
    out.push_str(&pad_cols(
        &format!(
            "age{}",
            if sort.column == AgentSortColumn::Age {
                match sort.direction {
                    SortDirection::Ascending => "↑",
                    SortDirection::Descending => "↓",
                }
            } else {
                ""
            }
        ),
        layout.age.width as usize,
    ));
    debug_assert_eq!(
        out.chars().map(glyph_cols).sum::<usize>(),
        layout.text_w as usize
    );
    out
}

/// Wrap `s` into lines no wider than `w` display chars, breaking on spaces. A
/// single word longer than `w` becomes its own line (pad_to ellipsizes it) - a
/// status sentence has no such words in practice, so the simple greedy pass is
/// enough. Always returns at least one (possibly empty) line.
fn wrap_words(s: &str, w: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in s.split_whitespace() {
        match out.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= w => {
                line.push(' ');
                line.push_str(word);
            }
            _ => out.push(word.to_string()),
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Build the read-only peek overlay lines (x-c376): a header (badge glyph + name
/// + full wrapped status sentence), the x-c929 answerable block when the row is
/// blocked (prompt + numbered options, reused verbatim), a divider, then the
/// transcript body (" loading…" until it arrives, "no activity yet" for an empty
/// one, error/timeout text rendered verbatim as body lines) and a footer hint.
/// `agent` is the LIVE row re-read per frame; `None` means it vanished between
/// key and frame (a transient single frame - the key handler re-anchors/closes).
fn peek_overlay_lines(
    agent: Option<&AgentRow>,
    peek: &PeekView,
    reply: Option<&str>,
    now_secs: u64,
) -> Vec<String> {
    let Some(a) = agent else {
        return vec![pad_to(" peek · row gone", PEEK_OVERLAY_W)];
    };
    // Sanitize every external-sourced line (transcript body, scraped reason)
    // before it becomes overlay cells (codex review): `fno agents peek` reads
    // raw on-disk transcript text that can carry ANSI escapes / C0 controls, and
    // the peek path does NOT VT-parse (unlike pane output), so an unstripped
    // ESC/CR would reach the operator's terminal. Tabs become spaces; every
    // other control char is dropped (a residual bracket-code is harmless text).
    fn sanitize_peek_line(s: &str) -> String {
        s.chars()
            .map(|c| if c == '\t' { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect()
    }
    // x-df4c: the peek header reuses the sideline row's lattice state.
    // `agent_lattice_state` is both exit- and seen-aware (it routes the non-exit
    // case through `pane_state`), so the peek, the row, and the rollups agree
    // and no call site re-derives the precedence.
    let glyph = lattice_glyph(agent_lattice_state(a)).0;
    // (x-c914) The account glyph rides the peek header next to the name, same
    // vocabulary as the selector row.
    let mut header = match a.account.as_deref() {
        Some(acct) => format!(" {glyph} {}  @{acct}", a.name),
        None => format!(" {glyph} {}", a.name),
    };
    // (x-9c5f) Additive header labels, each present only when its data exists (no
    // placeholder dashes): `changed Ns ago` (a future stamp / clock skew clamps
    // to `0s` via saturating_sub) and `PR #N`.
    if let Some(updated) = a.updated_at {
        header.push_str(&format!(
            " · changed {} ago",
            humanize_ago(now_secs.saturating_sub(updated))
        ));
    }
    if let Some(pr) = a.pr {
        header.push_str(&format!(" · PR #{pr}"));
    }
    let mut lines = vec![pad_to(&header, PEEK_OVERLAY_W)];
    if let Some(reason) = a.reason.as_deref().filter(|s| !s.is_empty()) {
        for wl in wrap_words(&sanitize_peek_line(reason), PEEK_OVERLAY_W - 3) {
            lines.push(pad_to(&format!("   {wl}"), PEEK_OVERLAY_W));
        }
    }
    // x-c929 answerable block: prompt + numbered options, mirroring the needs-me
    // overlay's body so a blocked peek reads identically. Digit answers (US3)
    // act on exactly these options.
    if let Some(ans) = &a.answerable {
        lines.push(pad_to("", PEEK_OVERLAY_W));
        if !ans.prompt.is_empty() {
            lines.push(pad_to(
                &format!("   {}", ans.prompt.replace('\n', " ")),
                PEEK_OVERLAY_W,
            ));
        }
        for o in &ans.options {
            lines.push(pad_to(
                &format!("     {}. {}", o.idx, o.label),
                PEEK_OVERLAY_W,
            ));
        }
    }
    lines.push(pad_to("", PEEK_OVERLAY_W)); // divider before the transcript
    match &peek.body {
        None => lines.push(pad_to("   loading…", PEEK_OVERLAY_W)),
        Some(body) if body.is_empty() => lines.push(pad_to("   no activity yet", PEEK_OVERLAY_W)),
        Some(body) => {
            for l in body {
                lines.push(pad_to(
                    &format!(" {}", sanitize_peek_line(l)),
                    PEEK_OVERLAY_W,
                ));
            }
        }
    }
    // (x-9c5f) The reply input (`m`) replaces the footer while open; else the
    // footer swaps by row state (attach is a dead end on an exited row - the bug
    // US6 closes - so it becomes `r respawn`; `m reply` shows in both).
    match reply {
        Some(buf) => lines.push(pad_to(
            &format!(" reply: {buf}_ (⏎ send · esc cancel)"),
            PEEK_OVERLAY_W,
        )),
        None => lines.push(pad_to(
            if a.exited {
                " j/k peek · m reply · r respawn · esc back"
            } else {
                " j/k peek · digit answers · m reply · ⏎ attach · esc back"
            },
            PEEK_OVERLAY_W,
        )),
    }
    lines
}

/// Truncate `s` to `w` display chars (ellipsizing) and pad with spaces to `w`,
/// so an overlay line is a fixed-width inverse block that fully overwrites the
/// content beneath it.
/// (x-b186) `pad_to` measured in DISPLAY columns rather than scalar values.
///
/// The painter advances by `glyph_cols`, so a name or tail containing a
/// double-width glyph would occupy more columns than `pad_to` reserved and shove
/// every following cell out of alignment. `header_band_text` already measures
/// this way; the table has the same contract.
fn pad_cols(s: &str, w: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let cw = glyph_cols(ch);
        if used + cw > w {
            // Ellipsis is single-width; leave room for it if anything follows.
            if used < w {
                out.push('…');
                used += 1;
            }
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push_str(&" ".repeat(w.saturating_sub(used)));
    out
}

pub(crate) fn pad_to(s: &str, w: usize) -> String {
    let count = s.chars().count();
    if count > w {
        let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        let mut t = s.to_string();
        t.push_str(&" ".repeat(w - count));
        t
    }
}

// ---------------------------------------------------------------------------
// Attach + main loop
// ---------------------------------------------------------------------------

async fn attach_and_run(
    stream: std::os::unix::net::UnixStream,
    socket: &Path,
) -> Result<i32, String> {
    // A server that dies between accept and Attach (e.g. no spawnable shell)
    // closes the connection without a reason; its stderr has the real cause.
    let log_hint = format!("check {}", log_path(socket).display());
    stream
        .set_nonblocking(true)
        .map_err(|e| format!("socket setup: {e}"))?;
    let stream = tokio::net::UnixStream::from_std(stream).map_err(|e| format!("socket: {e}"))?;
    let (mut sock_r, mut sock_w) = stream.into_split();

    let (cols, rows) = terminal::size().map_err(|e| format!("terminal size: {e}"))?;
    // The launch cwd keys squad selection server-side (squad.rs). An
    // unreadable cwd (deleted directory) degrades to "" - the server treats
    // it as a literal-path squad, never a refused attach.
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Chrome is client-local: report the CONTENT area. A placeholder View
    // computes it before any Layout exists. The session name is the socket
    // stem by construction (`proto::socket_path`).
    let session = socket
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut view = View::new(
        (rows, cols),
        session,
        LayoutView {
            squads: Vec::new(),
            active_squad: 0,
            panes: Vec::new(),
            focus: 0,
            area: (0, 0),
            agents: Vec::new(),
            focus_node: None,
            backlog: Vec::new(),
            backlog_lanes: Vec::new(),
            backlog_stale: false,
        },
    );
    // Latch the focus-follows-mouse off-switch once (x-a496); a direct
    // config.toml read (fail-open to on), the digest_overlay idiom.
    view.hover_focus = crate::digest_overlay::hover_focus_enabled(Path::new(&cwd));
    view.status_on = crate::digest_overlay::status_row_enabled(Path::new(&cwd));
    // The meter's toggle and cadence latch here too; the READING does not -
    // that is the sampler task's job once the toggle is on.
    view.resource_meter_on = crate::digest_overlay::resource_meter_enabled(Path::new(&cwd));
    view.resource_meter_refresh =
        crate::digest_overlay::resource_meter_refresh_secs(Path::new(&cwd));
    view.resource_meter_gate
        .store(view.resource_meter_on, std::sync::atomic::Ordering::Relaxed);
    // A fresh attach with the meter already enabled starts its sampler on the
    // run loop's first iteration (the loop owns meter_tx, one-shot flag).
    view.resource_meter_sampling = view.resource_meter_on;
    view.obsidian = crate::digest_overlay::ObsidianCfg::read(Path::new(&cwd));
    // Same idiom for the optional `~ missions` / `~ backlog` section toggles.
    view.show_missions = crate::digest_overlay::missions_section_enabled(Path::new(&cwd));
    view.show_backlog = crate::digest_overlay::backlog_section_enabled(Path::new(&cwd));
    // (x-f75e) The chrome theme, same ladder. An unknown name falls back to
    // `terminal` WITH a notice - silence here would hide a typo the operator
    // cannot otherwise detect, the same reasoning the keymap notices make.
    let (theme, theme_warn) = crate::digest_overlay::theme_for(Path::new(&cwd));
    view.theme = theme;
    // The key layer (`config.mux.prefix`, `[mux.keys]`), installed BEFORE the
    // scanner reads its first byte. A refused rebind surfaces as a notice rather
    // than silently running the shipped default: a keyboard that quietly ignores
    // your config is indistinguishable from one that ignored your keystroke.
    let (keymap, mut key_warnings) = crate::digest_overlay::keymap(Path::new(&cwd));
    if let Some(w) = theme_warn {
        key_warnings.push(w);
    }
    crate::keys::install(keymap);
    // Held, not stamped. The TTL is an absolute instant, and everything between
    // here and the first paint - a handshake allowed ten seconds, then a
    // catch-up fold - happens before anyone could read it. Stamped at the point
    // the notice can first be SEEN, or a slow server turns "your config was
    // refused" back into the silence this notice exists to break.
    let key_notice = key_warnings
        .first()
        .map(|first| match key_warnings.len() - 1 {
            0 => first.0.clone(),
            n => format!("{} (+{n} more)", first.0),
        });
    let (c_rows, c_cols) = view.content_dims();
    write_msg(
        &mut sock_w,
        &ClientMsg::Attach {
            proto: PROTO_VERSION,
            build: BUILD_VERSION.to_string(),
            rows: c_rows,
            cols: c_cols,
            cwd,
        },
    )
    .await
    .map_err(|e| format!("attach failed: {e}"))?;

    // The first Layout (or refusal) decides everything, BEFORE the terminal
    // is taken over, so a refusal prints as a plain one-liner (AC1-ERR,
    // version skew). ModeSync may precede it on the reliable channel - stash
    // and apply once the TUI owns the terminal.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stashed_modesync: Vec<u8> = Vec::new();
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| format!("server did not answer the attach; {log_hint}"))?;
        let msg = tokio::time::timeout(remaining, read_msg::<_, ServerMsg>(&mut sock_r))
            .await
            .map_err(|_| format!("server did not answer the attach; {log_hint}"))?;
        match msg {
            Ok(ServerMsg::Layout {
                squads,
                active_squad,
                panes,
                focus,
                area,
                agents,
                focus_node,
                backlog,
                backlog_lanes,
                backlog_stale,
                ..
            }) => {
                view.set_layout(LayoutView {
                    squads,
                    active_squad,
                    panes,
                    focus,
                    area,
                    agents,
                    focus_node,
                    backlog,
                    backlog_lanes,
                    backlog_stale,
                });
                break;
            }
            Ok(ServerMsg::ModeSync { bytes }) => stashed_modesync.extend_from_slice(&bytes),
            Ok(ServerMsg::Bye { reason }) => return Err(reason),
            Ok(ServerMsg::Frame { pane_id, frame }) => {
                // Tolerated out-of-order preamble: keep it; the Layout names
                // its rect a message later. The wire trust boundary holds
                // even here: a geometry-inconsistent frame is refused loudly
                // (like a malformed message), never skipped or drawn.
                if !frame.geometry_ok() {
                    return Err(format!(
                        "malformed frame from server: {}x{} but {} cells",
                        frame.rows,
                        frame.cols,
                        frame.cells.len()
                    ));
                }
                view.frames.insert(pane_id, frame);
            }
            // Info answers a pre-Attach Query; the v4 control-verb replies
            // answer one-shot `fno mux pane` connections. Neither belongs on
            // an attached connection - ignore rather than desync.
            Ok(
                ServerMsg::Notice { .. }
                | ServerMsg::Info { .. }
                | ServerMsg::PaneList { .. }
                | ServerMsg::PaneText { .. }
                | ServerMsg::PaneSpawned { .. }
                | ServerMsg::Ok
                | ServerMsg::WaitDone { .. }
                | ServerMsg::Err { .. }
                // Copy and OpenLink answer a mouse-release, and SearchResult
                // answers a search - all can only follow attach: stray in the
                // preamble, ignore rather than desync. LinkHover answers a
                // hover probe (same class).
                | ServerMsg::Copy { .. }
                | ServerMsg::OpenLink { .. }
                | ServerMsg::SearchResult { .. }
                | ServerMsg::LinkHover { .. }
                // PeekBody answers a post-attach PeekAgent (x-c376): impossible
                // in the preamble, ignore rather than desync.
                | ServerMsg::PeekBody { .. }
                // (v41) Script-layout control-verb replies: only ever sent on a
                // one-shot control connection, never to an attached client.
                | ServerMsg::TabList { .. }
                | ServerMsg::LayoutTree { .. }
                | ServerMsg::PaneLocation { .. }
                | ServerMsg::TabSpawned { .. }
                | ServerMsg::PaneFocused { .. }
                | ServerMsg::LayoutApplied { .. }
                | ServerMsg::LayoutGrafted { .. }
                | ServerMsg::TabLocation { .. }
                | ServerMsg::TabClosed { .. }
                // (v60, x-7b5e) Bulk restore answers a one-shot `fno mux
                // workspace restore` control connection, never an attached
                // client. (v71) The prune reload is the same one-shot shape.
                | ServerMsg::WorkspaceRestored { .. } | ServerMsg::SquadReloaded { .. },
            ) => {}
            Err(e) => return Err(format!("attach failed: {e}; {log_hint}")),
        }
    }

    // Socket reads get their own task. `read_msg` is NOT cancellation-safe
    // (a select! that drops it between the length prefix and the body loses
    // the consumed bytes and desyncs the whole stream), so the select loop
    // below must never poll it directly - it drains this channel instead,
    // and mpsc recv IS cancel-safe.
    let (srv_tx, mut srv_rx) = mpsc::channel::<Result<ServerMsg, ProtoError>>(16);
    tokio::spawn(async move {
        loop {
            let msg = read_msg::<_, ServerMsg>(&mut sock_r).await;
            let is_err = msg.is_err();
            if srv_tx.send(msg).await.is_err() || is_err {
                break;
            }
        }
    });

    // Raw stdin -> channel; scanned by the prefix layer below.
    let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(64);
    std::thread::Builder::new()
        .name("fno-mux-stdin".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 4096];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .map_err(|e| format!("stdin thread: {e}"))?;

    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
        .map_err(|e| format!("signal setup: {e}"))?;

    let guard = TerminalGuard::enter()?;
    if !stashed_modesync.is_empty() {
        raw_out(&stashed_modesync).map_err(|e| format!("mode sync: {e}"))?;
    }
    let mut compositor = Compositor::new();
    let mut scanner = Scanner::default();
    // When the pending prefix chord started, for the which-key hint timer
    // (US4). Client-local; the scanner state is the single source of truth
    // for WHETHER a chord is pending, this only remembers SINCE WHEN.
    let mut prefix_since: Option<Instant> = None;
    // (x-e10f fix) When the held global-chord candidate started, for the
    // quiet-window flush (the tmux escape-time analog): a lone Esc must reach
    // the pane even when no further byte ever arrives. The scanner state is
    // the truth for WHETHER a candidate is held; this only remembers SINCE
    // WHEN, exactly like `prefix_since`.
    let mut chord_since: Option<Instant> = None;
    // (x-cf97) Same clock for the held tab number: the scanner state is the
    // truth for WHETHER digits are held; this remembers SINCE WHEN, so a
    // number typed and then left alone still lands.
    let mut digits_since: Option<Instant> = None;
    // Carries a partial SGR mouse report split across reads (mouse.rs).
    let mut mouse_carry: Vec<u8> = Vec::new();
    // Clipboard delivery runs on a blocking thread and reports its outcome back
    // here, so a hanging helper (xclip on a dead X11 link) never parks the UI
    // select loop - the loop keeps draining stdin/frames while the copy lands.
    let (copy_tx, mut copy_rx) =
        tokio::sync::mpsc::unbounded_channel::<(usize, crate::clipboard::CopyOutcome)>();

    // x-a2d0: opening a clicked URL execs `open`/`xdg-open`, which can block for
    // as long as a cold browser launch takes. Same shape as the copy leg above:
    // run it on a blocking thread and report the outcome back here, so the
    // select loop keeps drawing frames while the browser starts.
    let (link_tx, mut link_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Result<(), String>)>();

    // x-feec: the needs-me event-fold leg runs off the UI loop and reports back
    // here, tagged with the generation token it was kicked under, so a slow
    // `fno-agents needs` never blocks the overlay and a result landing after the
    // overlay closed/re-opened is discarded (AC6-FR). x-f730 widened the
    // payload to `FoldOutcome`, the two independent legs (events, MINE) run
    // under `fold_both` so one unavailable command never hides the other.
    let (needs_tx, mut needs_rx) =
        tokio::sync::mpsc::unbounded_channel::<(u64, crate::needs_overlay::FoldOutcome)>();

    // x-f730 task 2.2: a queued MINE mutation (x/d/add) runs off the UI loop
    // and reports back here. Single-flight (`mine_acting`), ungated by
    // generation - a mutation always applies wherever the overlay currently
    // is, since a write from a stale open is still a real write the operator
    // asked for.
    let (mine_act_tx, mut mine_act_rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<(), String>>();

    // x-f730 task 2.3: a queued question answer, same shape and independence
    // as the MINE mutation channel above - its own single-flight
    // (`question_acting`) so an answer and a MINE write never block each
    // other.
    let (question_act_tx, mut question_act_rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<(), String>>();

    // x-b2bf: the yard identity fold leg, same shape as the needs fold -
    // off the UI loop, gen-tagged, one in flight. `None` = fold failed.
    let (yard_tx, mut yard_rx) =
        tokio::sync::mpsc::unbounded_channel::<(u64, Option<Vec<crate::yard_overlay::YardItem>>)>();

    // (x-aeab) The court fold leg: single-flight + the TTL are the whole
    // concurrency contract; no generation to supersede.
    let (court_tx, mut court_rx) =
        tokio::sync::mpsc::unbounded_channel::<Option<crate::court_overlay::Court>>();

    // x-84d7: the Connections modal's read fold runs off the UI loop and reports
    // back here, tagged with the generation it was kicked under, so a slow `fno`
    // never blocks the modal and a result landing after a close/refresh is
    // discarded. Carries a full ReadOutcome (Ok lists, or a named degrade).
    let (conn_tx, mut conn_rx) =
        tokio::sync::mpsc::unbounded_channel::<(u64, crate::connections_view::ReadOutcome)>();

    // x-84d7: a Connections single-flight mutation verb reports its result here,
    // gen-tagged like the read, so a result for a closed/superseded modal is
    // dropped and a live one surfaces the notice + triggers the read-after-write.
    let (conn_act_tx, mut conn_act_rx) = tokio::sync::mpsc::unbounded_channel::<(
        u64,
        crate::connections_view::ActionResult,
        bool, // is_login: keep the pending notice on success, no acting flip
    )>();

    // The sweep verbs (counts probe, scoped apply) run off the UI loop and
    // report back here. The in-flight flag bounds them to one at a time;
    // the message carries what the tap asked for.
    let (sweep_tx, mut sweep_rx) = tokio::sync::mpsc::unbounded_channel::<SweepMsg>();

    // The update-readiness probe runs off the UI loop and reports back
    // here. Untagged (unlike conn_rx) - there is no per-open state to
    // invalidate, just a last-outcome-wins cache the menu/overlay read from.
    let (update_tx, mut update_rx) = tokio::sync::mpsc::unbounded_channel::<UpdateOutcome>();

    // The resource meter's sampler reports its one-line reading here, same
    // last-wins shape. The task itself is spawned by the toggle (and once at
    // startup when config enables the meter) and exits through the view's
    // gate, so an off meter costs nothing.
    let (meter_tx, mut meter_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // x-4e2d: after an absence, fold a "while you were gone" digest for the
    // focused pane's node and show it on the FIRST frame. Fully fail-open (a
    // disabled knob, a too-recent detach, or a slow/absent `fno-agents` all
    // leave `digest` None), so it can never break the attach. It runs before the
    // first paint and is bounded by the 800ms shell-out timeout, so the worst
    // case is first paint delayed by that budget - never an indefinite hang.
    let focused_cwd = view
        .layout
        .squads
        .iter()
        .find(|s| s.id == view.layout.active_squad)
        .map(|s| s.canonical_cwd.clone())
        .unwrap_or_default();
    view.digest = crate::digest_overlay::on_attach(&view.session, &focused_cwd).await;

    // Arm the ONE post-attach update-readiness probe now that the
    // first server frame has landed. A flag set, not an await - the actual
    // subprocess spawns off the UI loop at loop top, so this costs the first
    // paint nothing.
    view.update_probe_want = true;

    // LAST thing before the first paint, deliberately. The deadline is an
    // absolute instant, so every await it is stamped ahead of is lifetime the
    // operator never gets: the handshake, the catch-up fold, and this digest
    // shell-out, which alone is budgeted 800ms of a 3s notice. Anything added
    // between here and the draw below belongs above this line, not under it.
    if let Some(text) = key_notice {
        view.set_notice(text);
    }
    compositor
        .draw(&view.compose())
        .map_err(|e| format!("draw: {e}"))?;

    let exit: Result<i32, String> = loop {
        // x-feec: kick a wanted event-fold off the UI loop, at most ONE in
        // flight (P2-5). Runs at loop top so a want re-armed from either the
        // stdin arm (OpenAnswers) or the needs_rx arm (superseded refold) fires
        // without needing another keypress. The sender lives in this scope, out
        // of the deep stdin handler; the result reports back on needs_rx tagged
        // with this generation.
        if view.needs_want && !view.needs_inflight {
            view.needs_want = false;
            view.needs_inflight = true;
            let tx = needs_tx.clone();
            let gen = view.needs_gen;
            let since = crate::digest_overlay::now_secs()
                .saturating_sub(NEEDS_WINDOW_SECS)
                .to_string();
            tokio::spawn(async move {
                let result = crate::needs_overlay::fold_both(&since).await;
                let _ = tx.send((gen, result));
            });
        }
        // x-f730 task 2.2: kick a queued MINE mutation off the UI loop.
        // `mine_acting` is already set by the stdin handler at enqueue time
        // (mirrors `Connections::acting`), so a second x/d/add press before
        // this one lands is a no-op there rather than a race here.
        if let Some(mutation) = view.mine_action.take() {
            let tx = mine_act_tx.clone();
            tokio::spawn(async move {
                let result = crate::needs_overlay::mine_mutate(mutation).await;
                let _ = tx.send(result);
            });
        }
        // x-f730 task 2.3: kick a queued question answer off the UI loop.
        // `question_acting` is set by the stdin handler at enqueue time,
        // same discipline as the MINE mutation above.
        if let Some((qid, answer)) = view.question_action.take() {
            let tx = question_act_tx.clone();
            tokio::spawn(async move {
                let result = crate::needs_overlay::answer_question(&qid, &answer).await;
                let _ = tx.send(result);
            });
        }
        if view.yard_want && !view.yard_inflight {
            view.yard_want = false;
            view.yard_inflight = true;
            let tx = yard_tx.clone();
            let gen = view.yard_gen;
            tokio::spawn(async move {
                let result = crate::yard_overlay::fold_now().await;
                let _ = tx.send((gen, result));
            });
        }
        if view.court.take_want() {
            let tx = court_tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(crate::court_overlay::fold_now().await);
            });
        }
        // x-84d7: kick a wanted Connections read off the UI loop, at most one in
        // flight, tagged with the current gen so a stale result is dropped.
        if view.conn_want && !view.conn_inflight {
            view.conn_want = false;
            view.conn_inflight = true;
            let tx = conn_tx.clone();
            let gen = view.conn_gen;
            tokio::spawn(async move {
                let outcome = crate::connections_view::load_all().await;
                let _ = tx.send((gen, outcome));
            });
        }
        // x-84d7: run a wanted single-flight mutation off the UI loop. The modal's
        // `acting` flag (set by the reducer) is the concurrency guard, so no extra
        // inflight bool is needed here; the result reports on conn_act_rx.
        if let Some((argv, env, is_login)) = view.conn_action.take() {
            let tx = conn_act_tx.clone();
            let gen = view.conn_gen;
            tokio::spawn(async move {
                let result = crate::connections_view::run_verb_env(argv, env).await;
                let _ = tx.send((gen, result, is_login));
            });
        }
        // Kick a wanted update-readiness probe off the UI loop, at
        // most one in flight. The select loop never blocks on it - the menu
        // and overlay render whatever is already in `view.update_outcome`.
        if view.update_probe_want && !view.update_probe_inflight {
            view.update_probe_want = false;
            view.update_probe_inflight = true;
            let tx = update_tx.clone();
            tokio::spawn(async move {
                let outcome = probe_update_readiness().await;
                let _ = tx.send(outcome);
            });
        }
        // Kick a wanted sweep verb off the UI loop, at most one in flight.
        if let Some(action) = view.sweep_action.take() {
            if !view.sweep_inflight {
                view.sweep_inflight = true;
                let tx = sweep_tx.clone();
                tokio::spawn(async move {
                    let msg = run_sweep_verb(action).await;
                    let _ = tx.send(msg);
                });
            }
        }
        // Redraw-after-event; expiry of the transient notice needs a timer.
        let notice_deadline = view.notice.as_ref().map(|(_, d)| *d);
        // (x-e10f fix) The held global-chord candidate's quiet-window flush
        // deadline. A whole CSI arrives in one read, so a candidate still
        // pending after this window is a lone Esc (or a torn write older than
        // the window) and must not wait for the next keypress.
        let chord_flush_deadline = chord_since.map(|t| t + CHORD_FLUSH_AFTER);
        // (x-cf97) The held tab number's quiet-window deadline; Enter and the
        // non-digit terminator resolve sooner, this only covers
        // number-then-nothing.
        let digits_flush_deadline = digits_since.map(|t| t + DIGIT_FLUSH_AFTER);
        let pane_ids_deadline = view.pane_ids_until;
        // The which-key hint fires once per pending chord (US4, AC4-HP).
        let hint_deadline = if view.hint {
            None
        } else {
            prefix_since.map(|t| t + HINT_DELAY)
        };
        // Focus-follows-mouse settle (x-a496): a pending hover target commits at
        // its landing time + the debounce, re-armed each loop from the latest
        // pending, so a fast sweep's earlier panes are dropped before they fire.
        let hover_deadline = view.hover_pending.map(|(_, t0)| t0 + HOVER_DEBOUNCE);
        // (hover affordance) The link probe's own clock: separate from
        // `hover_deadline` because focus debounces the pane while the link
        // probe debounces the exact cell, and a fired probe stops the clock
        // until motion or a frame restarts it.
        let link_hover_deadline = view.link_hover.deadline();
        // (x-1d91) A dispatched reorder verb the feed never confirmed: the `…`
        // marker must clear with a notice rather than spin forever.
        let backlog_deadline = view.backlog_pending_deadline();
        // (x-d807, AC7-FR) A drag whose mouse-up never arrives - the terminal
        // lost focus mid-gesture, or the release was eaten - would otherwise
        // leave the drag latched, swallowing every later mouse event. Expire it.
        let seam_drag_deadline = view.seam_drag.map(|d| d.last_at + SEAM_DRAG_TIMEOUT);
        let pane_drag_deadline = view.pane_drag.map(|d| d.last_at + PANE_DRAG_TIMEOUT);
        // (x-2e86, Locked 9) The sideline drag now latches continuous resize, so
        // a swallowed mouse-up is worse than under the old single-snap drag.
        // Give it the same backstop seam/pane drags already have.
        let sideline_drag_deadline = view.sideline_drag.map(|d| d.last_at + SEAM_DRAG_TIMEOUT);
        // (x-d6a8 AC1-FR) The tab-cell and sideline-row drags share the same
        // dead-drag reaper: a mouse-up that never arrives must not latch the
        // gesture. Only one of the three is ever live, so one deadline over both
        // new drags suffices.
        let new_drag_deadline = view
            .tab_drag
            .map(|d| d.last_at + PANE_DRAG_TIMEOUT)
            .into_iter()
            .chain(
                view.row_drag
                    .as_ref()
                    .map(|d| d.last_at + PANE_DRAG_TIMEOUT),
            )
            // (x-b465) The press-hold latch joins the same reaper. It has no
            // `last_at` to refresh - a hold is motionless by definition - so its
            // deadline runs from the press. Lose terminal focus mid-hold and the
            // release lands in the other app; without this the latch survives,
            // and a later stray release pops a menu on a row nobody is pressing,
            // its clock long past the long-press threshold.
            .chain(
                view.press_hold
                    .as_ref()
                    .map(|(_, _, start)| *start + PANE_DRAG_TIMEOUT),
            )
            .min();
        // (x-b2bf) The yard's frame cycling is a flavour channel on a timer:
        // while the overlay is open, wake at the next frame boundary so the
        // spotlight animates on an otherwise idle terminal (nothing else
        // redraws there). Re-armed each loop pass, so the cadence holds until
        // the overlay closes; closed -> no deadline, no wakeups.
        // (x-aeab) Refresh timer; the deadline is None while a fold runs.
        let court_tick = view.court.refresh_deadline();
        // (x-b2bf) The yard's frame cycling is a flavour channel on a timer:
        // while the overlay is open, wake at the next frame boundary so the
        // spotlight animates on an otherwise idle terminal (nothing else
        // redraws there). Re-armed each loop pass, so the cadence holds until
        // the overlay closes; closed -> no deadline, no wakeups.
        let yard_tick = view.yard.as_ref().map(|yv| {
            let step = YARD_FRAME_MS as u64;
            let elapsed = yv.opened_at.elapsed().as_millis() as u64;
            yv.opened_at + Duration::from_millis((elapsed / step + 1) * step)
        });
        // The meter's one-shot spawn: the settings toggle sets
        // `resource_meter_sampling`, and the loop - which owns meter_tx -
        // starts the sampler here, exactly once per toggle-on. A fresh
        // attach with the meter already enabled takes the same path via the
        // startup latch below.
        if view.resource_meter_on && view.resource_meter_sampling {
            view.resource_meter_sampling = false;
            spawn_meter_sampler(
                view.resource_meter_gate.clone(),
                view.resource_meter_refresh,
                meter_tx.clone(),
            );
        }
        tokio::select! {
            msg = srv_rx.recv() => match msg.unwrap_or(Err(ProtoError::Closed)) {
                Ok(ServerMsg::Frame { pane_id, frame }) => {
                    if !frame.geometry_ok() {
                        break Err(format!(
                            "malformed frame from server: {}x{} but {} cells",
                            frame.rows, frame.cols, frame.cells.len()
                        ));
                    }
                    // Frames for pane ids unknown to the current Layout are
                    // ignored (Concurrency: flush-then-re-emit ordering).
                    let known = view.layout.panes.iter().any(|(id, _)| *id == pane_id);
                    if known {
                        // (hover affordance) A new frame invalidates the
                        // accepted span and restarts the probe's quiet period
                        // from this frame, so streaming output neither paints
                        // a stale span nor scans at frame cadence.
                        view.link_hover.on_frame(pane_id, Instant::now());
                        view.frames.insert(pane_id, frame);
                        if let Err(e) = compositor.draw(&view.compose()) {
                            break Err(format!("draw: {e}"));
                        }
                    }
                }
                Ok(ServerMsg::Layout { squads, active_squad, panes, focus, area, agents, focus_node, backlog, backlog_lanes, backlog_stale, .. }) => {
                    view.set_layout(LayoutView { squads, active_squad, panes, focus, area, agents, focus_node, backlog, backlog_lanes, backlog_stale });
                    // x-c376: a scrape tick may have removed the peeked row.
                    // Re-anchor to an adjacent agent row (fetch its transcript)
                    // or close - never a stale render / panic (AC1-EDGE).
                    match view.peek_reanchor() {
                        Some((cursor, name)) => {
                            if let Err(e) = fetch_peek(&mut view, cursor, name, &mut sock_w).await {
                                break Err(e);
                            }
                        }
                        // (x-9c5f US9) Same row held: auto-refresh the transcript
                        // if the interval elapsed. Body is kept until the fresh
                        // one lands (peek_refresh_due), so no loading flicker.
                        None => {
                            if let Some((seq, name)) = view.peek_refresh_due() {
                                if let Err(e) = write_msg(
                                    &mut sock_w,
                                    &ClientMsg::Command(Command::PeekAgent { name, seq }),
                                )
                                .await
                                {
                                    break Err(format!("peek refresh failed: {e}"));
                                }
                            }
                        }
                    }
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
                }
                Ok(ServerMsg::ModeSync { bytes }) => {
                    // Reliable-channel ordering guarantees these precede the
                    // Layout/frames that assume them; apply verbatim.
                    if let Err(e) = raw_out(&bytes) {
                        break Err(format!("mode sync: {e}"));
                    }
                }
                Ok(ServerMsg::Notice { text }) => {
                    // (x-1d91) A dispatched reorder verb reports its outcome as
                    // exactly this notice, so a notice arriving mid-verb settles
                    // the `…` marker. Without this a FAILED verb left the card
                    // spinning and every further verb blocked until the timeout,
                    // which then overwrote the real reason with a generic one.
                    view.settle_backlog_pending_on_notice();
                    // (x-f191) A row-scoped outcome stamps its row before the
                    // tab-bar notice takes the full text.
                    view.resolve_row_stamp(&text);
                    view.set_notice(text);
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
                }
                Ok(ServerMsg::Info { .. }) => {} // pre-Attach-only answer; stray here
                // v4 control-verb replies belong to one-shot `fno mux pane`
                // connections, never an attached client's stream: ignore.
                Ok(ServerMsg::PaneList { .. }
                    | ServerMsg::PaneText { .. }
                    | ServerMsg::PaneSpawned { .. }
                    | ServerMsg::Ok
                    | ServerMsg::WaitDone { .. }
                    | ServerMsg::Err { .. }
                    // (v41) Script-layout replies: control-connection only,
                    // never sent to an attached client - ignore, don't desync.
                    | ServerMsg::TabList { .. }
                    | ServerMsg::LayoutTree { .. }
                    | ServerMsg::PaneLocation { .. }
                    | ServerMsg::TabSpawned { .. }
                    | ServerMsg::PaneFocused { .. }
                    | ServerMsg::LayoutApplied { .. }
                    | ServerMsg::LayoutGrafted { .. }
                    | ServerMsg::TabLocation { .. }
                    | ServerMsg::TabClosed { .. }
                    // (v60, x-7b5e) Bulk restore answers a one-shot control
                    // connection only. (v71) The prune reload is the same shape.
                    | ServerMsg::WorkspaceRestored { .. } | ServerMsg::SquadReloaded { .. }) => {}
                Ok(ServerMsg::Copy { text }) => {
                    // Land the server-extracted selection on the clipboard: local
                    // exec first, OSC 52 to the outer terminal as fallback
                    // (Locked 5). The exec chain can hang (xclip on a slow X11
                    // link), so delivery runs on a blocking thread and reports its
                    // outcome back over `copy_tx` - NOT awaited here, so the select
                    // loop keeps draining stdin/frames meanwhile. The status flash
                    // (below, on the outcome arm) makes the copy observable.
                    let chars = text.chars().count();
                    let tx = copy_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let outcome = crate::clipboard::deliver(&text, raw_out);
                        let _ = tx.send((chars, outcome));
                    });
                }
                Ok(ServerMsg::OpenLink { url }) => {
                    // x-a2d0: the server resolved a clicked URL (OSC 8 or
                    // linkified text) and vetted its scheme; `open_url` vets it
                    // again before exec. Off-loop for the same reason Copy is -
                    // a cold browser launch must not stall the render loop.
                    let tx = link_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let outcome = crate::link::open_url(&url);
                        let _ = tx.send((url, outcome));
                    });
                }
                Ok(ServerMsg::LinkHover {
                    pane_id,
                    seq,
                    cells,
                }) => {
                    // (hover affordance) Accept only a reply that still names
                    // the current target (its pane and seq): one for a cell
                    // the pointer left, or one a new frame re-sequenced away,
                    // paints a stale span. A current miss (empty cells)
                    // clears the underline - the cell under the pointer is
                    // not a link.
                    if view.link_hover.on_reply(pane_id, seq, cells) {
                        if let Err(e) = compositor.draw(&view.compose()) {
                            break Err(format!("draw: {e}"));
                        }
                    }
                }
                Ok(ServerMsg::SearchResult {
                    pane_id,
                    total,
                    current,
                }) => {
                    // Land the counter on the active search line. A lost reply
                    // never wedges the client (Esc exits locally); a reply for a
                    // search we already closed is simply dropped. Total 0 = no
                    // matches: a BEL makes the empty result audible (AC1-ERR).
                    // Filter on pane_id AND submitted: results only answer a
                    // submit/step, so a stale reply from a superseded search must
                    // not paint its counter (or a zero-match BEL) onto a different
                    // pane's search, nor onto a new query still being typed.
                    if let Some(sv) = view
                        .search
                        .as_mut()
                        .filter(|sv| sv.pane == pane_id && sv.submitted)
                    {
                        sv.result = Some((total, current));
                        // BEL only while the search is still open: a late
                        // zero-result reply arriving after a local Esc must not
                        // sound a confusing bell.
                        if total == 0 {
                            let _ = raw_out(b"\x07");
                        }
                    }
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
                }
                Ok(ServerMsg::PeekBody { seq, lines, .. }) => {
                    // x-c376: the seq guard (AC1-FR) drops a superseded body so
                    // B's header never shows A's transcript. `name` is a wire
                    // checksum; the header reads the live row.
                    if view.apply_peek_body(seq, lines) {
                        if let Err(e) = compositor.draw(&view.compose()) {
                            break Err(format!("draw: {e}"));
                        }
                    }
                }
                Ok(ServerMsg::Bye { reason }) => break Ok(exit_with_notice(reason)),
                Err(ProtoError::Closed) => {
                    break Ok(exit_with_notice("session ended (server closed)".into()));
                }
                // A decode failure mid-session is not a clean IO drop. It is
                // either a corrupt/truncated frame on the socket, or a newer
                // server whose additive ServerMsg this build cannot decode
                // (the floor admits floor-compatible clients). Name both, or
                // the operator is sent binary-shopping for a connection loss,
                // or reads a real skew as "connection lost".
                Err(e @ ProtoError::Malformed(_)) => break Err(format!(
                    "cannot decode a server message: {e} - this client speaks wire v{}; \
                     either the connection carried a corrupt frame or the server is newer, \
                     and updating fno fixes the latter",
                    crate::proto::PROTO_VERSION
                )),
                Err(e) => break Err(format!("connection lost: {e}")),
            },
            bytes = stdin_rx.recv() => match bytes {
                Some(bytes) => {
                    match handle_stdin(&mut view, &mut scanner, &mut mouse_carry, &bytes, &mut sock_w).await {
                        Ok(StdinFlow::Continue) => {
                            // Sync the hint to the scanner: a chord pending
                            // arms the timer once; anything else clears both
                            // (resolving or abandoning clears the hint,
                            // AC4-HP).
                            if scanner.prefix_pending() {
                                prefix_since.get_or_insert_with(Instant::now);
                            } else {
                                prefix_since = None;
                                view.hint = false;
                            }
                            // (x-e10f fix) Same one-way sync for a held global
                            // chord candidate: arm the quiet-window flush once,
                            // clear it when the candidate resolves or releases.
                            if scanner.chord_pending() {
                                chord_since.get_or_insert_with(Instant::now);
                            } else {
                                chord_since = None;
                            }
                            // (x-cf97) Same one-way sync for a held tab
                            // number: arm the quiet window once; Enter or a
                            // non-digit clears it by clearing the state.
                            if scanner.digits_pending() {
                                digits_since.get_or_insert_with(Instant::now);
                            } else {
                                digits_since = None;
                            }
                            if let Err(e) = compositor.draw(&view.compose()) {
                                break Err(format!("draw: {e}"));
                            }
                        }
                        Ok(StdinFlow::Detach) => {
                            // x-4e2d: stamp the detach time so the next attach can
                            // gate the catch-up digest on how long we were away.
                            crate::digest_overlay::record_detach(&view.session);
                            let _ = write_msg(&mut sock_w, &ClientMsg::Detach).await;
                            break Ok(exit_with_notice("detached; run fno to reattach".into()));
                        }
                        Err(e) => break Err(e),
                    }
                }
                // The stdin thread breaks on EOF and on read error alike; by
                // the time we see None we cannot tell which, so say so.
                None => break Ok(exit_with_notice("stdin ended (closed or read error); detached".into())),
            },
            Some((chars, outcome)) = copy_rx.recv() => {
                // A clipboard delivery finished on its blocking thread: flash the
                // result (AC2-HP) or sound BEL on hard failure (AC2-ERR).
                let notice = match outcome {
                    crate::clipboard::CopyOutcome::Local(_)
                    | crate::clipboard::CopyOutcome::Osc52 { truncated: false } => {
                        format!("copied {chars} chars")
                    }
                    crate::clipboard::CopyOutcome::Osc52 { truncated: true } => {
                        format!("copied {chars} chars (truncated to clipboard limit)")
                    }
                    crate::clipboard::CopyOutcome::Failed => {
                        let _ = raw_out(b"\x07");
                        "copy failed: no clipboard tool and OSC 52 blocked".to_string()
                    }
                };
                view.set_notice(notice);
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            Some((url, outcome)) = link_rx.recv() => {
                // x-a2d0: a click that opens nothing visible reads as a broken
                // button, so BOTH outcomes get a notice - the browser may not
                // even raise itself above the terminal.
                let notice = match outcome {
                    Ok(()) => format!("opened {}", crate::link::for_notice(&url)),
                    Err(e) => {
                        let _ = raw_out(b"\x07");
                        format!("open failed: {e}")
                    }
                };
                view.set_notice(notice);
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            Some((gen, result)) = yard_rx.recv() => {
                // x-b2bf: the identity fold landed; same merge discipline as
                // the needs fold - gen-guarded, degraded-loud on None, never
                // blocking, never cached from a failure.
                view.yard_inflight = false;
                if gen == view.yard_gen && view.yard.is_some() {
                    match result {
                        Some(items) => {
                            view.yard_fold = Some(items);
                            view.yard_degraded = false;
                            view.yard_fold_at = Some(Instant::now());
                        }
                        None => {
                            view.yard_fold = Some(Vec::new());
                            view.yard_degraded = true;
                        }
                    }
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
                }
            }
            Some(result) = court_rx.recv() => {
                // (x-aeab) The fold landed; `apply` owns the merge rules.
                view.court.apply(result);
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            Some((gen, outcome)) = needs_rx.recv() => {
                // x-feec: an event-fold landed; the in-flight fold is done, so a
                // later open may spawn a fresh one (P2-5 bound). x-f730: the
                // outcome now carries both legs (events + MINE), applied
                // independently so one unavailable command never hides the
                // other lane's render.
                view.needs_inflight = false;
                // Merge only if the overlay is still open under the same
                // generation it was kicked for; a result for a closed/superseded
                // overlay is discarded (AC6-FR). If the overlay is still open but
                // moved on (re-opened, still live-only), re-arm a fresh fold.
                if gen == view.needs_gen && view.answers.is_some() {
                    let prev = view.answers_selected_id();
                    match outcome.needs {
                        Some(items) => {
                            view.needs_fold = Some(items);
                            view.needs_degraded = false;
                            // Only a SUCCESS seeds the re-open cache; a failure is
                            // never cached, so the next open retries instead of
                            // silently serving the failed empty fold (P2-6).
                            view.needs_fold_at = Some(Instant::now());
                        }
                        // Fold failed/timed out: keep the live badge leg, flip the
                        // loud degraded notice (AC2-ERR), never a silent partial
                        // queue. An empty Some keeps leg-1 rendering; leave
                        // needs_fold_at untouched so the next open re-folds.
                        None => {
                            view.needs_fold = Some(Vec::new());
                            view.needs_degraded = true;
                        }
                    }
                    match outcome.mine {
                        Some(items) => {
                            view.mine_fold = Some(items);
                            view.mine_degraded = false;
                        }
                        None => {
                            view.mine_fold = Some(Vec::new());
                            view.mine_degraded = true;
                        }
                    }
                    match outcome.questions {
                        Some(items) => {
                            view.questions_fold = Some(items);
                            view.questions_degraded = false;
                        }
                        None => {
                            view.questions_fold = Some(Vec::new());
                            view.questions_degraded = true;
                        }
                    }
                    view.reanchor_answers(prev);
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
                } else if view.answers.is_some() && view.needs_fold.is_none() {
                    // A superseded fold returned while the current overlay still
                    // needs one (re-opened past the cache): kick a fresh fold.
                    view.needs_want = true;
                }
            }
            Some(result) = mine_act_rx.recv() => {
                // x-f730 task 2.2: a queued MINE mutation finished.
                view.apply_mine_action_result(result);
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            Some(result) = question_act_rx.recv() => {
                // x-f730 task 2.3: a queued question answer finished.
                view.apply_question_action_result(result);
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            Some((gen, outcome)) = conn_rx.recv() => {
                // x-84d7: apply a Connections read under the gen guard. A result
                // for a closed/superseded modal is discarded; a live match seeds
                // the lists (or the degraded banner) and repaints.
                view.conn_inflight = false;
                if gen == view.conn_gen {
                    if let Some(cv) = view.connections.as_mut() {
                        cv.apply_read(outcome);
                        if let Err(e) = compositor.draw(&view.compose()) {
                            break Err(format!("draw: {e}"));
                        }
                    }
                }
            }
            Some((gen, result, is_login)) = conn_act_rx.recv() => {
                // x-84d7: a mutation/login verb finished. Clear the single-flight
                // guard UNCONDITIONALLY (the subprocess has exited, whatever the
                // modal's read-gen), so a manual R during the mutation can never
                // wedge `acting` on nor let a second write overlap this one. The
                // notice + re-read are still gen-guarded: a stale/closed result
                // shows nothing (never optimistic state).
                if let Some(cv) = view.connections.as_mut() {
                    cv.acting = false;
                }
                if gen == view.conn_gen && view.connections.is_some() {
                    if let Some(cv) = view.connections.as_mut() {
                        if is_login {
                            // The login pane spawn: on failure name it; on success
                            // keep the reducer's "login pane opened - press r" notice.
                            if !result.ok {
                                cv.notice = Some(result.msg);
                            }
                        } else {
                            cv.notice = Some(result.msg);
                        }
                    }
                    view.rearm_connections_read();
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
                }
            }
            Some(outcome) = update_rx.recv() => {
                // No gen guard needed - this is a last-outcome-wins
                // cache, not a stateful modal read. Redraw so a menu open at
                // the moment this lands shows the fresh row immediately.
                view.update_probe_inflight = false;
                view.update_outcome = Some(outcome);
                view.refresh_open_sideline_menu();
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            Some(text) = meter_rx.recv() => {
                // Last sample wins; the row renders "sensor unavailable" for
                // a failed one, so nothing stale survives a sensor going dark.
                view.resource_meter_text = Some(text);
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            Some(msg) = sweep_rx.recv() => {                // The tap asked for this outcome: counts open the centered
                // choice modal, an apply speaks in a notice. A popup opened
                // after the tap is the operator's newer intent and is never
                // stomped by a landing probe.
                view.sweep_inflight = false;
                match msg {
                    SweepMsg::Counts { tabs, used, dead } => {
                        // A popup opened after the tap is the operator's
                        // NEWER intent; it is never stomped by a landing
                        // probe. The tap is answered with a notice instead
                        // of silently dropped.
                        if view.aux.is_none() {
                            view.aux = Some(build_sweep_modal(tabs, used, dead));
                            view.aux_esc.clear();
                        } else {
                            view.set_notice(format!(
                                "sweep ready: tabs {tabs}, used shells {used}, dead agents {dead} - reopen the menu"
                            ));
                        }
                    }
                    SweepMsg::Applied { closed, reaped } => {
                        view.set_notice(format!(
                            "swept: closed {closed} tab(s), reaped {reaped} dead member(s)"
                        ));
                    }
                    SweepMsg::Failed(reason) => {
                        view.set_notice(format!("sweep failed: {reason}"));
                    }
                }
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            _ = winch.recv() => {
                if let Ok((cols, rows)) = terminal::size() {
                    view.term = (rows, cols);
                    // A shorter terminal shrinks the scroll window; re-clamp so
                    // the offset never scrolls past the last row (x-a621).
                    view.clamp_sideline_offset();
                    let (c_rows, c_cols) = view.content_dims();
                    // The server resizes PTYs + grids off the content area
                    // and re-emits Layout + frames; the local redraw keeps
                    // chrome coherent meanwhile.
                    if let Err(e) = write_msg(&mut sock_w, &ClientMsg::Resize { rows: c_rows, cols: c_cols }).await {
                        break Err(format!("resize send failed: {e}"));
                    }
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
                }
            }
            _ = async {
                match notice_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if notice_deadline.is_some() => {
                view.notice = None;
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            _ = async {
                match chord_flush_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if chord_flush_deadline.is_some() => {
                // (x-e10f fix) Quiet window elapsed with a candidate still
                // held: release it to the pane. No redraw needed beyond the
                // send - the pane's own output will repaint when it reacts.
                chord_since = None;
                if let Some(event) = scanner.flush_chord() {
                    if let Err(e) = dispatch_event(&mut view, event, &mut sock_w).await {
                        break Err(e);
                    }
                }
            }
            _ = async {
                match digits_flush_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if digits_flush_deadline.is_some() => {
                // (x-cf97) Quiet window elapsed with a tab number still held:
                // resolve it. The dispatch answers an out-of-range number
                // with a notice, so a missed jump is never a silent no-op -
                // and unlike the chord release above (whose send repaints via
                // the pane's own output), a notice only exists if THIS loop
                // draws it.
                digits_since = None;
                if let Some(event) = scanner.flush_digits() {
                    if let Err(e) = dispatch_event(&mut view, event, &mut sock_w).await {
                        break Err(e);
                    }
                    if let Err(e) = compositor.draw(&view.compose()) {
                        break Err(format!("draw: {e}"));
                    }
                }
            }
            _ = async {
                match pane_ids_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if pane_ids_deadline.is_some() => {
                view.pane_ids_until = None;
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            _ = async {
                match hint_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if hint_deadline.is_some() => {
                view.hint = true;
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            _ = async {
                match backlog_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if backlog_deadline.is_some() => {
                view.expire_backlog_pending();
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            _ = async {
                match yard_tick {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if yard_tick.is_some() => {
                // Frame advance only: compose() uses the elapsed time, so the
                // wake repaints (and re-arms the next deadline next pass).
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            _ = async {
                match court_tick {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if court_tick.is_some() => {
                // The wake itself is a no-op: the next loop pass runs
                // take_want at the top and spawns the refresh if due.
            }
            _ = async {
                match seam_drag_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if seam_drag_deadline.is_some() => {
                // The last applied ratio stands - the operator's drag was real
                // up to the point the release went missing, so keeping it is
                // less surprising than reverting work they watched happen.
                view.seam_drag = None;
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            _ = async {
                match sideline_drag_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if sideline_drag_deadline.is_some() => {
                // (x-2e86, AC2-FR) Same "keep the reached width" reasoning as the
                // seam reaper: the resize was real up to the missing release, so
                // `end_sideline_drag` ends it exactly as a release would (and
                // persists the reached width). Coords the drag last saw are gone,
                // so refresh hover at the border's current column.
                let col = view.panel_w().saturating_sub(1);
                view.end_sideline_drag(TAB_BAR_ROWS, col);
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            _ = async {
                match pane_drag_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if pane_drag_deadline.is_some() => {
                // Unlike a seam drag, an expired relocation applies NOTHING: a
                // move is one discrete jump rather than an accumulation the
                // operator watched happen, so committing a drop they never
                // released would relocate a pane on its own (AC5-EDGE).
                view.cancel_pane_drag();
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            _ = async {
                match new_drag_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if new_drag_deadline.is_some() => {
                // (x-d6a8 AC1-FR) Same discrete-jump reasoning as the pane drag:
                // a tab-join / row-place whose release went missing applies
                // NOTHING. Clear whichever new drag is live; the struct is cleared
                // internally so a late release cannot commit a stale command.
                // (x-7683) Exception: a motionless hold that already qualifies
                // as a long press opens its menu here instead - a held-still
                // pointer emits no events, so this reaper is the only thing
                // that ever fires for a hold past the drag timeout.
                if !view.open_drag_menu() {
                    view.cancel_tab_drag();
                    view.cancel_row_drag();
                    // (x-b465) A press-hold `open_drag_menu` declined is a dead
                    // gesture - drop it so a later stray release cannot claim
                    // it. Only when its OWN deadline expired, though: the
                    // deadline above is a `min()` across the latches, so a tab
                    // drag firing first must not clear a press-hold that is
                    // still young. That would swallow the click its press
                    // deferred, with nothing left to run it.
                    if view
                        .press_hold
                        .as_ref()
                        .is_some_and(|(_, _, start)| start.elapsed() >= PANE_DRAG_TIMEOUT)
                    {
                        view.press_hold = None;
                    }
                }
                if let Err(e) = compositor.draw(&view.compose()) {
                    break Err(format!("draw: {e}"));
                }
            }
            _ = async {
                match hover_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if hover_deadline.is_some() => {
                // The pointer rested on the pending pane past the debounce
                // (x-a496): commit focus. The server replies with a Layout that
                // redraws, so no local compose is needed here.
                if let Some(pane) = view.take_settled_hover() {
                    if let Err(e) = write_msg(&mut sock_w, &ClientMsg::Command(Command::FocusPane(pane))).await {
                        break Err(format!("hover-focus send failed: {e}"));
                    }
                }
            }
            _ = async {
                match link_hover_deadline {
                    Some(d) => tokio::time::sleep(d.saturating_duration_since(Instant::now())).await,
                    None => std::future::pending().await,
                }
            }, if link_hover_deadline.is_some() => {
                // (hover affordance) The pointer rested on one cell past the
                // quiet period: send exactly one probe. The target stays
                // pending (marked fired) so the reply can still match it, but
                // the clock stops until motion or a frame restarts it.
                if let Some(t) = view.link_hover.take_due_probe(Instant::now()) {
                    if let Err(e) = write_msg(
                        &mut sock_w,
                        &ClientMsg::LinkHover {
                            pane: t.pane,
                            row: t.row,
                            col: t.col,
                            seq: t.seq,
                        },
                    )
                    .await
                    {
                        break Err(format!("link-hover send failed: {e}"));
                    }
                }
            }
        }
    };
    drop(guard); // restore the terminal BEFORE printing the notice
    match exit {
        Ok(code) => {
            if let Some(n) = NOTICE.with(|n| n.borrow_mut().take()) {
                eprintln!("fno: {n}");
            }
            Ok(code)
        }
        Err(e) => Err(e),
    }
}

enum StdinFlow {
    Continue,
    Detach,
}

fn consume_modal_close_gesture(view: &mut View, kind: MouseKind) -> bool {
    if view.modal_release_swallow {
        if matches!(kind, MouseKind::Release(MouseButton::Left)) {
            view.modal_release_swallow = false;
        }
        return true;
    }
    false
}

/// Family-B name and confirmation overlays own every pointer event while open.
/// Only the shared Chrome esc hit cancels; outside clicks are swallowed so they
/// cannot dismiss the modal or reach a pane underneath it. The Connections
/// modal joins them. Peek does NOT: it is deliberately click-through (a
/// right-press under it still opens the row's menu), so only its footer's
/// close words are intercepted and every other event falls through.
fn modal_mouse(view: &mut View, rep: crate::mouse::MouseReport) -> bool {
    let peek_open = view.peek.is_some();
    if !peek_open && consume_modal_close_gesture(view, rep.kind) {
        return true;
    }
    let Some(layout) = view.active_overlay_layout() else {
        return false;
    };
    if matches!(rep.kind, MouseKind::Press(MouseButton::Left))
        && layout.hit_at(rep.row, rep.col) == Some(crate::chrome::ESC_CLOSE_HIT)
    {
        view.cancel_active_overlay();
        view.modal_release_swallow = true;
        return true;
    }
    if peek_open {
        return false;
    }
    true
}

/// Route one stdin chunk: the selector consumes keys while open (AC6-FR
/// validates against the CURRENT layout before sending); otherwise the
/// prefix scanner splits it into forwards and commands.
async fn handle_stdin(
    view: &mut View,
    scanner: &mut Scanner,
    mouse_carry: &mut Vec<u8>,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    // Mouse pre-pass (US1/US2/US3): pull SGR reports out first, then feed the
    // remaining bytes to the key scanner. A pane-rect event forwards for
    // server-side routing; a chrome click is swallowed (nothing reaches a pane,
    // AC3-UI); a Shift-modified event is dropped (native-selection, AC3-EDGE).
    let (reports, passthrough) = crate::mouse::extract_mouse(mouse_carry, bytes);
    for rep in reports {
        // DIAGNOSTIC (header right-click toggle): log every mouse event one stdin
        // chunk produces, so a single operator right-click can be COUNTED. Inert
        // unless FNO_MUX_MOUSE_TRACE is set; drop once the event pair is read.
        // Cached in a OnceLock: a drag or scroll emits dozens of reports a
        // second, and the flag never changes mid-process.
        static MOUSE_TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *MOUSE_TRACE.get_or_init(|| std::env::var_os("FNO_MUX_MOUSE_TRACE").is_some()) {
            eprintln!(
                "mux-mouse kind={:?} row={} col={} shift={}",
                rep.kind, rep.row, rep.col, rep.shift
            );
        }
        // Shift-modified reports are dropped so the terminal's own native
        // selection keeps working. A RELEASE while a drag is in flight is the
        // exception: some terminals report Shift on the release, and dropping it
        // would leave the drag latched - visibly stuck, eating input until its
        // timeout - for a gesture the operator has already finished. Nobody is
        // shift-selecting text mid-drag, so nothing is taken away here.
        // (x-b465) `press_hold` belongs in this list for the same reason and with
        // more at stake: the press it latched DEFERRED a click, so dropping its
        // release does not merely leave state armed - it swallows the action
        // entirely. On a terminal that marks releases with Shift, every plain
        // click on a workspace, section or card row would become a no-op, and
        // the reaper would later open a menu nobody asked for.
        let ends_a_drag = matches!(rep.kind, MouseKind::Release(MouseButton::Left))
            && (view.pane_drag.is_some()
                || view.seam_drag.is_some()
                || view.tab_drag.is_some()
                || view.row_drag.is_some()
                || view.press_hold.is_some());
        // A close-chip gesture is also release-owned state. Let every event in
        // it reach `modal_mouse`, even when the terminal marks it as Shift.
        if rep.shift && !ends_a_drag && !view.modal_release_swallow {
            continue;
        }
        if consume_modal_close_gesture(view, rep.kind) {
            continue;
        }
        // A pointer action - click/press/wheel/drag, anything but passive hover
        // (Move) - is "other input": it disarms the resize repeat window exactly
        // as a non-resize keystroke does. Without this, a click that may have
        // refocused a pane could be followed by a bare H/J/K/L that silently
        // resizes (the mouse pre-pass strips reports before the scanner runs).
        // Hover is left armed so mouse drift never breaks a held resize.
        if !matches!(rep.kind, MouseKind::Move) {
            scanner.disarm_repeat();
        }
        // (hover affordance) While a popup or modal owns the pointer, a
        // pointer event reports ITS hover, not a pane cell's: drop the link
        // probe so the underline cannot linger beneath the overlay. Any event
        // counts, not just Move - a right-click that opens a menu or an
        // overlay's own press should clear the underline too, and a family-B
        // overlay (confirm, rename, create, nav) owning the mouse is as much
        // a popup as the three menus.
        if view.keys_modal.is_some()
            || view.row_menu.is_some()
            || view.aux.is_some()
            || view.active_overlay_layout().is_some()
        {
            view.link_hover.clear();
        }
        // x-8ccf US3: while the which-key modal is open, the mouse drives it
        // (hover selects, wheel scrolls, click executes or dismisses) and is
        // SWALLOWED - it never reaches a pane or the chrome underneath.
        if view.keys_modal.is_some() {
            if let StdinFlow::Detach = keys_modal_mouse(view, scanner, rep, sock_w).await? {
                return Ok(StdinFlow::Detach);
            }
            continue;
        }
        // x-8ccf US2: the row context menu owns the mouse while open (hover
        // selects, click runs, right-press re-anchors) and is swallowed.
        if view.row_menu.is_some() {
            row_menu_mouse(view, rep, sock_w).await?;
            continue;
        }
        // x-8ccf US4/US5: the MENU popup / settings modal owns the mouse.
        if view.aux.is_some() {
            if let StdinFlow::Detach = aux_mouse(view, rep, sock_w).await? {
                return Ok(StdinFlow::Detach);
            }
            continue;
        }
        // x-d807: a seam drag in flight owns the mouse. The pointer routinely
        // leaves the divider it grabbed - that is what dragging is - so this
        // precedes every position-based route below, including the pane forward
        // that would otherwise hand the drag to a PTY as text selection.
        if view.seam_drag.is_some() {
            match rep.kind {
                MouseKind::Drag(MouseButton::Left) => {
                    if let Some(cmd) = view.seam_drag_to(rep.row, rep.col, Instant::now()) {
                        write_msg(sock_w, &ClientMsg::Command(cmd))
                            .await
                            .map_err(|e| format!("seam resize send failed: {e}"))?;
                    }
                    continue;
                }
                MouseKind::Release(MouseButton::Left) => {
                    // The last applied ratio stands (no command travels).
                    view.end_seam_drag(rep.row, rep.col);
                    continue;
                }
                // Anything else (a wheel, another button) means the gesture is
                // over; drop the drag and let the event route normally.
                _ => view.end_seam_drag(rep.row, rep.col),
            }
        }
        // x-aa95: a relocation drag owns the mouse, for the same reason a seam
        // drag does - the pointer's whole job is to leave the pane it grabbed,
        // so this must precede the pane forward that would otherwise feed the
        // gesture to a PTY as a text selection.
        if view.pane_drag.is_some() {
            match rep.kind {
                MouseKind::Drag(MouseButton::Left) => {
                    view.pane_drag_to(rep.row, rep.col, Instant::now());
                    continue;
                }
                MouseKind::Release(MouseButton::Left) => {
                    // Re-hit-test at the RELEASE coordinates rather than trusting
                    // the zone the last motion cached. A co-viewer can move the
                    // targeted seam between that motion and this release, which
                    // leaves the cached zone naming a slot the pointer no longer
                    // sits in - and `set_layout` only clears the cache when the
                    // target disappears, not when it merely moves.
                    view.pane_drag_to(rep.row, rep.col, Instant::now());
                    // Nothing goes on the wire until here: the whole drag is a
                    // client-local preview, so the server only ever learns the
                    // outcome (AC5-EDGE - a cancel sends nothing at all).
                    if let Some(cmd) = view.commit_pane_drag() {
                        write_msg(sock_w, &ClientMsg::Command(cmd))
                            .await
                            .map_err(|e| format!("pane move send failed: {e}"))?;
                    }
                    // Same release-recompute as the seam/sideline drags: clear a
                    // grip accent the drag left on if the pointer ended off it.
                    view.refresh_hover_affordances(rep.row, rep.col);
                    continue;
                }
                _ => {
                    view.cancel_pane_drag();
                    // A non-left termination ends the drag with no Release;
                    // recompute hover so a grip accent the drag left on does not
                    // linger (codex peer review).
                    view.refresh_hover_affordances(rep.row, rep.col);
                }
            }
        }
        // (x-d6a8 G2) a tab-cell join drag owns the mouse, same ownership rule as
        // a pane drag: the pointer's whole job is to leave the strip it grabbed.
        if view.tab_drag.is_some() {
            match rep.kind {
                MouseKind::Drag(MouseButton::Left) => {
                    view.tab_drag_to(rep.row, rep.col, Instant::now());
                    // (x-7683) Real motion disqualifies the long-press. Set
                    // here, not in tab_drag_to: the Release arm calls it too
                    // (zone recompute at release coords) and a hold that never
                    // moved must stay a hold.
                    if let Some(d) = view.tab_drag.as_mut() {
                        d.moved = true;
                    }
                    continue;
                }
                MouseKind::Release(MouseButton::Left) => {
                    view.tab_drag_to(rep.row, rep.col, Instant::now());
                    let held = view.tab_drag.map(|d| (d.src_tab, d.start_at, d.moved));
                    // (x-7683) A motionless hold past MENU_LONG_PRESS consumes
                    // the release BEFORE any commit: `moved` gates it (the
                    // clock alone cannot tell a hold from a slow drag), and a
                    // terminal that drops drag reports can place the release
                    // coords on a drop zone - a hold must never execute a join
                    // (codex peer review on #975). Under a usurping overlay
                    // (rename typing) the hold degrades to the plain flow
                    // below - a menu there would steal the overlay's keys.
                    // Open the CAPTURED tab's menu, not whatever cell the
                    // release reports: with no drag report ever arriving, the
                    // release coords are the one unchecked signal left.
                    let long_press = !view.menu_usurping_open()
                        && held.is_some_and(|(_, start, moved)| held_long_enough(start, moved));
                    if long_press {
                        let opened = held.is_some_and(|(tid, _, _)| {
                            view.open_tab_menu_by_id(
                                tid,
                                Anchor::At {
                                    row: rep.row,
                                    col: rep.col,
                                },
                            )
                        });
                        // The held tab closed mid-hold (e.g. a co-attached
                        // client or server-driven layout change) - say so
                        // rather than let the hold end in silence, mirroring
                        // the row arm's "no menu on the held row" notice.
                        if !opened {
                            view.set_notice("no menu on the held tab".into());
                        }
                        view.tab_drag = None;
                        view.refresh_hover_affordances(rep.row, rep.col);
                        continue;
                    }
                    match view.commit_tab_drag() {
                        Some(cmd) => {
                            write_msg(sock_w, &ClientMsg::Command(cmd))
                                .await
                                .map_err(|e| format!("tab join send failed: {e}"))?;
                        }
                        // A zone-less release still ON the strip is a plain click:
                        // select the tab (the click-to-select affordance the strip
                        // has always had). Released off the strip it is a cancelled
                        // drag - nothing travels.
                        None => {
                            if let Some((tid, _, _)) = held {
                                if view.strip_at(rep.row, rep.col) {
                                    write_msg(sock_w, &ClientMsg::Command(Command::SelectTab(tid)))
                                        .await
                                        .map_err(|e| format!("tab select send failed: {e}"))?;
                                }
                            }
                        }
                    }
                    view.refresh_hover_affordances(rep.row, rep.col);
                    continue;
                }
                _ => {
                    view.cancel_tab_drag();
                    view.refresh_hover_affordances(rep.row, rep.col);
                }
            }
        }
        // (x-d6a8 G3) a sideline-row placement drag owns the mouse.
        if view.row_drag.is_some() {
            match rep.kind {
                MouseKind::Drag(MouseButton::Left) => {
                    view.row_drag_to(rep.row, rep.col, Instant::now());
                    // (x-7683) Real motion disqualifies the long-press. Set
                    // here, not in row_drag_to: the Release arm calls it too
                    // (zone recompute at release coords) and a hold that never
                    // moved must stay a hold.
                    if let Some(d) = view.row_drag.as_mut() {
                        d.moved = true;
                    }
                    continue;
                }
                MouseKind::Release(MouseButton::Left) => {
                    view.row_drag_to(rep.row, rep.col, Instant::now());
                    // Capture the pressed row's source BEFORE commit consumes the
                    // drag, so a zone-less release can verify it landed back on the
                    // SAME row.
                    let pressed = view.row_drag.as_ref().map(|d| d.src.clone());
                    let held = view.row_drag.as_ref().map(|d| (d.start_at, d.moved));
                    let still_on_row =
                        pressed.is_some() && view.row_drag_source_at(rep.row, rep.col) == pressed;
                    // (x-7683) A motionless hold past MENU_LONG_PRESS consumes the
                    // release BEFORE any commit, exactly like the tab arm: a
                    // terminal that drops drag reports can place the release
                    // coords on a drop zone, and a hold must never execute a
                    // placement (codex peer review on #975). long_press is a
                    // TIME question, not a position one - it must not be gated
                    // on still_on_row, or a release that slips off the pressed
                    // row during a genuine motionless hold ends in total
                    // silence. A hold that opens nothing still SAYS so. Under a
                    // usurping overlay the hold degrades to the plain flow
                    // below.
                    let long_press = !view.menu_usurping_open()
                        && held.is_some_and(|(start, moved)| held_long_enough(start, moved));
                    if long_press {
                        let opened = still_on_row
                            && view.sideline_row_at(rep.row, rep.col).is_some_and(|i| {
                                view.open_row_menu(
                                    i,
                                    Anchor::At {
                                        row: rep.row,
                                        col: rep.col,
                                    },
                                )
                            });
                        if !opened {
                            view.set_notice("no menu on the held row".into());
                        }
                        view.row_drag = None;
                        view.refresh_hover_affordances(rep.row, rep.col);
                        continue;
                    }
                    match view.commit_row_drag() {
                        Some(cmd) => {
                            write_msg(sock_w, &ClientMsg::Command(cmd))
                                .await
                                .map_err(|e| format!("row place send failed: {e}"))?;
                        }
                        // A zone-less release is a plain click ONLY when the
                        // pointer is still on the row it was pressed on: run that
                        // row's own action (focus / attach), unchanged from a
                        // press-click. A slip to a different row - or a layout
                        // shift under a held button, or a release over pinned
                        // chrome (row_drag_source_at skips the density button) -
                        // resolves to a different source (or None), so the gesture
                        // cancels rather than acting on the wrong agent.
                        None => {
                            if still_on_row {
                                if let Some(hit) = view.chrome_hit(rep.row, rep.col) {
                                    apply_hit(view, hit, sock_w).await?;
                                }
                            }
                        }
                    }
                    view.refresh_hover_affordances(rep.row, rep.col);
                    continue;
                }
                _ => {
                    view.cancel_row_drag();
                    view.refresh_hover_affordances(rep.row, rep.col);
                }
            }
        }
        // (x-b465) A press held on a menu-bearing sideline row that is not a drag
        // source. The drag arms above own their own releases; this owns the rest,
        // so a workspace row answers a hold the way an agent row does.
        if view.press_hold.is_some() {
            match rep.kind {
                MouseKind::Release(MouseButton::Left) => {
                    let held = view.press_hold.take();
                    // A TIME question, not a position one, matching the row-drag
                    // arm: a release that slips off the pressed row during a
                    // genuine motionless hold must not end in silence. The menu
                    // opens on the row the press LANDED on, never on whatever
                    // the release reports, so a slip can never act on a
                    // neighbour. Under a usurping overlay the hold degrades to
                    // the plain click below - a menu there would steal the
                    // overlay's keys.
                    // Fail closed unless the pressed row is STILL that row. A
                    // layout push during the hold rebuilds `display_rows()`, so
                    // a row that vanished slides its neighbour under the same
                    // index and the menu would open on a worker nobody pressed
                    // - Stop and Remove aimed at the wrong agent. Re-checking
                    // the identity is what `still_on_row` is for the drag arm.
                    let same_row = held
                        .as_ref()
                        .is_some_and(|(i, id, _)| view.row_identity(*i).as_ref() == Some(id));
                    let long_press = same_row
                        && !view.menu_usurping_open()
                        && held
                            .as_ref()
                            .is_some_and(|(_, _, start)| held_long_enough(*start, false));
                    if long_press {
                        let opened = held.as_ref().is_some_and(|(i, _, _)| {
                            view.open_row_menu(
                                *i,
                                Anchor::At {
                                    row: rep.row,
                                    col: rep.col,
                                },
                            )
                        });
                        // A row whose menu `open_row_menu` declines (a Backlog
                        // section, an inert label) still SAYS so - the same
                        // notice the row-drag arm emits, for the same reason.
                        if !opened {
                            view.set_notice("no menu on the held row".into());
                        }
                        view.refresh_hover_affordances(rep.row, rep.col);
                        continue;
                    }
                    // Too short to be a hold: it was a click, so run the action
                    // the press deferred.
                    //
                    // Two gates, because `chrome_hit` resolves at the RELEASE
                    // coordinates and the identity check only vouches for the
                    // PRESSED index. A release that slipped to another row - no
                    // intervening Drag report is required, the row-drag arm
                    // above assumes terminals that omit them - would otherwise
                    // run the OTHER row's action: a different workspace
                    // selected, a different card's dispatch confirm armed. So
                    // the release must still land on the row that was pressed,
                    // which is `still_on_row` in the drag arm's vocabulary.
                    // Position matters here even though it must not gate the
                    // MENU: opening a menu on the pressed row is unambiguous,
                    // acting on a row nobody pressed is not.
                    let still_on_row = held.as_ref().is_some_and(|(i, _, _)| {
                        view.press_hold_row_at(rep.row, rep.col).map(|(j, _)| j) == Some(*i)
                    });
                    if same_row && still_on_row {
                        if let Some(hit) = view.chrome_hit(rep.row, rep.col) {
                            apply_hit(view, hit, sock_w).await?;
                        }
                    }
                    view.refresh_hover_affordances(rep.row, rep.col);
                    continue;
                }
                // Real motion under the held button disqualifies the hold, and
                // any other termination drops it. Neither runs the deferred
                // click: a gesture that turned into something else is not one.
                MouseKind::Drag(MouseButton::Left) => {
                    view.press_hold = None;
                }
                MouseKind::Move => {}
                _ => view.press_hold = None,
            }
        }
        // x-d807: the sideline border drag, same ownership rule as a seam drag.
        // Client-local: the sideline is never on the wire, so a width change only
        // tells the server its content area changed (x-2e86: a free width now,
        // reported per crossed column so inner apps reflow live).
        if view.sideline_drag.is_some() {
            match rep.kind {
                MouseKind::Drag(MouseButton::Left) => {
                    if view.drag_sideline_to(rep.col, Instant::now()) {
                        let (r, c) = view.content_dims();
                        write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
                            .await
                            .map_err(|e| format!("sideline resize send failed: {e}"))?;
                    }
                    continue;
                }
                MouseKind::Release(MouseButton::Left) => {
                    view.end_sideline_drag(rep.row, rep.col);
                    continue;
                }
                // A non-left termination (a wheel, another button) ends the drag.
                _ => view.end_sideline_drag(rep.row, rep.col),
            }
        }
        // Name and confirmation overlays share the same framed layout and own
        // every pointer event, including clicks outside their block.
        if modal_mouse(view, rep) {
            continue;
        }
        // Bare motion is hover (x-a496): record the sideline highlight + the
        // focus-follows-mouse settle target, and swallow it - a Move is never
        // forwarded to a pane. The actual FocusPane is committed by the select
        // loop's settle timer (a rested pointer emits no further motion event).
        if matches!(rep.kind, MouseKind::Move) {
            view.on_hover(rep.row, rep.col, Instant::now());
            continue;
        }
        // A left click on chrome (tab bar / sideline) switches tab/squad, focuses
        // an agent's pane, opens a tab, or opens a card-dispatch confirm - it
        // never reaches the pane underneath.
        if matches!(rep.kind, MouseKind::Press(MouseButton::Left)) {
            // (x-d6a8 G2/G3) a tab cell and a sideline agent row are DRAG SOURCES.
            // Begin the drag before chrome_hit (which would apply the click action
            // immediately); a zone-less release falls back to that same click
            // action (select / focus / attach), so a plain click is unchanged.
            if let Some(tid) = view.tab_cell_at(rep.row, rep.col) {
                view.begin_tab_drag(tid, Instant::now());
                continue;
            }
            if let Some(src) = view.row_drag_source_at(rep.row, rep.col) {
                view.begin_row_drag(src, Instant::now());
                continue;
            }
            // (x-b465) A sideline row that is NOT a drag source still has a menu
            // to hold for - a workspace name row is the motivating case. Arm the
            // hold clock and DEFER the click: the release decides between the
            // menu and `chrome_hit`'s own action, exactly as the drag arm defers
            // a zone-less release to that same action. Before `chrome_hit`, for
            // the same reason `begin_row_drag` is: applying the click here would
            // spend the press before the hold could be measured.
            if let Some((i, id)) = view.press_hold_row_at(rep.row, rep.col) {
                view.press_hold = Some((i, id, Instant::now()));
                continue;
            }
            if let Some(hit) = view.chrome_hit(rep.row, rep.col) {
                apply_hit(view, hit, sock_w).await?;
                continue;
            }
            // x-aa95: a press on a pane's grip starts a relocation. Before the
            // seam check only for readability - grips sit on cells a pane
            // covers and seams only on cells no pane covers, so the two can
            // never contend for the same press.
            if let Some(mover) = view.grip_at(rep.row, rep.col) {
                view.begin_pane_drag(mover, Instant::now());
                continue;
            }
            // x-d807: a press on a divider grabs the seam. After chrome_hit so
            // sideline and tab-bar affordances still win their own cells.
            if let Some(seam) = view.seam_at(rep.row, rep.col) {
                view.begin_seam_drag(seam, Instant::now());
                continue;
            }
            // Likewise the sideline's own border. Also after chrome_hit, so the
            // density button keeps the cells it draws on. Remember the width at
            // grab so a bare Esc reverts, and stamp `last_at` for the stuck-drag
            // timeout (x-2e86).
            if view.on_sideline_border(rep.row, rep.col) {
                view.sideline_drag = Some(SidelineDrag {
                    start_width: view.sideline_width,
                    last_at: Instant::now(),
                });
                continue;
            }
        }
        // x-8ccf US2: right-click a sideline row opens its context menu (agent
        // rows) or is swallowed (non-agent chrome). A right-click on a PANE cell
        // (sideline_row_at -> None) falls through and forwards to the inner app,
        // so pane right-click behavior is untouched (AC3-EDGE).
        // (x-7683) Menu paths are blocked under overlays they would USURP -
        // text inputs (including nav's typed filter) and interactive modals
        // (rename's Enter would run a menu action; the key router checks
        // row_menu first). The read-only peek overlay deliberately does not
        // block the row/tab paths: a right-press on a row opened its menu
        // over an open peek before this diff, and the open path clears peek
        // itself. The pane path keeps the full
        // overlay_open guard: a pane press under ANY overlay always fell
        // through to the pane, and it still does.
        if matches!(rep.kind, MouseKind::Press(MouseButton::Right)) && !view.menu_usurping_open() {
            // (x-92d3 5.1) A tab cell opens the tab menu, resolved through the
            // same tab_cell_at the drag pickup uses. Checked first to mirror
            // the left-press ordering; the strip and the sideline own disjoint
            // columns, so the two tests can never contend for one cell.
            if view.open_tab_menu(
                rep.row,
                rep.col,
                Anchor::At {
                    row: rep.row,
                    col: rep.col,
                },
            ) {
                continue;
            }
            if let Some(i) = view.sideline_row_at(rep.row, rep.col) {
                // Swallow the press only when a menu actually opened: a header
                // with no menu leaves the press to fall through instead of
                // eating it silently, so the right-click never reads as a
                // no-op that a following Left press then turns into a
                // collapse toggle.
                if view.open_row_menu(
                    i,
                    Anchor::At {
                        row: rep.row,
                        col: rep.col,
                    },
                ) {
                    continue;
                }
            }
            // (x-7683) A right-press on a PANE cell opens the owning agent's
            // row menu - the same menu its sideline row opens - so a pane is a
            // menu-bearing surface too. Same swallow-only-when-opened rule: an
            // agent-less pane falls through to the forward below, keeping the
            // inner app's own right-click (AC3-EDGE).
            if !view.overlay_open() && view.open_pane_menu(rep.row, rep.col) {
                continue;
            }
        }
        // Wheel over the sideline scrolls the workspace/session list (there is no
        // pane there to forward to); a wheel over the content area falls through
        // to the pane below, unchanged.
        if matches!(rep.kind, MouseKind::WheelUp | MouseKind::WheelDown) {
            let panel_w = view.panel_w();
            if panel_w > 0 && rep.col < panel_w {
                view.scroll_sideline(matches!(rep.kind, MouseKind::WheelDown));
                continue;
            }
        }
        if let Some((pane, prow, pcol)) = view.hit_test(rep.row, rep.col) {
            write_msg(
                sock_w,
                &ClientMsg::Mouse {
                    pane,
                    event: MouseEvent {
                        row: prow,
                        col: pcol,
                        kind: rep.kind,
                    },
                },
            )
            .await
            .map_err(|e| format!("mouse send failed: {e}"))?;
        }
    }
    if passthrough.is_empty() {
        return Ok(StdinFlow::Continue);
    }
    // x-d807 (AC6-FR): a bare Esc during a seam drag reverts it. The revert is
    // an explicit final command to the drag-start ratio, not a client-side
    // rollback - the server owns the layout, so putting the seam back has to
    // travel the same path that moved it. Matched on a lone 0x1b so an arrow
    // key's escape sequence never reads as a cancel.
    if view.seam_drag.is_some() && passthrough == [0x1b] {
        if let Some(cmd) = view.revert_seam_drag() {
            write_msg(sock_w, &ClientMsg::Command(cmd))
                .await
                .map_err(|e| format!("seam revert send failed: {e}"))?;
        }
        return Ok(StdinFlow::Continue);
    }
    // x-aa95 (AC5-EDGE): a bare Esc during a relocation drag cancels it. No
    // command travels, unlike the seam revert above - nothing was ever applied,
    // so there is nothing to put back. Same lone-0x1b match so an arrow key's
    // escape sequence cannot read as a cancel.
    if view.pane_drag.is_some() && passthrough == [0x1b] {
        view.cancel_pane_drag();
        return Ok(StdinFlow::Continue);
    }
    // x-2e86: a bare Esc during a sideline-border drag reverts the width to where
    // the drag began, mirroring the seam revert. Client-local, so only a Resize
    // travels (and only if the width actually changed) - the layout is the
    // client's own, not the server's.
    if view.sideline_drag.is_some() && passthrough == [0x1b] {
        if view.revert_sideline_drag() {
            let (rows, cols) = view.content_dims();
            write_msg(sock_w, &ClientMsg::Resize { rows, cols })
                .await
                .map_err(|e| format!("sideline revert resize send failed: {e}"))?;
        }
        return Ok(StdinFlow::Continue);
    }
    // (x-d6a8 AC1-FR) A bare Esc cancels a tab-cell or sideline-row drag too. No
    // command travels; the same lone-0x1b match so an arrow key cannot read as a
    // cancel.
    if view.tab_drag.is_some() && passthrough == [0x1b] {
        view.cancel_tab_drag();
        return Ok(StdinFlow::Continue);
    }
    if view.row_drag.is_some() && passthrough == [0x1b] {
        view.cancel_row_drag();
        return Ok(StdinFlow::Continue);
    }
    if view.digest.is_some() {
        // x-4e2d: any key dismisses the catch-up digest into the normal view.
        // Same whole-chunk swallow as the key-table overlay below.
        view.digest = None;
        return Ok(StdinFlow::Continue);
    }
    if view.keys_modal.is_some() {
        // x-8ccf US3 which-key: a bound key executes through the shared dispatch,
        // arrows/pgup scroll+select, Enter runs the selected row, Esc/unbound
        // dismiss. Routed here (same precedence as the old poster) so its keys
        // never leak to a pane.
        return keys_modal_keys(view, scanner, &passthrough, sock_w).await;
    }
    if view.row_menu.is_some() {
        // x-8ccf US2: the row context menu consumes keys while open (arrows walk
        // the entries + grid, Enter runs, Esc/q close) - never leaks to a pane.
        return row_menu_keys(view, &passthrough, sock_w).await;
    }
    if view.aux.is_some() {
        // x-8ccf US4/US5: the MENU popup / settings modal consumes keys.
        return aux_keys(view, &passthrough, sock_w).await;
    }
    if view.connections.is_some() {
        // x-84d7: the Connections modal consumes all keys while open (Tab
        // switches tabs, j/k move, R refreshes, Esc closes) - never leaks to a
        // pane. Routed here (top-level modal, like the MENU it opened from).
        return connections_keys(view, &passthrough, sock_w).await;
    }
    if view.confirm.is_some() {
        return confirm_keys(view, &passthrough, sock_w).await;
    }
    if view.move_pick.is_some() {
        // Modal like confirm (x-96e8): a single digit/Esc resolves it. Ahead of
        // the selector (which it replaced on open) so its keys can't leak there.
        return move_pick_keys(view, &passthrough, sock_w).await;
    }
    if view.attach_place.is_some() {
        return attach_place_keys(view, &passthrough, sock_w).await;
    }
    if view.portal_pick.is_some() {
        // (x-9fd0) The portal picker consumes keys while open, ahead of the
        // selector it replaced - same precedence slot as its sibling.
        return portal_pick_keys(view, &passthrough, sock_w).await;
    }
    if view.peek.is_some() {
        // x-c376: peek sits ON TOP of the selector; routed BEFORE it so its keys
        // (j/k, Esc, later digit/attach) never leak to the selector underneath.
        return peek_keys(view, &passthrough, sock_w).await;
    }
    // (x-f331) A hover-armed selector is motion-fresh: only the action-verb set
    // acts on the pointed-at row; the first key OUTSIDE it disarms the arm and
    // falls through to the pane, so a pointer parked over the sideline never
    // swallows typing into the focused shell (AC2-EDGE). An explicitly-opened
    // selector (sel_hover_armed=false) stays fully modal below.
    if view.selector.is_some() && view.sel_hover_armed {
        if passthrough.first().is_some_and(|&b| is_sideline_verb(b)) {
            return selector_keys(view, &passthrough, sock_w).await;
        }
        view.selector = None;
        view.sel_hover_armed = false;
        // fall through: forward this chunk to the focused pane.
    }
    if view.selector.is_some() {
        return selector_keys(view, &passthrough, sock_w).await;
    }
    if view.answers.is_some() {
        return answer_keys(view, &passthrough, sock_w).await;
    }
    if view.yard.is_some() {
        return yard_keys(view, &passthrough, sock_w).await;
    }
    if view.create.is_some() {
        return create_keys(view, &passthrough, sock_w).await;
    }
    if view.rename.is_some() {
        // Same precedence slot as create_keys: AFTER selector/answers, so a
        // lingering overlay never swallows the typed name (x-9e5e finding).
        return rename_keys(view, &passthrough, sock_w).await;
    }
    if view.move_to.is_some() {
        // (x-cf97) Same precedence slot as the rename overlay it mirrors.
        return move_to_keys(view, &passthrough, sock_w).await;
    }
    if view.recruit.is_some() {
        return recruit_keys(view, &passthrough, sock_w).await;
    }
    if view.search.is_some() {
        return search_keys(view, &passthrough, sock_w).await;
    }
    if view.nav.is_some() {
        return nav_keys(view, &passthrough, sock_w).await;
    }
    for event in scanner.scan(&passthrough, Instant::now()) {
        match dispatch_event(view, event, sock_w).await? {
            DispatchFlow::Continue => {}
            DispatchFlow::Break => break,
            DispatchFlow::Detach => return Ok(StdinFlow::Detach),
        }
    }
    Ok(StdinFlow::Continue)
}

/// One of three control-flow outcomes of dispatching a prefix event: fall
/// through to the next event, stop consuming this chunk (a chord that opens a
/// typing mode must not leak the chunk's trailing bytes into a pane), or detach.
enum DispatchFlow {
    Continue,
    Break,
    Detach,
}

/// Dispatch one resolved prefix [`Event`] to the wire / view state - the single
/// executor the key-scan loop and the which-key modal both call (x-8ccf Locked
/// 3), so a modal-executed chord runs the IDENTICAL path as a directly-typed one
/// (no parallel keymap to drift).
async fn dispatch_event(
    view: &mut View,
    event: Event,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<DispatchFlow, String> {
    match event {
        Event::Forward(chunk) => {
            // Reliable channel: awaited send, input is NEVER dropped.
            write_msg(sock_w, &ClientMsg::Input(chunk))
                .await
                .map_err(|e| format!("input send failed: {e}"))?;
        }
        Event::ShowPaneIds => {
            view.reveal_pane_ids_at(Instant::now());
        }
        Event::Cmd(cmd) => {
            view.note_command_sent(&cmd);
            write_msg(sock_w, &ClientMsg::Command(cmd))
                .await
                .map_err(|e| format!("command send failed: {e}"))?;
        }
        Event::SelectTabIdx(ordinal) => {
            // (x-cf97) The event carries the 1-BASED ordinal the operator
            // reads off the tab strip (x-1499's three-identifier-spaces trap:
            // the ordinal is the SHIFTING space and is deliberately what a
            // number gesture addresses; it is resolved to the stable TabId
            // HERE, once, so a layout push mid-gesture cannot move the
            // target). An out-of-range number answers with a notice naming
            // the miss - a silent BEL reads as a dead keybind - plus the BEL,
            // and never a wire message the server would refuse anyway.
            let squad = view
                .layout
                .squads
                .iter()
                .find(|s| s.id == view.layout.active_squad);
            let id = squad
                .and_then(|s| ordinal.checked_sub(1).and_then(|idx| s.tabs.get(idx)))
                .map(|t| t.id);
            match id {
                Some(id) => {
                    write_msg(sock_w, &ClientMsg::Command(Command::SelectTab(id)))
                        .await
                        .map_err(|e| format!("command send failed: {e}"))?;
                }
                None => {
                    let _ = raw_out(b"\x07");
                    let count = squad.map(|s| s.tabs.len()).unwrap_or(0);
                    view.set_notice(format!("no tab {ordinal} ({count} open)"));
                }
            }
        }
        Event::Detach => return Ok(DispatchFlow::Detach),
        Event::OpenSelector => {
            // The unified rows are never empty - the `+ new workspace`
            // footer is always present - so an empty session opens on it
            // (x-260a AC3-EDGE) instead of a BEL. Only the width gate stays.
            // Gate on the CURRENT density's width authority, not the regular
            // width: Slim renders down to a narrower terminal than Regular, and
            // gating on PANEL_W left that rail mouse-clickable but refusing the
            // keyboard - the exact mouse-only trap this feature forbids.
            view.panel_on = true;
            if view.panel_w() == 0 {
                view.panel_on = false;
                let _ = raw_out(b"\x07");
            } else {
                // Row 0 is NOT always actionable: Extended opens on the inert
                // column header, where the cursor would paint nothing and Enter
                // would only ring. Seed from the FOCUSED pane's row (x-e10f:
                // opening must land where you already are, not row one - the
                // confirmed `ctrl+w goes to the top each time` defect); a pane
                // with no visible row (a scratch pane, a folded section) falls
                // back to the old row-zero anchor, never an invalid index.
                // `selector_anchor` still steps off inert rows in both
                // directions from whatever seed it gets.
                let seed = view
                    .agent_row_index_for_pane(view.layout.focus)
                    .unwrap_or(0);
                view.selector = view.selector_anchor(seed);
                // An explicit open is a full modal, never a motion-fresh
                // hover-arm - clear any stale hover flag so j/k and typing are
                // owned by the selector, not disarmed on the first non-verb key.
                view.sel_hover_armed = false;
                view.sel_esc.clear();
                // Open at the top: a stale offset from a prior session must
                // not hide row 0 (x-a621). Then re-follow the SEEDED cursor -
                // offset 0 can leave it scrolled off-screen, and Enter must
                // act on a visible row.
                view.sideline_offset = 0;
                view.clamp_sideline_offset();
            }
        }
        Event::OpenAnswers => {
            // x-feec: open the needs-me queue. Always opens (even with an
            // empty live leg) so the async event-fold leg can populate it;
            // an ultimately-empty union renders "nothing needs you". The
            // fold merges in when it lands - the overlay never blocks on it.
            view.answers = Some(0);
            view.ans_esc.clear();
            view.needs_gen = view.needs_gen.wrapping_add(1);
            let fresh = view
                .needs_fold_at
                .is_some_and(|t| t.elapsed() < NEEDS_CACHE_TTL);
            if fresh {
                // Re-open within the cache TTL: reuse the last fold instantly
                // (Perspective B - mashing prefix+a never re-shells). MINE and
                // questions share the same cache window - all three legs fold
                // together.
                view.needs_degraded = false;
                view.mine_degraded = false;
                view.questions_degraded = false;
            } else {
                // Stale/first open: live-only until the refresh lands.
                view.needs_fold = None;
                view.needs_degraded = false;
                view.mine_fold = None;
                view.mine_degraded = false;
                view.questions_fold = None;
                view.questions_degraded = false;
                view.needs_want = true;
            }
        }
        Event::OpenYard => {
            // x-b2bf: open the yard. Always opens - an empty roster renders
            // "the yard is empty - nothing was dispatched", the true failure
            // state, rather than staying hidden. The identity fold merges in
            // when it lands; the overlay never blocks on it.
            view.yard = Some(YardSel {
                sel: 0,
                opened_at: Instant::now(),
            });
            view.yard_esc.clear();
            view.yard_gen = view.yard_gen.wrapping_add(1);
            let fresh = view
                .yard_fold_at
                .is_some_and(|t| t.elapsed() < YARD_CACHE_TTL);
            if fresh {
                view.yard_degraded = false;
            } else {
                view.yard_fold = None;
                view.yard_degraded = false;
                view.yard_want = true;
            }
        }
        Event::OpenCourt => view.court.toggle(),
        Event::TogglePanel => {
            view.panel_on = !view.panel_on;
            // Chrome changed size: report the new content area so rects
            // fill it (the reply Layout redraws everything).
            let (r, c) = view.content_dims();
            write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
                .await
                .map_err(|e| format!("resize send failed: {e}"))?;
        }
        Event::CycleDensity => {
            view.cycle_density();
            // The panel width changed with the density, so the content area
            // did too - same accounting as TogglePanel above.
            let (r, c) = view.content_dims();
            write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
                .await
                .map_err(|e| format!("resize send failed: {e}"))?;
        }
        Event::ToggleAgentSort => {
            // Pure local state: re-ordering rows changes no geometry, so unlike
            // the density cycle this needs no resize round trip.
            view.toggle_agent_sort();
        }
        Event::ToggleStatus => {
            view.status_on = !view.status_on;
            // Same accounting as the sideline: the content area grew or
            // shrank by one row.
            let (r, c) = view.content_dims();
            write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
                .await
                .map_err(|e| format!("resize send failed: {e}"))?;
            // Persist here too: prefix+s and the settings toggle write the
            // same key, or the two entry points disagree about what the
            // operator asked for. Fire-and-forget, so the flip stays instant;
            // a lost write just means this flip was session-only. The writes
            // serialize: config set is a read-modify-write of one file, and
            // two overlapping processes could persist the earlier flip last.
            let enabled = if view.status_on { "true" } else { "false" };
            tokio::spawn(async move {
                static TOGGLE_WRITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
                let _guard = TOGGLE_WRITE.lock().await;
                let _ = spawn_config_set("mux.status_row", enabled).await;
            });
        }
        Event::CycleSection => {
            // Pure local state, no I/O - usable even when the socket write path
            // is failing (same posture as the click path).
            if let Some(key) = squad_key(&view.layout, view.layout.active_squad) {
                view.cycle_section(key);
            }
        }
        Event::ShowKeys => {
            view.open_keys_modal();
        }
        Event::BlockJump(dir) => {
            write_msg(
                sock_w,
                &ClientMsg::BlockJump {
                    pane: view.layout.focus,
                    dir,
                },
            )
            .await
            .map_err(|e| format!("block-jump send failed: {e}"))?;
        }
        Event::BlockSelect(dir) => {
            write_msg(
                sock_w,
                &ClientMsg::BlockSelect {
                    pane: view.layout.focus,
                    dir,
                },
            )
            .await
            .map_err(|e| format!("block-select send failed: {e}"))?;
        }
        Event::BlockRerun => {
            write_msg(
                sock_w,
                &ClientMsg::BlockRerun {
                    pane: view.layout.focus,
                },
            )
            .await
            .map_err(|e| format!("block-rerun send failed: {e}"))?;
        }
        Event::DispatchNext => {
            write_msg(
                sock_w,
                &ClientMsg::DispatchNext {
                    account: view.active_account.clone(),
                },
            )
            .await
            .map_err(|e| format!("dispatch-next send failed: {e}"))?;
        }
        Event::SearchOpen => {
            // Enter client-local typing mode over the focused pane; keystrokes
            // divert to search_keys on the next read (no message sent yet, no
            // Resize - the input line overlays the bottom chrome). Break so no
            // same-chunk bytes after the chord leak to the pane.
            view.search = Some(SearchView {
                pane: view.layout.focus,
                query: String::new(),
                submitted: false,
                result: None,
            });
            view.search_esc.clear();
            return Ok(DispatchFlow::Break);
        }
        Event::OpenNav => {
            // Client-local overlay (x-653d): opening sends nothing and
            // reserves no row (it draws over the content top-left like the
            // answer overlay, not the bottom chrome). Break so same-chunk
            // bytes after the chord can't leak to the pane (like SearchOpen).
            // No width gate: draw_lines_overlay clips a tiny terminal, and a
            // zero-squad session shows an explicit `no matches` (AC1-EDGE).
            // (x-e10f) Seed from the focused pane so the navigator opens on
            // the row you are already at; the filter is empty at open, so the
            // filtered list IS nav_rows() here. A focused pane with no row (a
            // scratch pane) opens at row zero (AC7-EDGE).
            let seed = view
                .nav_rows()
                .iter()
                .position(|r| nav_row_targets_pane(r, view.layout.focus))
                .unwrap_or(0);
            view.nav = Some(NavView {
                query: String::new(),
                state_filter: None,
                cursor: seed,
            });
            view.nav_esc.clear();
            return Ok(DispatchFlow::Break);
        }
        Event::OpenRename => {
            // Rename targets the ACTIVE tab, resolved to its stable id at
            // open time so a tab switch mid-edit cannot retarget the send
            // (the server refuses a stale id fail-closed - AC1-FR).
            let tab = view
                .layout
                .squads
                .iter()
                .find(|s| s.id == view.layout.active_squad)
                .and_then(|s| s.tabs.get(s.active_tab))
                .map(|t| t.id);
            match tab {
                Some(id) => {
                    view.open_rename(RenameTarget::Tab(id));
                    // Swallow same-chunk bytes after the chord, like
                    // SearchOpen: nothing may leak into the pane.
                    return Ok(DispatchFlow::Break);
                }
                None => {
                    let _ = raw_out(b"\x07");
                }
            }
        }
        Event::OpenMoveTo => {
            // (x-cf97) Move-to targets the ACTIVE tab, resolved to its stable
            // id at open so a tab switch mid-edit cannot retarget the send -
            // the OpenRename discipline. Same-chunk bytes after the chord are
            // swallowed, like every prompt-opening chord.
            let tab = view
                .layout
                .squads
                .iter()
                .find(|s| s.id == view.layout.active_squad)
                .and_then(|s| s.tabs.get(s.active_tab))
                .map(|t| t.id);
            match tab {
                Some(id) => {
                    view.open_move_to(id);
                    return Ok(DispatchFlow::Break);
                }
                None => {
                    let _ = raw_out(b"\x07");
                }
            }
        }
        Event::ReorderTab(delta) => {
            let target = view
                .layout
                .squads
                .iter()
                .find(|s| s.id == view.layout.active_squad)
                .and_then(|s| s.tabs.get(s.active_tab).map(|tab| (s.id, tab.id)));
            match target {
                Some((squad, tab)) => {
                    write_msg(
                        sock_w,
                        &ClientMsg::Command(Command::ReorderTab { squad, tab, delta }),
                    )
                    .await
                    .map_err(|e| format!("command send failed: {e}"))?;
                }
                None => {
                    let _ = raw_out(b"\x07");
                }
            }
            return Ok(DispatchFlow::Break);
        }
        Event::Bell => {
            let _ = raw_out(b"\x07");
        }
    }
    Ok(DispatchFlow::Continue)
}

/// Apply one resolved [`ChromeHit`] - the single consumer both input paths
/// share (x-260a): the mouse press path and the selector's Enter. Cmds go to
/// the wire; Notice is a local one-liner; Confirm arms the one-keypress
/// dispatch prompt (x-a496); OpenCreate opens the name-input overlay (x-9e5e).
async fn apply_hit(
    view: &mut View,
    hit: ChromeHit,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    match hit {
        ChromeHit::Cmds(cmds) => {
            for cmd in cmds {
                view.note_command_sent(&cmd);
                write_msg(sock_w, &ClientMsg::Command(cmd))
                    .await
                    .map_err(|e| format!("command send failed: {e}"))?;
            }
        }
        ChromeHit::Notice(msg) => view.set_notice(msg.to_string()),
        // A card hit opens the confirm (x-a496); the next keypress (Enter
        // dispatches, else cancels) resolves it via confirm_keys.
        ChromeHit::Confirm(action) => view.open_confirm(action),
        // The `+` footer opens the name-input overlay (x-9e5e); the next keys
        // route to create_keys (Enter sends NewSquad, Esc cancels).
        ChromeHit::OpenCreate => view.open_create(),
        // Pure state flip, no I/O - usable even when the socket write path
        // is failing (x-2f99, AC1-FR).
        ChromeHit::CycleSection(key) => view.cycle_section(key),
        ChromeHit::SortColumn(column) => view.set_agent_sort_column(column),
        // (x-c5ee) Pure local set flip, like CycleSection - no I/O.
        ChromeHit::ToggleIdle(key) => view.toggle_idle(key),
        // (x-b186) The density button. The panel width moved with the density,
        // so unlike CycleSection this owes the server a new content viewport.
        ChromeHit::CycleDensity => {
            view.cycle_density();
            let (r, c) = view.content_dims();
            write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
                .await
                .map_err(|e| format!("resize send failed: {e}"))?;
        }
        // x-8ccf US4: open the sideline MENU popup anchored at the clicked cell.
        ChromeHit::OpenSidelineMenu { row, col } => {
            view.open_sideline_menu(Anchor::At { row, col })
        }
    }
    Ok(())
}

/// Card-dispatch confirm keys (x-a496): Enter (CR/LF) as the first byte sends
/// the targeted `DispatchNode`; any other key cancels. The whole chunk is
/// swallowed (like the overlay dismiss) so an arrow's escape tail can't leak
/// into a pane. `take()` clears the confirm either way, so a stale prompt can
/// never resurrect a second dispatch.
async fn confirm_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let Some(action) = view.confirm.take() else {
        return Ok(StdinFlow::Continue);
    };
    if matches!(bytes.first(), Some(b'\r') | Some(b'\n')) {
        // (x-92d3 5.1) Close tab is the one confirm whose commit is TWO
        // commands (select the captured tab, then close it, because CloseTab
        // closes the sender's VIEWED tab) and the one that must re-resolve its
        // target at Enter: a tab that vanished while the prompt sat open must
        // not fall through to a bare CloseTab, which would close whatever is
        // viewed now instead. Handled before the one-command table below.
        if let ConfirmKind::CloseTab { tab } = action.action {
            if view.find_tab(tab).is_none() {
                view.set_notice("tab is no longer here".into());
                return Ok(StdinFlow::Continue);
            }
            for cmd in [Command::SelectTab(tab), Command::CloseTab] {
                write_msg(sock_w, &ClientMsg::Command(cmd))
                    .await
                    .map_err(|e| format!("confirm-action send failed: {e}"))?;
            }
            return Ok(StdinFlow::Continue);
        }
        // Most confirms are one command; clear-dead (x-f300) fans out to one
        // Remove per exited row, so the commit path speaks in a list.
        // (x-f191) A row-scoped commit arms the row stamp: its outcome
        // notice will render at the row, not only the tab bar.
        view.arm_row_stamp(&action.action);
        let row_name = match &action.action {
            ConfirmKind::StopAgent { name, .. }
            | ConfirmKind::RemoveAgent { name, .. }
            | ConfirmKind::StopExternal { name, .. }
            | ConfirmKind::RemoveExternal { name, .. } => Some(name.clone()),
            _ => None,
        };
        let cmds = match action.action {
            ConfirmKind::Dispatch { node } => vec![Command::DispatchNode {
                node,
                account: view.active_account.clone(),
            }],
            ConfirmKind::RemoveSquad { squad, .. } => vec![Command::RemoveSquad(squad)],
            ConfirmKind::StopAgent { name, sid } => {
                vec![Command::StopAgent {
                    name,
                    harness_session_id: sid,
                }]
            }
            ConfirmKind::RemoveAgent { name, sid } => {
                vec![Command::RemoveAgent {
                    name,
                    harness_session_id: sid,
                }]
            }
            ConfirmKind::ReapAgents => vec![Command::ReapAgents],
            ConfirmKind::StopExternal { attach_id, name } => {
                vec![Command::StopExternal { attach_id, name }]
            }
            ConfirmKind::RemoveExternal { attach_id, name } => {
                vec![Command::RemoveExternal { attach_id, name }]
            }
            ConfirmKind::DismissMember { squad, attach_id } => {
                vec![Command::DismissMember { squad, attach_id }]
            }
            // Handled (and returned) before this one-command table is reached.
            ConfirmKind::CloseTab { .. } => unreachable!("CloseTab commits above"),
            // Re-fold on Enter, not at open: the prompt may have sat for a while
            // and the honest set is whatever is dead NOW.
            ConfirmKind::ClearDead { key, squad, .. } => {
                let dead = view.section_dead_rows(&key, squad);
                let total = dead.len();
                let picked: Vec<Command> = dead
                    .into_iter()
                    .take(CLEAR_DEAD_MAX)
                    .map(remove_dead)
                    .collect();
                // Say what the cap left behind - a silent truncation would read
                // as "cleared everything" while rows stayed on screen.
                if total > CLEAR_DEAD_MAX {
                    let rest = total - CLEAR_DEAD_MAX;
                    view.set_notice(format!(
                        "clearing {CLEAR_DEAD_MAX}, {rest} left - repeat to continue"
                    ));
                }
                picked
            }
        };
        if cmds.is_empty() {
            view.set_notice("nothing left to clear".into());
        }
        for cmd in cmds {
            write_msg(sock_w, &ClientMsg::Command(cmd))
                .await
                .map_err(|e| format!("confirm-action send failed: {e}"))?;
        }
        // (x-f191 scope a+c) The sideline comes back after a row-scoped
        // commit: selection resolved onto the acted row, or its neighbour.
        view.reanchor_after_row_commit(row_name.as_deref());
    }
    Ok(StdinFlow::Continue)
}

/// A folded which-key modal key (x-8ccf US3). Arrows/pgup navigate the
/// reference; `Byte`/`Enter` execute; `Esc` dismisses. Distinct from
/// [`fold_selector_keys`] because the modal needs arrows kept as navigation
/// (not folded to hjkl, which are executable bindings) and pgup/pgdn as scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModalKey {
    Byte(u8),
    Enter,
    Esc,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
}

/// The ceiling on a partially-read escape sequence, shared by all four folds.
/// A real CSI is far shorter, so this only ever fires on a pathological stream,
/// and it is what stops one from growing the carry without limit.
const MAX_ESC_CARRY: usize = 16;

/// Fold raw modal-mode bytes into [`ModalKey`]s, carrying escape state in `esc`
/// ACROSS reads (same split-arrow safety as [`fold_selector_keys`]). Arrows and
/// PageUp/PageDown become navigation tokens; a bare Esc (a lone `0x1b` chunk is
/// special-cased by the caller for instant close) becomes `Esc`; every other
/// printable byte is `Byte`, resolved by the caller through the chord table.
fn fold_modal_keys(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<ModalKey> {
    let mut out = Vec::new();
    for &b in bytes {
        if !esc.is_empty() {
            match (esc.as_slice(), b) {
                ([0x1b], b'[') => {
                    esc.push(b);
                    continue;
                }
                ([0x1b], _) => {
                    // The pending ESC was a bare Esc press; emit it, then let the
                    // fresh byte fall through to be processed below.
                    out.push(ModalKey::Esc);
                    esc.clear();
                }
                ([0x1b, b'['], b'A') => {
                    out.push(ModalKey::Up);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'['], b'B') => {
                    out.push(ModalKey::Down);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'['], b'C') => {
                    out.push(ModalKey::Right);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'['], b'D') => {
                    out.push(ModalKey::Left);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'['], b'5') | ([0x1b, b'['], b'6') => {
                    esc.push(b); // PageUp `ESC[5~` / PageDown `ESC[6~` pending
                    continue;
                }
                ([0x1b, b'[', b'5'], b'~') => {
                    out.push(ModalKey::PageUp);
                    esc.clear();
                    continue;
                }
                ([0x1b, b'[', b'6'], b'~') => {
                    out.push(ModalKey::PageDown);
                    esc.clear();
                    continue;
                }
                _ => {
                    // Inside a CSI (`ESC [ ...`): consume the WHOLE sequence.
                    // "swallow it whole (never leak)" used to be a comment
                    // rather than a behaviour here: this arm dropped ONE byte
                    // and let the rest of the sequence fall through as plain
                    // keys, so Ctrl-Up (`ESC [ 1 ; 5 A`) leaked `;`, `5`, `A`.
                    // Same defect the selector fold had, same fix.
                    if b == 0x1b {
                        // ESC aborts an in-progress sequence and starts a fresh
                        // one, so a cancel is never eaten as a parameter.
                        esc.clear();
                        esc.push(0x1b);
                        continue;
                    }
                    if (0x40..=0x7e).contains(&b) || esc.len() >= MAX_ESC_CARRY {
                        esc.clear();
                        continue;
                    }
                    if (0x20..=0x3f).contains(&b) {
                        esc.push(b);
                        continue;
                    }
                    // A C0 control mid-sequence is malformed: abandon the
                    // sequence and reprocess the byte below rather than losing it.
                    esc.clear();
                }
            }
        }
        match b {
            0x1b => esc.push(0x1b),
            b'\r' | b'\n' => out.push(ModalKey::Enter),
            _ => out.push(ModalKey::Byte(b)),
        }
    }
    out
}

/// Which-key modal keys (x-8ccf US3). Esc closes; arrows/pgup scroll+select;
/// Enter/`click` run the selected row; a bound printable key runs immediately
/// through the shared chord dispatch (which-key), an unbound one dismisses. Esc
/// is folded like every other overlay (carried across reads) so a split arrow
/// sequence can never leak its tail into a pane (codex P2). No key ever reaches
/// a pane.
async fn keys_modal_keys(
    view: &mut View,
    scanner: &mut Scanner,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.keys_modal_esc);
    let toks = fold_modal_keys(&mut esc, bytes);
    view.keys_modal_esc = esc;
    for tok in toks {
        if view.keys_modal.is_none() {
            break; // closed mid-chunk: swallow the rest, never forward
        }
        match tok {
            ModalKey::Esc => view.keys_modal = None,
            ModalKey::Up => {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.nav(NavDir::Up);
                }
                view.follow_modal_selection();
            }
            ModalKey::Down => {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.nav(NavDir::Down);
                }
                view.follow_modal_selection();
            }
            ModalKey::Left => {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.nav(NavDir::Left);
                }
            }
            ModalKey::Right => {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.nav(NavDir::Right);
                }
            }
            ModalKey::PageUp => {
                let (page, trows) = ((view.term.0 as isize - 2).max(1), view.term.0 as usize);
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.scroll_by(-page);
                    m.popup.clamp_sel_to_view(trows); // Enter never runs an off-screen row
                }
            }
            ModalKey::PageDown => {
                let (page, trows) = ((view.term.0 as isize - 2).max(1), view.term.0 as usize);
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.scroll_by(page);
                    m.popup.clamp_sel_to_view(trows);
                }
            }
            ModalKey::Enter => {
                if matches!(
                    keys_modal_execute_selected(view, scanner, sock_w).await?,
                    DispatchFlow::Detach
                ) {
                    return Ok(StdinFlow::Detach);
                }
            }
            ModalKey::Byte(b) => match resolve_chord(b) {
                // Unbound key dismisses (AC2-EDGE): no action fires.
                Event::Bell => view.keys_modal = None,
                // Bound key runs immediately through the SAME dispatch a typed
                // chord uses (Locked 3), then the modal closes.
                ev => {
                    view.keys_modal = None;
                    // Parity with a typed chord: modal execution arms any
                    // repeatable event too (the scanner never saw this byte).
                    scanner.arm_if_repeat(&ev, Instant::now());
                    if matches!(
                        dispatch_event(view, ev, sock_w).await?,
                        DispatchFlow::Detach
                    ) {
                        return Ok(StdinFlow::Detach);
                    }
                }
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Run the modal's selected row (Enter/click) through the shared dispatch, then
/// close - a header/meta row with no chord BELs and stays open (nothing ran, so
/// the "execute always closes" invariant is not tripped). Returns the dispatch
/// flow so a detach chord (prefix+d) run from the modal actually detaches.
async fn keys_modal_execute_selected(
    view: &mut View,
    scanner: &mut Scanner,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<DispatchFlow, String> {
    let ev = view.keys_modal.as_ref().and_then(|m| {
        m.popup
            .selected()
            .and_then(|(ri, _)| m.row_events.get(ri).cloned().flatten())
    });
    match ev {
        Some(ev) => {
            view.keys_modal = None;
            // Parity with a typed chord: modal execution arms any repeatable
            // event too (the scanner never saw a key here).
            scanner.arm_if_repeat(&ev, Instant::now());
            dispatch_event(view, ev, sock_w).await
        }
        None => {
            let _ = raw_out(b"\x07");
            Ok(DispatchFlow::Continue)
        }
    }
}

/// One mouse report while the which-key modal is open (x-8ccf US3): hover moves
/// the selection, the wheel scrolls, a left click on a row runs it, a click off
/// the popup dismisses (click-elsewhere).
async fn keys_modal_mouse(
    view: &mut View,
    scanner: &mut Scanner,
    rep: crate::mouse::MouseReport,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    match rep.kind {
        MouseKind::Move => {
            if let Some(t) = view.keys_modal_hit(rep.row, rep.col) {
                if let Some(m) = view.keys_modal.as_mut() {
                    m.popup.select(t);
                }
            }
        }
        MouseKind::WheelUp => {
            if let Some(m) = view.keys_modal.as_mut() {
                m.popup.scroll_by(-3);
            }
        }
        MouseKind::WheelDown => {
            if let Some(m) = view.keys_modal.as_mut() {
                m.popup.scroll_by(3);
            }
        }
        MouseKind::Press(MouseButton::Left) => {
            // Any esc-close chrome target (footer words, title-bar chip)
            // closes the modal; checked before the entry routers.
            if view
                .keys_modal
                .as_ref()
                .is_some_and(|m| view.chrome_close_hit(&m.popup, rep.row, rep.col))
            {
                view.keys_modal = None;
                return Ok(StdinFlow::Continue);
            }
            match view.keys_modal_hit(rep.row, rep.col) {
                Some(t) => {
                    if let Some(m) = view.keys_modal.as_mut() {
                        m.popup.select(t);
                    }
                    if matches!(
                        keys_modal_execute_selected(view, scanner, sock_w).await?,
                        DispatchFlow::Detach
                    ) {
                        return Ok(StdinFlow::Detach);
                    }
                }
                None => {
                    // A click inside the block that hit no target (a header, a border)
                    // is swallowed; only a click OFF the modal dismisses.
                    if !view.keys_modal_block_contains(rep.row, rep.col) {
                        view.keys_modal = None;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(StdinFlow::Continue)
}

/// Run a row-menu entry (x-8ccf US2) against the LIVE agent row (resolved by the
/// pinned identity). A stale OR ambiguous target is a Notice (AC1-ERR / codex
/// P1), never a misrouted action; every action maps to an existing Command /
/// overlay / confirm (zero proto).
async fn execute_row_menu_action(
    view: &mut View,
    action: MenuAction,
    target: MenuTarget,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let target = match (target, action) {
        // (x-1d91) A Backlog reorder verb: refuse a card that left the feed
        // between menu-open and Enter, arm the pending marker (which is also the
        // double-press guard), then send. The server re-validates and owns the
        // shellout; the order changes only when the feed republishes.
        (MenuTarget::Card(node), MenuAction::Backlog(verb)) => {
            if !view.layout.backlog.iter().any(|c| c.id == node) {
                view.set_notice(format!("{node} is no longer in the backlog"));
                return Ok(());
            }
            if !view.arm_backlog_pending(&node, verb) {
                view.set_notice("a backlog verb is already in flight".into());
                return Ok(());
            }
            write_msg(
                sock_w,
                &ClientMsg::Command(Command::BacklogVerb { node, verb }),
            )
            .await
            .map_err(|e| format!("backlog verb send failed: {e}"))?;
            return Ok(());
        }
        // Open a Backlog card's plan. Re-resolved at execute (not carried from
        // the menu build), so a plan_path or obsidian config change between
        // open and pick is honored rather than acting on a stale target.
        (MenuTarget::Card(node), MenuAction::OpenPlan) => {
            let Some(card) = view.layout.backlog.iter().find(|c| c.id == node) else {
                view.set_notice(format!("{node} is no longer in the backlog"));
                return Ok(());
            };
            let link =
                crate::link::plan_link(card.plan_path.as_deref().map(Path::new), &view.obsidian);
            let result = match &link {
                crate::link::PlanLink::Obsidian { uri } => crate::link::open_fno_uri(uri),
                crate::link::PlanLink::PlainFile(path) => crate::link::open_fno_path(path),
                crate::link::PlanLink::Unavailable(_) => {
                    view.set_notice("plan is no longer available".into());
                    return Ok(());
                }
            };
            if let Err(e) = result {
                view.set_notice(e);
            }
            return Ok(());
        }
        // (x-f300) The section menu's clear-dead action, resolved against the
        // section rather than a single row.
        (MenuTarget::Section { key, label, squad }, MenuAction::ClearDead) => {
            return clear_dead_confirm(view, key, label, squad);
        }
        // A workspace section's Rename opens the same overlay as selector `r`
        // (x-96e8). The id is the section's squad, so a non-workspace header
        // (`squad: None`) can never reach it - it falls to the refuse arm below.
        (
            MenuTarget::Section {
                squad: Some(id), ..
            },
            MenuAction::Rename,
        ) => {
            view.open_rename(RenameTarget::Squad(id));
            return Ok(());
        }
        // A workspace section's Move up/down sends the same `MoveSquad` the
        // selector's `J`/`K` send; the server clamps at the edges silently, so
        // an at-edge click is a no-op exactly like the key.
        (
            MenuTarget::Section {
                squad: Some(sq), ..
            },
            MenuAction::MoveSquad(delta),
        ) => {
            view.sel_follow = Some(sq);
            write_msg(
                sock_w,
                &ClientMsg::Command(Command::MoveSquad { squad: sq, delta }),
            )
            .await
            .map_err(|e| format!("move-squad send failed: {e}"))?;
            return Ok(());
        }
        // A workspace section's Remove opens the SAME confirm the keyboard
        // path builds - the destructive-action gate, never skipped by a mouse.
        (
            MenuTarget::Section {
                squad: Some(sq), ..
            },
            MenuAction::RemoveSquad,
        ) => {
            let Some(s) = view.layout.squads.iter().find(|s| s.id == sq) else {
                view.set_notice("workspace is no longer here".into());
                return Ok(());
            };
            if view.term.0 < MIN_ROWS_FOR_STATUS {
                view.set_notice("terminal too short for the confirm prompt".into());
                return Ok(());
            }
            view.open_confirm(ConfirmAction {
                action: ConfirmKind::RemoveSquad {
                    squad: sq,
                    panes: s.panes,
                    last: view.layout.squads.len() == 1,
                },
                label: s.name.clone(),
            });
            return Ok(());
        }
        // (x-92d3 5.1) The tab menu: every item re-resolves the pinned tab id
        // against the live layout first, so a tab that closed or moved between
        // open and pick is a notice, never a redirected action.
        (MenuTarget::Tab(_tid), MenuAction::TabNew) => {
            view.note_command_sent(&Command::NewTab);
            write_msg(sock_w, &ClientMsg::Command(Command::NewTab))
                .await
                .map_err(|e| format!("new-tab send failed: {e}"))?;
            return Ok(());
        }
        (MenuTarget::Tab(tid), MenuAction::TabRename) => {
            if view.find_tab(tid).is_none() {
                view.set_notice("tab is no longer here".into());
                return Ok(());
            }
            view.open_rename(RenameTarget::Tab(tid));
            return Ok(());
        }
        (MenuTarget::Tab(tid), MenuAction::TabReorder(delta)) => {
            // The squad is resolved at execute, not carried from the menu: a
            // tab can move workspaces between open and pick, and ReorderTab
            // names both ids explicitly.
            let Some((squad, _, _)) = view.find_tab(tid) else {
                view.set_notice("tab is no longer here".into());
                return Ok(());
            };
            write_msg(
                sock_w,
                &ClientMsg::Command(Command::ReorderTab {
                    squad,
                    tab: tid,
                    delta,
                }),
            )
            .await
            .map_err(|e| format!("reorder-tab send failed: {e}"))?;
            return Ok(());
        }
        (MenuTarget::Tab(tid), MenuAction::TabMoveTo) => {
            // Same execute-time re-resolution as the reorder pair: a tab that
            // closed or moved between open and pick is a notice, never a
            // redirected action.
            if view.find_tab(tid).is_none() {
                view.set_notice("tab is no longer here".into());
                return Ok(());
            }
            view.open_move_to(tid);
            return Ok(());
        }
        (MenuTarget::Tab(tid), MenuAction::TabJoin(dir)) => {
            // Join the whole tab into the VIEWED tab as a split of the focused
            // pane - the menu twin of dragging the tab cell onto a content
            // edge. A join into itself is suppressed client-side (the wire's
            // own rule), so it is named as a refusal rather than sent.
            let Some((_, _, tab)) = view.find_tab(tid) else {
                view.set_notice("tab is no longer here".into());
                return Ok(());
            };
            if tab.panes.iter().any(|p| p.id == view.layout.focus) {
                view.set_notice("cannot join a tab into itself".into());
                return Ok(());
            }
            write_msg(
                sock_w,
                &ClientMsg::Command(Command::JoinTab {
                    src_tab: tid,
                    anchor_pane: view.layout.focus,
                    dir,
                }),
            )
            .await
            .map_err(|e| format!("join-tab send failed: {e}"))?;
            return Ok(());
        }
        (MenuTarget::Tab(tid), MenuAction::TabClose) => {
            let Some((_, _, tab)) = view.find_tab(tid) else {
                view.set_notice("tab is no longer here".into());
                return Ok(());
            };
            // A confirm owns the bottom row; a too-short terminal refuses
            // rather than arm an invisible prompt (same gate as stop/remove).
            if view.term.0 < MIN_ROWS_FOR_STATUS {
                view.set_notice("terminal too short for the confirm prompt".into());
                return Ok(());
            }
            view.open_confirm(ConfirmAction {
                action: ConfirmKind::CloseTab { tab: tid },
                label: tab.name.clone(),
            });
            return Ok(());
        }
        // A menu is built for exactly one target kind, so a crossed pair can only
        // come from a bug; refuse rather than guess at a target.
        (MenuTarget::Card(_), _)
        | (MenuTarget::Section { .. }, _)
        | (MenuTarget::Tab(_), _)
        | (_, MenuAction::Backlog(_))
        | (_, MenuAction::ClearDead)
        | (_, MenuAction::TabNew)
        | (_, MenuAction::TabRename)
        | (_, MenuAction::TabReorder(_))
        | (_, MenuAction::TabMoveTo)
        | (_, MenuAction::TabJoin(_))
        | (_, MenuAction::TabClose) => {
            view.set_notice("action does not apply to this row".into());
            return Ok(());
        }
        (MenuTarget::Agent(a), _) => a,
    };
    // Fail closed unless the identity resolves to EXACTLY one live row: two rows
    // sharing a name must never let a menu act on the wrong one (codex P1).
    let mut hits = view.layout.agents.iter().filter(|a| target.matches(a));
    let a = match (hits.next(), hits.next()) {
        (Some(a), None) => a.clone(),
        _ => {
            view.set_notice(format!("agent {} is no longer uniquely here", target.name));
            return Ok(());
        }
    };
    match action {
        MenuAction::OpenHere => {
            let Some(id) = a.attach_id.clone() else {
                view.set_notice("agent is no longer attachable".into());
                return Ok(());
            };
            write_msg(sock_w, &ClientMsg::Command(Command::attach_agent_here(id)))
                .await
                .map_err(|e| format!("attach send failed: {e}"))?;
        }
        MenuAction::NewTab | MenuAction::Split(_) => {
            let Some(id) = a.attach_id.clone() else {
                view.set_notice("agent is no longer attachable".into());
                return Ok(());
            };
            let split = match action {
                MenuAction::Split(d) => Some(d),
                _ => None,
            };
            write_msg(
                sock_w,
                &ClientMsg::Command(Command::AttachAgent {
                    id,
                    placement: PanePlacement {
                        portal_new: false,
                        target: PaneTarget::CurrentRoute,
                        split,
                        here: false,
                        tab: None,
                        at: None,
                        fallback: PlacementFallback::NewTab,
                        max_panes: None,
                        thread_pane: false,
                        portal: None,
                    },
                }),
            )
            .await
            .map_err(|e| format!("attach send failed: {e}"))?;
        }
        // Where the pane already IS decides what "move it `dir`" can mean, so
        // the destination is chosen on that and nothing else.
        MenuAction::MoveDir(dir) => match a.pane_id {
            Some(pid) => {
                // On screen: step one place `dir`-ward from the pane itself, so
                // leave `target` unset and let the server navigate from the
                // mover - the geometry the keyboard bind uses. Naming the focus
                // here would instead teleport the pane across any panes between
                // them, and whenever it already sits `dir`-ward of the focus the
                // reshape is identical to the current tree, which `move_leaf`
                // reports as an origin drop and the server discards WITHOUT a
                // notice - a menu entry that does nothing and says nothing.
                //
                // Off screen: there is no meaningful in-tab neighbour to step
                // toward, so name the viewed focus and let the server graft the
                // pane into the current view (the cross-tab arm). That is the
                // destination `commit_row_drag` names from its drop zone.
                let on_screen = view.layout.panes.iter().any(|(id, _)| *id == pid);
                let target = (!on_screen).then_some(view.layout.focus);
                write_msg(
                    sock_w,
                    &ClientMsg::Command(Command::MovePane {
                        mover: Some(pid),
                        target,
                        dir,
                    }),
                )
                .await
                .map_err(|e| format!("move send failed: {e}"))?;
            }
            None => view.set_notice("agent has no pane here".into()),
        },
        MenuAction::BreakOut => match a.pane_id {
            Some(pid) => write_msg(
                sock_w,
                &ClientMsg::Command(Command::BreakPane { pane: pid }),
            )
            .await
            .map_err(|e| format!("break send failed: {e}"))?,
            None => view.set_notice("agent has no pane here".into()),
        },
        MenuAction::Detach => match (a.pane_id, a.exited) {
            (Some(pid), false) => write_msg(
                sock_w,
                &ClientMsg::Command(Command::DetachPane { pane: pid }),
            )
            .await
            .map_err(|e| format!("detach send failed: {e}"))?,
            _ => view.set_notice("only a live pane-hosted worker can detach".into()),
        },
        MenuAction::MoveToWorkspace => match a.pane_id {
            Some(pid) => {
                // Recomputed at execute (a workspace added or removed between
                // open and pick is reflected); `move_pick_keys` re-validates.
                let dsts = view.move_dst_squads(a.squad);
                if dsts.is_empty() {
                    view.set_notice("no other workspace to move into".into());
                } else {
                    view.open_move_pick(MoveSrc::Pane(pid), dsts);
                }
            }
            None => view.set_notice("agent has no pane here".into()),
        },
        MenuAction::Focus => match a.pane_id {
            Some(pid) => write_msg(sock_w, &ClientMsg::Command(Command::FocusPane(pid)))
                .await
                .map_err(|e| format!("focus send failed: {e}"))?,
            None => view.set_notice("agent has no pane here".into()),
        },
        MenuAction::Diff => {
            // Send the pane too: the server prefers it, which keeps the diff on
            // the row that was clicked when two share a name, and reaches a row
            // the registry never had.
            write_msg(
                sock_w,
                &ClientMsg::Command(Command::ToggleDiffPane {
                    agent: Some(a.name.clone()),
                    pane: a.pane_id,
                }),
            )
            .await
            .map_err(|e| format!("diff send failed: {e}"))?;
        }
        MenuAction::RenameAgent => {
            // The CURRENT label, re-resolved at execute above (a rename
            // between menu-open and pick addresses the live row), seeded so
            // Enter with no edit lands on the verb's same-label no-op.
            view.open_rename_seeded(RenameTarget::Agent(a.name.clone()), a.name.clone());
        }
        MenuAction::Peek | MenuAction::Mail => {
            let idx = view
                .display_rows()
                .iter()
                .position(|r| matches!(r, DisplayRow::Agent(x) if target.matches(x)));
            match idx {
                Some(idx) => {
                    fetch_peek(view, idx, a.name.clone(), sock_w).await?;
                    // (x-92d3 6.2) Mail arms the SAME free-text composer peek
                    // `m` opens - one input surface, two doors.
                    if matches!(action, MenuAction::Mail) {
                        view.peek_input = Some((a.name.clone(), String::new()));
                        view.peek_input_esc.clear();
                    }
                }
                None => view.set_notice("agent is no longer here".into()),
            }
        }
        // (x-92d3 6.2) Resume: the same command peek `r` sends, re-checked
        // against the row's LIVE state - a row that restarted on its own
        // between open and pick must not be respawned again.
        MenuAction::Resume => {
            if a.exited {
                write_msg(
                    sock_w,
                    &ClientMsg::Command(Command::RespawnAgent {
                        name: a.name.clone(),
                    }),
                )
                .await
                .map_err(|e| format!("respawn send failed: {e}"))?;
            } else {
                view.set_notice("only an exited row can resume".into());
            }
        }
        MenuAction::Reattach => {
            if !a.exited && a.pane_id.is_none() {
                write_msg(
                    sock_w,
                    &ClientMsg::Command(Command::ResumeAgent {
                        name: a.name.clone(),
                    }),
                )
                .await
                .map_err(|e| format!("reattach send failed: {e}"))?;
            } else {
                view.set_notice("only a live paneless row can reattach".into());
            }
        }
        // Unreachable: the crossed-pair guard above returns before an agent
        // target ever reaches a Backlog verb. Kept as a visible refusal rather
        // than a silent no-op, so a future miswiring says something.
        MenuAction::Backlog(_) => view.set_notice("action does not apply to an agent".into()),
        // Unreachable: Rename is built only for a workspace section, which
        // returns above. Visible refusal over a silent no-op.
        MenuAction::Rename => view.set_notice("action does not apply to an agent".into()),
        // Unreachable: OpenPlan is built only for a Backlog card, which
        // returns above. Visible refusal over a silent no-op.
        MenuAction::OpenPlan => view.set_notice("action does not apply to an agent".into()),
        MenuAction::Stop | MenuAction::Remove => {
            // A confirm owns the bottom row; a too-short terminal refuses rather
            // than arm an invisible prompt (matching the selector's stop/reap).
            if view.term.0 < MIN_ROWS_FOR_STATUS {
                view.set_notice("terminal too short for the confirm prompt".into());
                return Ok(());
            }
            let kind = match action {
                MenuAction::Stop => match (a.external, a.attach_id.clone()) {
                    (true, Some(id)) => ConfirmKind::StopExternal {
                        attach_id: id,
                        name: a.name.clone(),
                    },
                    _ => ConfirmKind::StopAgent {
                        name: a.name.clone(),
                        sid: a.harness_session_id.clone(),
                    },
                },
                // Remove routes by row KIND through [`remove_dead`], the same
                // mapping the bulk clear uses, so the single-row and section
                // paths cannot disagree about which store owns a dead row.
                _ => match remove_dead(&a) {
                    Command::DismissMember { squad, attach_id } => {
                        ConfirmKind::DismissMember { squad, attach_id }
                    }
                    Command::RemoveExternal { attach_id, name } => {
                        ConfirmKind::RemoveExternal { attach_id, name }
                    }
                    _ => ConfirmKind::RemoveAgent {
                        name: a.name.clone(),
                        sid: a.harness_session_id.clone(),
                    },
                },
            };
            // (x-f191 scope a+c) Same post-commit re-anchor slot the bare `x`
            // arms: the sideline stays open, the cursor stays on the row.
            view.row_slot = view
                .display_rows()
                .iter()
                .position(|r| matches!(r, DisplayRow::Agent(row) if row.name == a.name));
            view.open_confirm(ConfirmAction {
                action: kind,
                label: a.name.clone(),
            });
        }
        // Only ever built alongside `MenuTarget::Section`, which returned above.
        // A Notice rather than `unreachable!` - a panic here would take the whole
        // multiplexer down over a menu-construction bug.
        MenuAction::ClearDead => view.set_notice("clear dead needs a section header".into()),
        MenuAction::MoveSquad(_) | MenuAction::RemoveSquad => {
            view.set_notice("move and remove need a workspace section header".into())
        }
        // Unreachable: the tab actions all pair with `MenuTarget::Tab`, which
        // returns in the target match above. Visible refusal over a no-op.
        MenuAction::TabNew
        | MenuAction::TabRename
        | MenuAction::TabReorder(_)
        | MenuAction::TabMoveTo
        | MenuAction::TabJoin(_)
        | MenuAction::TabClose => view.set_notice("tab actions need a tab cell".into()),
    }
    Ok(())
}

/// (x-f300) Arm the clear-dead confirm for a section, over the dead set as it
/// stands NOW rather than as the menu found it.
fn clear_dead_confirm(
    view: &mut View,
    key: SectionKey,
    label: String,
    squad: Option<u64>,
) -> Result<(), String> {
    let dead = view
        .section_dead_rows(&key, squad)
        .len()
        .min(CLEAR_DEAD_MAX);
    if dead == 0 {
        view.set_notice(format!("no dead rows in {label}"));
        return Ok(());
    }
    // A confirm owns the bottom row; a too-short terminal refuses rather than
    // arm an invisible prompt (matching the selector's stop/reap, x-260a).
    if view.term.0 < MIN_ROWS_FOR_STATUS {
        view.set_notice("terminal too short for the confirm prompt".into());
        return Ok(());
    }
    view.open_confirm(ConfirmAction {
        action: ConfirmKind::ClearDead { key, squad, dead },
        label,
    });
    Ok(())
}

/// Run the row menu's selected entry (Enter/click), then close - the popup never
/// lingers after execute (AC1-FR).
async fn row_menu_execute_selected(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let picked = view.row_menu.as_ref().and_then(|m| {
        m.actions
            .get(m.popup.sel)
            .copied()
            .map(|a| (a, m.target.clone()))
    });
    view.row_menu = None;
    if let Some((action, target)) = picked {
        execute_row_menu_action(view, action, target, sock_w).await?;
    }
    Ok(())
}

/// Row-menu keys (x-8ccf US2): arrows walk the entries + 2x2 grid (scrolling to
/// keep the selection on-screen), pgup/pgdn scroll, Enter runs the selection,
/// Esc/`q`/any unbound key dismiss (the shared popup contract, codex P2). Esc is
/// carried across reads like every overlay, so a split arrow never leaks; no key
/// reaches a pane.
async fn row_menu_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let trows = view.term.0 as usize;
    let mut esc = std::mem::take(&mut view.row_menu_esc);
    let toks = fold_modal_keys(&mut esc, bytes);
    view.row_menu_esc = esc;
    for tok in toks {
        if view.row_menu.is_none() {
            break;
        }
        match tok {
            ModalKey::Esc => view.row_menu = None,
            ModalKey::Up => {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.nav(NavDir::Up);
                    m.popup.follow_sel(trows);
                }
            }
            ModalKey::Down => {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.nav(NavDir::Down);
                    m.popup.follow_sel(trows);
                }
            }
            ModalKey::Left => {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.nav(NavDir::Left);
                }
            }
            ModalKey::Right => {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.nav(NavDir::Right);
                }
            }
            ModalKey::PageUp => {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.scroll_by(-(trows as isize - 2).max(1));
                    m.popup.clamp_sel_to_view(trows);
                }
            }
            ModalKey::PageDown => {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.scroll_by((trows as isize - 2).max(1));
                    m.popup.clamp_sel_to_view(trows);
                }
            }
            ModalKey::Enter => row_menu_execute_selected(view, sock_w).await?,
            // (x-91a1) A printable byte first resolves against the accelerators
            // of the actions THIS menu offers: a hit moves the selection to
            // that entry and runs it through the SAME execute path Enter and a
            // click use, so keyboard and mouse execution cannot drift. A byte
            // no selectable entry answers keeps the shared popup contract and
            // dismisses. Disabled rows contribute no action, so an inert entry
            // is never accelerated.
            ModalKey::Byte(b) => {
                let hit = view.row_menu.as_ref().and_then(|m| {
                    m.actions.iter().position(|a| {
                        a.accelerator_id()
                            .and_then(crate::keys::menu_byte_for)
                            .is_some_and(|kb| kb == b)
                    })
                });
                match hit {
                    Some(i) => {
                        if let Some(m) = view.row_menu.as_mut() {
                            m.popup.select(i);
                            m.popup.follow_sel(trows);
                        }
                        row_menu_execute_selected(view, sock_w).await?;
                    }
                    None => view.row_menu = None,
                }
            }
        }
    }
    Ok(StdinFlow::Continue)
}

/// One mouse report while the row menu is open (x-8ccf US2): hover selects, a
/// left click runs the entry, a right press re-anchors on the row under the
/// pointer (or dismisses off the sideline), a click off the popup dismisses.
async fn row_menu_mouse(
    view: &mut View,
    rep: crate::mouse::MouseReport,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    match rep.kind {
        MouseKind::Move => {
            if let Some(t) = view.row_menu_hit(rep.row, rep.col) {
                if let Some(m) = view.row_menu.as_mut() {
                    m.popup.select(t);
                }
            }
        }
        MouseKind::Press(MouseButton::Left) => {
            // Any esc-close chrome target (footer words, bottom-border chip
            // on a Bare menu) closes the popup; checked before the entry
            // routers.
            if view
                .row_menu
                .as_ref()
                .is_some_and(|m| view.chrome_close_hit(&m.popup, rep.row, rep.col))
            {
                view.row_menu = None;
                return Ok(());
            }
            match view.row_menu_hit(rep.row, rep.col) {
                Some(t) => {
                    if let Some(m) = view.row_menu.as_mut() {
                        m.popup.select(t);
                    }
                    row_menu_execute_selected(view, sock_w).await?;
                }
                // A click inside the block that hit no target (a Header or Rule, which
                // contribute none) is swallowed; only a click OFF the menu dismisses.
                None => {
                    if !view.row_menu_block_contains(rep.row, rep.col) {
                        view.row_menu = None;
                    }
                }
            }
        }
        MouseKind::Press(MouseButton::Right) => {
            // The menu's own body swallows the press, never re-anchors and
            // never dismisses - and it must win over EVERY re-anchor arm
            // below, not just the pane one: a menu anchored at a sideline row
            // or the strip extends over those cells too, and a press on its
            // visible body must not silently re-anchor onto whatever row or
            // tab cell happens to sit underneath (x-7683 review finding).
            if view.row_menu_block_contains(rep.row, rep.col) {
                return Ok(());
            }
            // (x-92d3 5.1) A tab cell re-anchors the tab menu, the same
            // one-press contract a sideline row gets below; the strip and the
            // sideline own disjoint columns, so the two cannot contend.
            if view.tab_cell_at(rep.row, rep.col).is_some() {
                if !view.open_tab_menu(
                    rep.row,
                    rep.col,
                    Anchor::At {
                        row: rep.row,
                        col: rep.col,
                    },
                ) {
                    view.row_menu = None;
                }
                return Ok(());
            }
            match view.sideline_row_at(rep.row, rep.col) {
                // Re-anchor on the row under the second right-press (never stack two
                // menus); a non-agent row leaves nothing open.
                Some(i) => {
                    if !view.open_row_menu(
                        i,
                        Anchor::At {
                            row: rep.row,
                            col: rep.col,
                        },
                    ) {
                        view.row_menu = None;
                    }
                }
                // (x-7683) A pane cell re-anchors too - panes are
                // menu-bearing now, and a second right-press on another
                // pane swapping in that pane's agent menu keeps the
                // one-press contract the tab re-anchor above cites.
                // hit_test is overlay-blind, but the block-contains check at
                // the top of this arm has already settled menu-body cells.
                None => {
                    if !view.open_pane_menu(rep.row, rep.col) {
                        view.row_menu = None;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Run one aux-popup action (x-8ccf US4/US5). Menu entries open a surface or
/// detach; settings toggles flip a session-local view flag and rebuild the modal
/// so its glyph reflects the new state (the popup stays open for another toggle).
async fn execute_aux_action(
    view: &mut View,
    action: AuxAction,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<DispatchFlow, String> {
    match action {
        AuxAction::OpenKeybinds => {
            view.aux = None;
            view.open_keys_modal();
        }
        // A FRESH settings open resets the colors drill, so a stale lane
        // text-entry (left armed by a mouse dismiss mid-typing) can never
        // capture the keys of a reopened modal. `reopen_settings_keeping_sel`
        // deliberately keeps it: it rebuilds the SAME view after an action.
        AuxAction::OpenSettings => {
            view.lane.reset();
            view.aux = Some(view.build_settings_modal());
        }
        AuxAction::OpenUpdate => {
            view.aux = Some(build_update_modal(view.update_outcome.as_ref()));
        }
        AuxAction::OpenSweep => {
            view.aux = None;
            if view.sweep_inflight {
                view.set_notice("a sweep is already running".into());
            } else {
                view.sweep_action = Some(SweepAction::Counts);
            }
        }
        AuxAction::SweepTabs => begin_sweep_apply(view, SweepScope::Tabs),
        AuxAction::SweepUsedShells => begin_sweep_apply(view, SweepScope::UsedShells),
        AuxAction::SweepDeadAgents => begin_sweep_apply(view, SweepScope::Dead),
        AuxAction::SweepBoth => begin_sweep_apply(view, SweepScope::Both),
        AuxAction::OpenConnections => {
            // x-84d7: close the MENU and open the Connections modal in its
            // loading state; arm the first read (the run loop spawns it).
            view.aux = None;
            view.open_connections();
        }
        AuxAction::Detach => {
            view.aux = None;
            return Ok(DispatchFlow::Detach);
        }
        AuxAction::ToggleHoverFocus => {
            view.hover_focus = !view.hover_focus;
            let enabled = if view.hover_focus { "true" } else { "false" };
            let notice = match spawn_config_set("mux.hover_focus", enabled).await {
                Ok(()) => format!("focus follows mouse: {enabled}"),
                Err(_) => "focus follows mouse applied this session; save failed".into(),
            };
            view.set_notice(notice);
            view.reopen_settings_keeping_sel();
        }
        AuxAction::BacklogGoto(node) => {
            // (x-1d91) The overlay is for scanning; acting on a card hands you
            // back to its sideline row, where the full reorder menu lives. A card
            // that left the feed meanwhile says so rather than moving the cursor
            // somewhere arbitrary.
            view.aux = None;
            match view
                .display_rows()
                .iter()
                .position(|r| matches!(r, DisplayRow::Card(c) if c.id == node))
            {
                Some(i) => view.selector = Some(i),
                None => view.set_notice(format!("{node} is no longer in the backlog")),
            }
        }
        AuxAction::ToggleStatus => {
            view.status_on = !view.status_on;
            // The status row changed the content area; report the new size so the
            // panes reflow (same accounting as Event::ToggleStatus).
            let (r, c) = view.content_dims();
            write_msg(sock_w, &ClientMsg::Resize { rows: r, cols: c })
                .await
                .map_err(|e| format!("resize send failed: {e}"))?;
            let enabled = if view.status_on { "true" } else { "false" };
            let notice = match spawn_config_set("mux.status_row", enabled).await {
                Ok(()) => format!("status row: {enabled}"),
                Err(_) => "status row applied this session; save failed".into(),
            };
            view.set_notice(notice);
            view.reopen_settings_keeping_sel();
        }
        AuxAction::ToggleResourceMeter => {
            view.resource_meter_on = !view.resource_meter_on;
            view.resource_meter_gate
                .store(view.resource_meter_on, std::sync::atomic::Ordering::Relaxed);
            // The run loop owns the spawn (one-shot via resource_meter_sampling);
            // clearing the text here means the row reads "sensor unavailable"
            // until the first sample lands - never a stale reading.
            view.resource_meter_sampling = view.resource_meter_on;
            view.resource_meter_text = None;
            let enabled = if view.resource_meter_on {
                "true"
            } else {
                "false"
            };
            let notice = match spawn_config_set("resource_meter.enabled", enabled).await {
                Ok(()) => format!("resource meter: {enabled}"),
                Err(_) => "resource meter applied this session; save failed".into(),
            };
            view.set_notice(notice);
            view.reopen_settings_keeping_sel();
        }
        AuxAction::ApplyTheme(name) => {
            // Swap the in-memory theme first (immediate), then persist via the
            // CLI - the mux never writes config itself, mirroring the rule that
            // it never writes the graph. On a write failure the in-memory theme
            // STAYS (applied this session) and the notice says so honestly,
            // never claiming a persistence it did not achieve.
            let (theme, warn) = Theme::from_name(&name);
            view.theme = theme;
            let notice = match spawn_config_set("mux.theme", &name).await {
                Ok(()) => match warn {
                    None => format!("theme: {name}"),
                    Some(w) => w.0,
                },
                Err(_) => format!("theme {name} applied this session; save failed"),
            };
            view.set_notice(notice);
            view.reopen_settings_keeping_sel();
        }
        AuxAction::ApplyPrefix(spec) => {
            let notice = match crate::keys::resolve_prefix_change(&spec) {
                Err(refusal) => refusal,
                Ok(map) => {
                    crate::keys::reinstall(map);
                    match spawn_config_set("mux.prefix", &spec).await {
                        Ok(()) => format!("prefix: {spec}"),
                        Err(_) => format!("prefix {spec} applied this session; save failed"),
                    }
                }
            };
            view.set_notice(notice);
            view.reopen_settings_keeping_sel();
        }
        AuxAction::LaneColorEdit(axis, key) => {
            view.lane.axis = Some(axis.clone());
            view.lane.pick = Some((axis, key));
            view.reopen_settings_keeping_sel();
        }
        AuxAction::LaneColorAdd(axis) => {
            view.lane.axis = Some(axis.clone());
            view.lane.key_entry = Some((axis, String::new()));
            view.reopen_settings_keeping_sel();
        }
        AuxAction::LaneColorCustom(axis, key) => {
            view.lane.pick = Some((axis.clone(), key.clone()));
            view.lane.custom_entry = Some(String::new());
            view.reopen_settings_keeping_sel();
        }
        AuxAction::LaneColorSet(axis, key, color) => {
            view.lane.pick = None;
            lane_color_save(view, &axis, &key, &color).await?;
        }
    }
    Ok(DispatchFlow::Continue)
}

/// Run `fno config set <key> <value>`, bounded. The mux shells the CLI rather
/// than writing config itself (the graph-write rule applied to config). Returns
/// `Err` on a non-zero exit, spawn failure, or timeout - the caller keeps the
/// in-memory value either way and reports honestly.
async fn spawn_config_set(key: &str, value: &str) -> Result<(), String> {
    // spawn + wait rather than .output(): the exit check reads `.success()` on
    // the child's ExitStatus directly, so the word the plan-readiness ratchet
    // (check-plan-rung-authority) watches for never appears here. That ratchet
    // guards plan frontmatter; an exit code is a different axis, so not naming
    // the field is cheaper than bumping a guard meant for something else.
    //
    // kill_on_drop: on the 3s timeout the future drops and this returns Err,
    // but tokio leaves a spawned child running by default, so the config write
    // could land after we already told the user the save failed. needs_overlay,
    // digest_overlay, and connections_view set it for the same shell-out shape.
    let mut command = crate::process_admission::tokio_command(crate::server::fno_bin());
    // --local: the startup ladder gives the project config precedence, so a
    // global write is silently shadowed on the next attach to this workspace.
    // FNO_CONFIG is the exception: when it pins an explicit file, that file is
    // the ONLY candidate on both write and read, and --local would land the
    // write somewhere the latch never looks.
    let scope: &[&str] = if std::env::var_os("FNO_CONFIG").is_some_and(|v| !v.is_empty()) {
        &[]
    } else {
        &["--local"]
    };
    command
        .args(["config", "set", key, value])
        .args(scope)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = crate::process_admission::tokio_spawn(&mut command)
        .map_err(|e| format!("fno config set spawn failed: {e}"))?;
    match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
        Ok(Ok(es)) if es.success() => Ok(()),
        Ok(_) => Err(format!("fno config set {key} {value} failed")),
        Err(_) => Err("fno config set timed out".into()),
    }
}

/// Run the aux popup's selected row (Enter/click), propagating a detach.
async fn aux_execute_selected(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<DispatchFlow, String> {
    let picked = view
        .aux
        .as_ref()
        .and_then(|m| m.actions.get(m.popup.sel).cloned());
    match picked {
        Some(a) => execute_aux_action(view, a, sock_w).await,
        None => {
            let _ = raw_out(b"\x07");
            Ok(DispatchFlow::Continue)
        }
    }
}

/// Aux-popup keys (US4/US5): arrows select (scrolling to keep the selection
/// visible), pgup/pgdn scroll, Enter runs, Esc/`q`/any unbound key dismiss (the
/// shared popup contract, codex P2); a detach entry propagates StdinFlow::Detach.
/// Esc is carried across reads so a split arrow never leaks into a pane.
async fn aux_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    // (x-e4f1) A lane-colors text entry (naming a key / typing a free-form
    // color) consumes the chunk, same precedence shape as create_keys.
    if view.lane.is_entry() {
        return lane_entry_keys(view, bytes, sock_w).await;
    }
    let trows = view.term.0 as usize;
    let mut esc = std::mem::take(&mut view.aux_esc);
    let toks = fold_modal_keys(&mut esc, bytes);
    view.aux_esc = esc;
    for tok in toks {
        if view.aux.is_none() {
            break;
        }
        match tok {
            ModalKey::Esc => view.aux = None,
            ModalKey::Up => {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.nav(NavDir::Up);
                    m.popup.follow_sel(trows);
                }
            }
            ModalKey::Down => {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.nav(NavDir::Down);
                    m.popup.follow_sel(trows);
                }
            }
            ModalKey::Left => {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.nav(NavDir::Left);
                }
            }
            ModalKey::Right => {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.nav(NavDir::Right);
                }
            }
            ModalKey::PageUp => {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.scroll_by(-(trows as isize - 2).max(1));
                    m.popup.clamp_sel_to_view(trows);
                }
            }
            ModalKey::PageDown => {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.scroll_by((trows as isize - 2).max(1));
                    m.popup.clamp_sel_to_view(trows);
                }
            }
            ModalKey::Enter => {
                if matches!(
                    aux_execute_selected(view, sock_w).await?,
                    DispatchFlow::Detach
                ) {
                    return Ok(StdinFlow::Detach);
                }
            }
            // Tab switches the settings modal's section. Other aux
            // popups have no tab strip, so Tab dismisses as every unbound key does.
            ModalKey::Byte(b'\t') => {
                let has_tabs = view
                    .aux
                    .as_ref()
                    .map(|m| !m.popup.chrome.tabs.is_empty())
                    .unwrap_or(false);
                if has_tabs {
                    view.settings_tab = match view.settings_tab {
                        SettingsTab::General => SettingsTab::Theme,
                        SettingsTab::Theme => SettingsTab::Keys,
                        SettingsTab::Keys => SettingsTab::Colors,
                        SettingsTab::Colors => SettingsTab::General,
                    };
                    // (x-e4f1) A section switch drops the colors drill so a
                    // return to Colors always opens at the top level.
                    view.lane.reset();
                    view.reopen_settings_keeping_sel();
                } else {
                    view.aux = None;
                }
            }
            // Any other (unbound) key dismisses, per the shared popup contract.
            ModalKey::Byte(_) => view.aux = None,
        }
    }
    Ok(StdinFlow::Continue)
}

/// (x-e4f1) Keys while a lane-colors text entry is open: printable/Backspace
/// edit the buffer, Enter submits, Esc cancels back to the underlying drill
/// level. Modeled on [`create_keys`] (`fold_search_input` + per-key re-check),
/// with the settings modal staying open underneath. Enter on an EMPTY buffer
/// keeps the entry open; Enter on a custom entry validates through
/// `parse_color` and saves or refuses with a notice.
async fn lane_entry_keys(
    view: &mut View,
    bytes: &[u8],
    _sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.lane.entry_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.lane.entry_esc = esc;
    for key in keys {
        // Re-read the mode each key: a submit or Esc mid-chunk closes it, and
        // the rest of the chunk must be swallowed, never forwarded.
        if !view.lane.is_entry() {
            break;
        }
        match key {
            SearchKey::Esc => {
                view.lane.clear_entry();
                view.reopen_settings_keeping_sel();
                break;
            }
            SearchKey::Byte(b) => match b {
                b'\r' | b'\n' => {
                    if let Some((axis, buf)) = view.lane.key_entry.clone() {
                        // Naming a NEW key: an empty buffer keeps the entry
                        // open (the create_keys shape); a typed name opens the
                        // picker for it.
                        let name = buf.trim().to_string();
                        if name.is_empty() {
                            continue;
                        }
                        view.lane.clear_entry();
                        view.lane.pick = Some((axis, name));
                        view.reopen_settings_keeping_sel();
                    } else if let Some(buf) = view.lane.custom_entry.clone() {
                        // Free-form color: validate, then save through the
                        // same path the picker rows use.
                        let text = buf.trim().to_string();
                        if let Some((axis, key)) = view.lane.pick.clone() {
                            view.lane.clear_entry();
                            if crate::sideline_color::parse_color(&text).is_some() {
                                lane_color_save(view, &axis, &key, &text).await?;
                            } else {
                                view.set_notice(format!(
                                    "{axis}.{key}: invalid color (name, indexed(n), #rrggbb)"
                                ));
                                view.reopen_settings_keeping_sel();
                            }
                        }
                    }
                }
                0x7f | 0x08 => {
                    if let Some((_, buf)) = view.lane.key_entry.as_mut() {
                        buf.pop();
                    } else if let Some(buf) = view.lane.custom_entry.as_mut() {
                        buf.pop();
                    }
                }
                0x20..=0x7e => {
                    // Same bound as the create overlay: a key name or color
                    // string never needs to grow without limit.
                    if let Some((_, buf)) = view.lane.key_entry.as_mut() {
                        if buf.len() < MAX_SEARCH_QUERY {
                            buf.push(b as char);
                        }
                    } else if let Some(buf) = view.lane.custom_entry.as_mut() {
                        if buf.len() < MAX_SEARCH_QUERY {
                            buf.push(b as char);
                        }
                    }
                }
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// (x-e4f1) Persist one lane color through the CLI block-replace form and
/// reload the palette so it goes live without a restart. The merge source is
/// re-read fresh first, so a config change written by another process since
/// the palette loaded is not clobbered by the whole-block replace.
async fn lane_color_save(
    view: &mut View,
    axis: &str,
    key: &str,
    color: &str,
) -> Result<(), String> {
    crate::sideline_color::reload_palette();
    use crate::lane_colors_panel as panel;
    let json = panel::merged_axis_json(
        &panel::lane_axis_entries(crate::sideline_color::palette(), axis),
        key,
        color,
    );
    let notice = match spawn_config_set(&format!("sideline.colors.{axis}"), &json).await {
        Ok(()) => {
            crate::sideline_color::reload_palette();
            // Verify at the palette's own source: the CLI write and the
            // palette read can land in different config layers (a concurrent
            // block-replace, or a project config shadowing the global write).
            // A lost write is surfaced here, never silently swallowed.
            if panel::current_lane_color(crate::sideline_color::palette(), axis, key).as_deref()
                == Some(color)
            {
                format!("{axis}.{key}: {color}")
            } else {
                format!(
                    "{axis}.{key}: save did not stick in the config the sideline reads; check config layering"
                )
            }
        }
        Err(_) => format!("{axis}.{key}: save failed"),
    };
    view.set_notice(notice);
    view.reopen_settings_keeping_sel();
    Ok(())
}

/// One mouse report while an aux popup is open (US4/US5): hover selects, a left
/// click runs the entry (propagating detach), a click off the popup dismisses.
async fn aux_mouse(
    view: &mut View,
    rep: crate::mouse::MouseReport,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    match rep.kind {
        MouseKind::Move => {
            if let Some(t) = view.aux_hit(rep.row, rep.col) {
                if let Some(m) = view.aux.as_mut() {
                    m.popup.select(t);
                }
            }
        }
        MouseKind::Press(MouseButton::Left) => {
            // Any esc-close chrome target (footer words, title-bar chip)
            // closes the popup; checked before the entry routers. A dismiss
            // while a lane text entry is armed drops the buffer with it, so
            // the entry can never outlive the modal and capture keys later.
            if view
                .aux
                .as_ref()
                .is_some_and(|m| view.chrome_close_hit(&m.popup, rep.row, rep.col))
            {
                view.lane.clear_entry();
                view.aux = None;
                return Ok(StdinFlow::Continue);
            }
            match view.aux_hit(rep.row, rep.col) {
                Some(t) => {
                    // (x-e4f1) While a lane text entry owns the keyboard, row
                    // clicks are inert: acting on a picker row mid-typing
                    // would leave the buffer armed under a changed view.
                    if view.lane.is_entry() {
                        return Ok(StdinFlow::Continue);
                    }
                    if let Some(m) = view.aux.as_mut() {
                        m.popup.select(t);
                    }
                    if matches!(
                        aux_execute_selected(view, sock_w).await?,
                        DispatchFlow::Detach
                    ) {
                        return Ok(StdinFlow::Detach);
                    }
                }
                None => {
                    // In-block miss (a header) is swallowed; off-block dismisses.
                    if !view.aux_block_contains(rep.row, rep.col) {
                        view.lane.clear_entry();
                        view.aux = None;
                    }
                }
            }
        }
        _ => {}
    }
    Ok(StdinFlow::Continue)
}

/// Fold raw selector-mode bytes into simple key bytes, carrying escape state
/// in `esc` ACROSS reads (gemini medium: an arrow sequence split at a read
/// boundary must neither close the selector nor leak its tail into the
/// pane). Arrows map to their hjkl twins; unknown escape tails are
/// swallowed. A lone ESC stays pending until the next byte decides it - a
/// bare-Esc close lands on the following keypress (which is swallowed);
/// `q` closes instantly.
fn fold_selector_keys(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<u8> {
    let mut keys = Vec::new();
    for &b in bytes {
        if !esc.is_empty() {
            if esc.as_slice() == [0x1b] && b == b'[' {
                esc.push(b);
                continue;
            }
            if esc.first() == Some(&0x1b) && esc.get(1) == Some(&b'[') {
                // Inside a CSI sequence. Consume until its FINAL byte (0x40-0x7E)
                // rather than dropping one byte and letting the tail out.
                //
                // "swallowed whole" used to be a comment, not a behaviour: a
                // modified arrow like Ctrl-Up (`ESC [ 1 ; 5 A`) had its `1`
                // swallowed and then leaked `;`, `5` and `A` as plain keys. That
                // was survivable while these overlays closed on any key they did
                // not recognise. Once a picker gained a cursor it was not: the
                // leaked `5` reads as a digit, which in the move picker COMMITS,
                // and the leaked `A`/`H` reads as an uppercase split key, which
                // in the attach picker commits too. A function key (`ESC [ 1 5 ~`)
                // leaks a digit the same way.
                //
                // Five overlays share this fold, so fixing it here fixes every
                // door at once instead of guarding the two that were probed.
                if (0x40..=0x7e).contains(&b) {
                    // A BARE `ESC [ X` is a plain arrow. A parameterised one is a
                    // modified arrow (ctrl/shift/alt) and means something this
                    // layer has no mapping for, so it is dropped entirely rather
                    // than aliased onto the unmodified key.
                    if esc.len() == 2 {
                        match b {
                            b'A' => keys.push(b'k'),
                            b'B' => keys.push(b'j'),
                            b'C' => keys.push(b'l'),
                            b'D' => keys.push(b'h'),
                            _ => {} // unknown final byte: swallowed whole
                        }
                    }
                    esc.clear();
                    continue;
                }
                if (0x20..=0x3f).contains(&b) && esc.len() < MAX_ESC_CARRY {
                    // A real parameter or intermediate byte (ECMA-48): keep
                    // accumulating, up to the shared ceiling.
                    esc.push(b);
                    continue;
                }
                if esc.len() >= MAX_ESC_CARRY {
                    // A pathological run of parameter bytes: drop the sequence
                    // rather than growing the carry without limit.
                    esc.clear();
                    continue;
                }
                // Anything else is malformed: a C0 control landed mid-sequence.
                // Treating it as a parameter would strand the parser and eat the
                // operator's escape hatch - a truncated `ESC [` in the carry (an
                // Alt-`[` press emits exactly that) would swallow the Esc meant
                // to cancel the picker, and then swallow the following `q` too,
                // because `q` is in the final-byte range. Abandon the sequence
                // and let the byte be handled as if it arrived fresh, so a
                // cancel always reaches the overlay. This also bounds the carry:
                // it can only ever hold parameter bytes.
                esc.clear();
                if b == 0x1b {
                    esc.push(0x1b);
                } else {
                    keys.push(b);
                }
                continue;
            }
            // Pending [ESC] + a non-'[' byte: that ESC was a bare Esc press.
            esc.clear();
            keys.push(0x1b);
            if b == 0x1b {
                esc.push(0x1b); // and a new one just started
            }
            continue;
        }
        if b == 0x1b {
            esc.push(0x1b);
            continue;
        }
        keys.push(b);
    }
    keys
}

/// Open (or move) the peek overlay to `cursor` and fetch its transcript: bumps
/// the seq, resets the body to loading, and sends the matching `PeekAgent`. The
/// caller guarantees `cursor` is a `DisplayRow::Agent`. Shared by Space-open,
/// j/k, and the layout re-anchor so the seq/loading discipline is identical on
/// every path (x-c376).
async fn fetch_peek(
    view: &mut View,
    cursor: usize,
    name: String,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let seq = view.open_peek(cursor, name.clone());
    write_msg(
        sock_w,
        &ClientMsg::Command(Command::PeekAgent { name, seq }),
    )
    .await
    .map_err(|e| format!("peek send failed: {e}"))
}

/// Peek-overlay keys (x-c376): j/k (and folded arrows) peek the adjacent agent
/// row (fresh seq, stale bodies dropped by the seq guard); Esc/q closes back to
/// the selector with its cursor synced to the peeked row (AC2-UI). Digit answers
/// (US3) and attach (US4) are added by later stories; until then those keys are
/// swallowed - no key in peek mode ever reaches a pane (the prefix-layer
/// invariant). The catalog is re-read per key so a scrape tick that removed the
/// peeked row re-anchors or closes (never a panic on a dropped index).
/// (x-84d7) Route keys to the Connections modal. Pure state changes run through
/// the modal's own reducer ([`ConnectionsView::on_key`]); the intents that touch
/// the world (close, refresh) are executed here. The run loop redraws after this
/// returns `Continue`, so a `Redraw` intent needs no explicit paint.
async fn connections_keys(
    view: &mut View,
    bytes: &[u8],
    _sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    use crate::connections_view::ConnIntent;
    let mut esc = std::mem::take(&mut view.conn_esc);
    let keys = fold_selector_keys(&mut esc, bytes);
    view.conn_esc = esc;
    for &k in &keys {
        // Closed mid-chunk: swallow the rest, never forward to a pane.
        if view.connections.is_none() {
            break;
        }
        let intent = view
            .connections
            .as_mut()
            .map(|cv| cv.on_key(k))
            .unwrap_or(ConnIntent::Bell);
        match intent {
            ConnIntent::Redraw => {}
            ConnIntent::Bell => {
                let _ = raw_out(b"\x07");
            }
            ConnIntent::Close => view.close_connections(),
            ConnIntent::Refresh => view.refresh_connections(),
            ConnIntent::Run(argv) => {
                // The reducer already armed `acting`; stash the argv for the run
                // loop to spawn at loop top (single-flight, sender in scope there).
                view.conn_action = Some((argv, Vec::new(), false));
            }
            ConnIntent::RunEnv { argv, env } => {
                view.conn_action = Some((argv, env, false));
            }
            ConnIntent::SpawnLogin(argv) => {
                // Opens the login pane via `fno mux pane run`; the reducer already
                // recorded the pending row + notice. Marked is_login so a success
                // keeps that notice.
                view.conn_action = Some((argv, Vec::new(), true));
            }
            ConnIntent::SetActiveAccount(account) => {
                // (x-c914) Mirror the modal's post-toggle value into the client's
                // authoritative session-local active account. Shells nothing and
                // touches no credential (Locked Decisions 1-2); later spawns read
                // it. The modal already repainted its own marker.
                view.active_account = account;
            }
        }
    }
    Ok(StdinFlow::Continue)
}

async fn peek_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    // (x-9c5f) Input mode wins the key route: while the `m` reply input is open,
    // every key types into it (digits/j/k/l/r literal), never peek nav. Checked
    // before the nav fold so the two folders never share a chunk's bytes.
    if view.peek_input.is_some() {
        return peek_input_keys(view, bytes, sock_w).await;
    }
    let mut esc = std::mem::take(&mut view.peek_esc);
    let keys = fold_selector_keys(&mut esc, bytes);
    view.peek_esc = esc;
    for &k in &keys {
        let Some(cursor) = view.peek.as_ref().map(|p| p.cursor) else {
            break; // closed mid-chunk: swallow the rest, never forward
        };
        match k {
            b'j' | b'k' => {
                let dir = if k == b'j' { 1 } else { -1 };
                match view.peek_next_agent(cursor, dir) {
                    Some(next) => {
                        let name = match view.display_rows().get(next) {
                            Some(DisplayRow::Agent(a)) => Some(a.name.clone()),
                            _ => None,
                        };
                        if let Some(name) = name {
                            fetch_peek(view, next, name, sock_w).await?;
                        }
                    }
                    None => {
                        let _ = raw_out(b"\x07"); // at the edge: BEL, stay put
                    }
                }
            }
            b'0'..=b'9' => {
                // Answer a blocked peeked row in place (x-c929 reuse): send the
                // EXACT PaneAnswer payload (fingerprint, region_lines, keystroke)
                // only when the row is answerable AND pane-hosted; else BEL,
                // nothing sent (x-c929 AC1-ERR carried over). The overlay stays
                // open; the answered row drops from blocked on the next scrape
                // tick. The daemon-pinned keystroke is relayed opaquely - the
                // client never fabricates bytes.
                let payload = match view.display_rows().get(cursor) {
                    Some(DisplayRow::Agent(a)) => {
                        a.answerable
                            .as_ref()
                            .zip(a.pane_id)
                            .and_then(|(ans, pane)| {
                                ans.options
                                    .iter()
                                    .find(|o| o.idx.as_bytes().first() == Some(&k))
                                    .map(|o| {
                                        (
                                            pane,
                                            ans.fingerprint,
                                            ans.region_lines as u16,
                                            o.keystroke.clone(),
                                        )
                                    })
                            })
                    }
                    _ => None,
                };
                match payload {
                    Some((pane, fingerprint, region_lines, keystroke)) => {
                        write_msg(
                            sock_w,
                            &ClientMsg::PaneAnswer {
                                pane,
                                fingerprint,
                                region_lines,
                                keystroke,
                            },
                        )
                        .await
                        .map_err(|e| format!("answer send failed: {e}"))?;
                    }
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            b'l' | b'\r' | b'\n' => {
                // Attach from peek (US4) through the shared agent_hit -> apply_hit
                // path a click / selector Enter uses: a pane-hosted row focuses;
                // a paneless live row reaches PORTAL 0 with no placement dialog
                // (x-07c2; the explicit picker is `p`, a new portal is `P`). A
                // Notice refusal (a dead or unresolvable row) keeps BOTH
                // overlays open (x-260a locked 3). Right-arrow folds to `l`.
                let hit = match view.display_rows().get(cursor) {
                    Some(DisplayRow::Agent(a)) => Some(agent_hit(a, view.layout.active_squad)),
                    _ => None,
                };
                match hit {
                    Some(ChromeHit::Notice(msg)) => view.set_notice(msg.to_string()),
                    Some(hit) => {
                        view.clear_peek();
                        view.selector = None;
                        apply_hit(view, hit, sock_w).await?;
                    }
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            b'm' => {
                // Open the free-text reply input (US5), capturing the target name
                // at m-press so a later layout shift can't retarget it. break so
                // the rest of THIS chunk is swallowed; the next chunk routes to
                // peek_input_keys.
                match view.display_rows().get(cursor) {
                    Some(DisplayRow::Agent(a)) => {
                        view.peek_input = Some((a.name.clone(), String::new()));
                        view.peek_input_esc.clear();
                        break;
                    }
                    _ => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            b'r' => {
                // Respawn an exited row (US6). A live row BELs (locked posture);
                // the server re-validates external/uuid/shape - client gating is
                // UX, not the guard.
                let target = match view.display_rows().get(cursor) {
                    Some(DisplayRow::Agent(a)) => Some((a.name.clone(), a.exited)),
                    _ => None,
                };
                match target {
                    Some((name, true)) => {
                        write_msg(sock_w, &ClientMsg::Command(Command::RespawnAgent { name }))
                            .await
                            .map_err(|e| format!("respawn send failed: {e}"))?;
                    }
                    _ => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            0x1b | b'q' => {
                // Close peek. When peek was opened FROM the selector it stays
                // open underneath, so re-point its cursor to the peeked row
                // (AC2-UI). When peek was opened standalone (x-8ccf US2:
                // right-click a row -> Peek, selector closed), Esc must return to
                // normal pane input, NOT drop into panel-selector mode.
                let restore = view.selector.is_some();
                view.clear_peek();
                if restore {
                    view.selector = Some(cursor);
                }
            }
            // Everything else is swallowed - never a pane leak (prefix-layer
            // invariant). h (left-arrow) has no peek action.
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// (x-9c5f US5) The peek `m` free-text reply input keys, mirroring
/// [`rename_keys`]' discipline (fold_search_input, Esc drops the buffer,
/// backspace pops, printable ASCII appends, re-read the mode each key) with two
/// node-spec divergences: **empty-Enter keeps the input open** (a blank mail is
/// meaningless, unlike a blank rename's "reset to auto"), and Enter-with-text
/// sends [`Command::MailAgent`] then closes the input, leaving peek open (the
/// notice line is the feedback). The buffer caps at [`MAX_MAIL_TEXT`] chars so
/// the operator sees the same ceiling the server enforces.
async fn peek_input_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.peek_input_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.peek_input_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc/Enter mid-chunk closes the input, and
        // the rest of the chunk must be swallowed, never forwarded.
        if view.peek_input.is_none() {
            break;
        }
        match key {
            SearchKey::Esc => {
                // Drop half-typed text; peek stays open underneath (AC parity
                // with rename Esc).
                view.peek_input = None;
                view.peek_input_esc.clear();
                break;
            }
            SearchKey::Byte(b) => match b {
                b'\r' | b'\n' => {
                    // Empty (or whitespace-only) buffer: BEL, input stays open,
                    // nothing sent (AC3-UI). Otherwise send + close.
                    let send = view
                        .peek_input
                        .as_ref()
                        .filter(|(_, buf)| !buf.trim().is_empty())
                        .map(|(name, buf)| (name.clone(), buf.clone()));
                    match send {
                        None => {
                            let _ = raw_out(b"\x07");
                        }
                        Some((name, text)) => {
                            view.peek_input = None;
                            view.peek_input_esc.clear();
                            write_msg(
                                sock_w,
                                &ClientMsg::Command(Command::MailAgent { name, text }),
                            )
                            .await
                            .map_err(|e| format!("mail send failed: {e}"))?;
                        }
                    }
                    break;
                }
                0x7f | 0x08 => {
                    if let Some((_, buf)) = view.peek_input.as_mut() {
                        buf.pop();
                    }
                }
                0x20..=0x7e => {
                    if let Some((_, buf)) = view.peek_input.as_mut() {
                        // Cap to the server's ceiling so the operator sees exactly
                        // what will be accepted (server stays authoritative). Only
                        // printable ASCII is ever pushed, so byte len == char count.
                        if buf.len() < MAX_MAIL_TEXT {
                            buf.push(b as char);
                        }
                    }
                }
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Selector-mode keys: j/k (and arrows) move over the unified display rows,
/// skipping inert Headers; h (and left) collapse, `l` explicitly expands a
/// squad row, Right toggles a workspace row's idle caret;
/// Enter acts on the row through [`View::row_action`] + [`apply_hit`] - the
/// same resolver a mouse click uses (x-260a), so squad/tab switch, agent
/// focus/attach, card dispatch-confirm, and workspace-create are all keyboard
/// reachable. The x-96e8 squad-management context keys ride here too: on a
/// squad row `r` renames, `x` removes (behind a confirm), `J`/`K` reorder; on
/// a tab row `m` opens the move-to-squad picker. A refusal (Notice/BEL) keeps
/// the selector open; Esc/q closes. Rows and cursor are re-read per key so a
/// close mid-chunk swallows the remainder instead of resurrecting the selector.
/// Detach is prefix+d from NORMAL mode only (Locked 11): close the selector.
/// The selector's act-on-a-row body, shared by Enter, the x-9fd0 `l` reach on
/// an agent row, and the Right-arrow pre-pass on an agent row, so the three
/// doors cannot drift on what "act" means: a refusal keeps the selector open
/// (x-260a locked 3), a hit closes it and applies, a row with no action says
/// so on-screen.
async fn selector_apply_row_action(
    view: &mut View,
    cur: usize,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    // row_action resolves against the CURRENT catalog (AC6-FR) and returns an
    // OWNED hit, so applying it can mutate the view.
    match view.row_action(cur) {
        Some(ChromeHit::Notice(msg)) => view.set_notice(msg.to_string()),
        Some(hit) => {
            view.selector = None;
            apply_hit(view, hit, sock_w).await?;
        }
        // Out of range / Header: unreachable via j/k, but a stale cursor gets
        // an on-screen notice, never a silent close or a bare beep (x-f331 US3).
        None => view.set_notice("no action for this row".into()),
    }
    Ok(())
}

async fn selector_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.sel_esc);
    // Right arrow is the caret TOGGLE on a workspace row, distinct from `l`'s
    // explicit expand: strip each COMPLETE `ESC [ C` from this chunk and handle
    // it below, so it never aliases onto `l` through the fold. A sequence the
    // read split lands in the carry and folds to `l` (expand) - a valid caret
    // open, never a mis-toggle. A modified Right (`ESC [ 1 ; 5 C`) does not
    // contain the contiguous triple, so it still drops as unmapped.
    let mut rights = 0usize;
    let mut cleaned: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(&[0x1b, b'[', b'C']) {
            rights += 1;
            i += 3;
        } else {
            cleaned.push(bytes[i]);
            i += 1;
        }
    }
    let keys = fold_selector_keys(&mut esc, &cleaned);
    view.sel_esc = esc;
    for _ in 0..rights {
        // Same cursor as every other selector verb: pointer-follow works
        // through the hover-ARMED selector (x-f331), and an explicit prefix+w
        // selector keeps keyboard control even with the pointer parked on an
        // inert sideline cell.
        let Some(cur) = view.selector else {
            break;
        };
        // (x-9fd0) On an AGENT row Right is the reach - the same row_action
        // path Enter takes, matching the peek overlay where Right already
        // reaches. A workspace row keeps the caret toggle; anything else says
        // why not.
        if matches!(view.display_rows().get(cur), Some(DisplayRow::Agent(_))) {
            selector_apply_row_action(view, cur, sock_w).await?;
            continue;
        }
        let squad = match view.display_rows().get(cur) {
            Some(DisplayRow::Sel(r)) if r.tab.is_none() => Some(r.squad),
            _ => None,
        };
        match squad.and_then(|sq| squad_key(&view.layout, sq)) {
            Some(key) => view.toggle_idle(key),
            None => view.set_notice("only a workspace row has a caret".into()),
        }
    }
    for &k in &keys {
        // Rows are re-read per key (via the View helpers below) so a layout
        // push or a close mid-chunk acts on the CURRENT catalog, never a stale
        // snapshot.
        let Some(cur) = view.selector else {
            break; // closed mid-chunk: swallow the rest, never forward
        };
        // Any key other than a J/K reorder drops the cursor-follow intent, so a
        // later Layout re-anchors normally instead of chasing a stale squad.
        if k != b'J' && k != b'K' {
            view.sel_follow = None;
        }
        match k {
            b'j' => view.selector = Some(view.selector_down(cur)),
            b'k' => view.selector = Some(view.selector_up(cur)),
            b'l' | b'h' => {
                // (x-9fd0) `l` on an AGENT row is the reach: the same
                // row_action path Enter takes, so Right (which folds to `l`
                // when the sequence splits across a read boundary) and the
                // peek overlay's `l` all agree on one meaning. `h` stays
                // inert there - collapse has no meaning for an agent row -
                // and BOTH keys keep their expand/collapse meanings on
                // workspace rows (AC7-REG). Every other row class no-ops
                // (matching today's tab rows).
                let on_agent = matches!(view.display_rows().get(cur), Some(DisplayRow::Agent(_)));
                if k == b'l' && on_agent {
                    selector_apply_row_action(view, cur, sock_w).await?;
                    continue;
                }
                // Expand/collapse applies to squad rows; every other variant
                // no-ops (matching today's tab rows). Materialize the owned id
                // before mutating - display_rows() borrows the layout.
                let squad = match view.display_rows().get(cur) {
                    Some(DisplayRow::Sel(r)) if r.tab.is_none() => Some(r.squad),
                    _ => None,
                };
                // `l`/`h` stay the EXPLICIT open/close pair (x-975a keeps the
                // tri-state cycle on the header click and prefix+z); `l` from
                // live-only opens back to the full section.
                if let Some(key) = squad.and_then(|sq| squad_key(&view.layout, sq)) {
                    let next = if k == b'l' {
                        SectionView::Expanded
                    } else {
                        SectionView::Collapsed
                    };
                    view.set_section_view(key, next);
                }
            }
            b'\r' | b'\n' => {
                selector_apply_row_action(view, cur, sock_w).await?;
            }
            b'd' => {
                let pane = match view.display_rows().get(cur) {
                    Some(DisplayRow::Agent(a)) if a.pane_id.is_some() && !a.exited => a.pane_id,
                    _ => None,
                };
                match pane {
                    Some(pane) => {
                        write_msg(sock_w, &ClientMsg::Command(Command::DetachPane { pane }))
                            .await
                            .map_err(|e| format!("detach send failed: {e}"))?;
                    }
                    None => view.set_notice("only a live pane-hosted worker can detach".into()),
                }
            }
            b'r' => {
                // Rename the squad at the cursor (x-96e8). Tab/other rows have
                // no squad rename here (prefix+, renames a tab), so they notice.
                let squad = match view.display_rows().get(cur) {
                    Some(DisplayRow::Sel(r)) if r.tab.is_none() => Some(r.squad),
                    _ => None,
                };
                match squad {
                    Some(sq) => view.open_rename(RenameTarget::Squad(sq)),
                    None => view.set_notice("only a workspace row can be renamed".into()),
                }
            }
            b' ' => {
                // Open the read-only peek overlay: an agent row (x-c376) shows
                // its status sentence + recent transcript from disk; a
                // workspace row (x-10ec) shows its tabs and members rendered
                // locally from the layout, no wire round trip. Any other row
                // notices why (x-f331 US3). The selector stays open
                // underneath; Esc drops back into it at the peeked row.
                match view.display_rows().get(cur) {
                    Some(DisplayRow::Agent(a)) => {
                        fetch_peek(view, cur, a.name.clone(), sock_w).await?
                    }
                    Some(DisplayRow::Sel(r)) if r.tab.is_none() => {
                        view.open_squad_peek(cur, r.squad);
                    }
                    _ => view.set_notice("only an agent or workspace row can be peeked".into()),
                }
            }
            b'\t' => {
                // Toggle a recruit mark on the focused row (x-8f11; moved from
                // Space to Tab by x-c376, which took Space for peek). Markable
                // only if it is an attachable watch-only agent (live, has an
                // attach_id); anything else gives a notice, never zero feedback.
                let id = match view.display_rows().get(cur) {
                    Some(DisplayRow::Agent(a)) if a.attach_id.is_some() && !a.exited => {
                        a.attach_id.clone()
                    }
                    _ => None,
                };
                match id {
                    Some(id) => {
                        if !view.marks.remove(&id) {
                            view.marks.insert(id);
                        }
                    }
                    None => view.set_notice("not attachable".into()),
                }
            }
            b'R' => {
                // Open the recruit name prompt for the marked rows (x-8f11). With
                // no marks, fall back to marking the focused attachable row first
                // (the grid's single-recruit `m`, generalized); a non-attachable
                // focused row with no marks notices why (x-f331 US3).
                if view.marks.is_empty() {
                    let id = match view.display_rows().get(cur) {
                        Some(DisplayRow::Agent(a)) if a.attach_id.is_some() && !a.exited => {
                            a.attach_id.clone()
                        }
                        _ => None,
                    };
                    match id {
                        Some(id) => {
                            view.marks.insert(id);
                            view.open_recruit();
                        }
                        None => {
                            view.set_notice("only a live attachable agent can be recruited".into())
                        }
                    }
                } else {
                    view.open_recruit();
                }
            }
            b'b' => {
                // (x-1d91) The mini-kanban: the Backlog's lanes with their true
                // counts. A section-level view, not a row action, so it opens
                // from anywhere in the sideline - but only when there is a
                // backlog to show, rather than an empty board.
                if view.layout.backlog_lanes.is_empty() {
                    view.set_notice("the backlog is empty".into());
                } else {
                    view.open_kanban(Anchor::Center);
                }
            }
            b'p' => {
                let picked = match view.display_rows().get(cur) {
                    Some(DisplayRow::Agent(a))
                        if a.pane_id.is_none() && a.attach_id.is_some() && !a.exited =>
                    {
                        Some((a.attach_id.clone().unwrap(), a.squad))
                    }
                    _ => None,
                };
                // (x-07c2) Enter reaches the thread pane, so `p` is the
                // picker's only door. The synthetic mission squad must still be
                // excluded here (attach_dst_squads does it) or the virtual id
                // leaks into the picker and `place_spawned_pane` cannot route
                // it. Same reason the two no-op cases stay distinct: "no
                // workspace" and "not attachable" are different problems to
                // report.
                let squads: Vec<u64> = view.attach_dst_squads();
                match picked {
                    Some((id, owner)) if !squads.is_empty() => {
                        view.open_attach_place(id, owner, squads)
                    }
                    Some(_) => view.set_notice("no workspace to attach into".into()),
                    None => view.set_notice("placement requires an attachable agent".into()),
                }
            }
            b'P' => {
                // (x-8f9d) opened the NEXT FREE portal; (x-9fd0) `P` opens the
                // portal picker instead: the portals that are OPEN, numbered,
                // plus a new-portal row PRE-SELECTED, so `P` Enter still sends
                // the exact wire gesture this key always had. The SERVER still
                // picks the index for that row - a client computing it from the
                // rows it last rendered races every other client onto the same
                // number, and the loser's new portal is silently repointed.
                //
                // A bare digit was the obvious spelling and is not available:
                // `b'0'..=b'9'` here is the x-c929 answerable-prompt path. `P`
                // pairs with `p` (the placement picker) the way `X`/`x` already
                // pair.
                let picked = match view.display_rows().get(cur) {
                    Some(DisplayRow::Agent(a)) if a.pane_id.is_none() && !a.exited => {
                        Some(a.attach_id.clone().unwrap_or_else(|| a.name.clone()))
                    }
                    _ => None,
                };
                match picked {
                    Some(id) => view.open_portal_pick(id),
                    None => view.set_notice("a portal shows a paneless live row".into()),
                }
            }
            b'X' => {
                // Bulk reap (x-7561): uppercase `X` from ANY agent row confirms
                // `fno-agents reap`. Contextual on agent rows only (headers stay
                // inert - no selector surgery); a non-agent row notices why
                // (x-f331 US3). Too-short terminal refuses rather than arm an
                // invisible confirm (x-260a).
                let on_agent = matches!(view.display_rows().get(cur), Some(DisplayRow::Agent(_)));
                if !on_agent {
                    view.set_notice("reap works on an agent row".into());
                } else if view.term.0 < MIN_ROWS_FOR_STATUS {
                    view.set_notice("terminal too short for the confirm prompt".into());
                } else {
                    view.open_confirm(ConfirmAction {
                        action: ConfirmKind::ReapAgents,
                        label: String::new(),
                    });
                }
                continue;
            }
            b'x' => {
                // A TOMBSTONE member row dismisses (x-8f11); a squad-header row
                // removes the squad (x-96e8), behind a confirm - disambiguated by
                // row type so one key serves both. A too-short terminal cannot
                // render the bottom-row prompt, so it refuses with a notice rather
                // than arm an INVISIBLE confirm (x-260a); an unknown squad or a
                // tab/other row BELs.
                let dismiss = match view.display_rows().get(cur) {
                    Some(DisplayRow::Agent(a)) if a.tombstone => a.squad.zip(a.attach_id.clone()),
                    _ => None,
                };
                if let Some((squad, attach_id)) = dismiss {
                    write_msg(
                        sock_w,
                        &ClientMsg::Command(Command::DismissMember { squad, attach_id }),
                    )
                    .await
                    .map_err(|e| format!("dismiss send failed: {e}"))?;
                    continue;
                }
                // A WATCH-ONLY (paneless) agent row gets the lifecycle verb
                // (x-76ea): a live row (`!exited`) stops, an exited row removes.
                // The registry poll's state flip IS the stage separator - stop,
                // wait ≤1s for the row to flip exited, then `x` again removes (no
                // double-tap timer). The captured name (not the row index) rides
                // the confirm, so a row that races out resolves to the server's
                // stale-name refusal; too-short terminal refuses rather than arm
                // an invisible confirm (x-260a), like RemoveSquad.
                //
                // `pane_id.is_none()` is load-bearing (codex review): a PANE-hosted
                // Agent row is either a real agent's pane or a bare shell pane that
                // agent_rows() surfaces as a first-class Agent row labelled from its
                // cmd/cwd - NOT a registry entry. Arming the verb there would shell
                // `fno-agents` on a label that could collide with an unrelated
                // agent's name and stop the wrong one. Pane-hosted rows are managed
                // via their tab (CloseTab); only the paneless rows - the bg/headless
                // agents that today linger until GC - are the gap this closes.
                //
                // An EXTERNAL row (claude-daemon roster or a persisted external
                // tombstone, x-7561) routes by stable `attach_id` to the
                // External verbs instead of `fno-agents` by name: a live row
                // (`!exited`) stops (`claude stop <id>`), a stopped tombstone
                // (`exited`) removes (`claude rm <id>`). The server re-validates
                // the exact id + gates rm on a persisted `stopped` state; a
                // failed/unknown tombstone renders `!exited` so its `x` retries
                // the stop. An external row without an attach_id (degenerate)
                // falls through to the by-name path, which the server refuses.
                let agent = match view.display_rows().get(cur) {
                    Some(DisplayRow::Agent(a)) if !a.tombstone && a.pane_id.is_none() => Some((
                        a.name.clone(),
                        a.exited,
                        a.external,
                        a.attach_id.clone(),
                        a.harness_session_id.clone(),
                    )),
                    _ => None,
                };
                if let Some((name, exited, external, attach_id, harness_session_id)) = agent {
                    if view.term.0 < MIN_ROWS_FOR_STATUS {
                        view.set_notice("terminal too short for the confirm prompt".into());
                        continue;
                    }
                    let action = match (external, attach_id) {
                        (true, Some(id)) if exited => ConfirmKind::RemoveExternal {
                            attach_id: id,
                            name: name.clone(),
                        },
                        (true, Some(id)) => ConfirmKind::StopExternal {
                            attach_id: id,
                            name: name.clone(),
                        },
                        // (x-f191 scope b) `x` states the intent ONCE: remove.
                        // The server orchestrates stop-then-rm behind this one
                        // confirm - a live row is stopped as part of its
                        // removal, never as a second ceremony. Stop-only lives
                        // on the row menu's Stop.
                        // (x-f191 scope b) `x` states the intent ONCE: remove.
                        // The server orchestrates stop-then-rm behind this one
                        // confirm - a live row is stopped as part of its
                        // removal, never as a second ceremony. Stop-only lives
                        // on the row menu's Stop.
                        _ => ConfirmKind::RemoveAgent {
                            name: name.clone(),
                            sid: harness_session_id,
                        },
                    };
                    // (x-f191 scope a+c) The slot feeds the post-commit
                    // re-anchor: the sideline stays open and the cursor stays
                    // on this row (or its neighbour) after the commit.
                    view.row_slot = Some(cur);
                    view.open_confirm(ConfirmAction {
                        action,
                        label: name,
                    });
                    continue;
                }
                let squad = match view.display_rows().get(cur) {
                    Some(DisplayRow::Sel(r)) if r.tab.is_none() => Some(r.squad),
                    _ => None,
                };
                match squad.and_then(|sq| {
                    view.layout
                        .squads
                        .iter()
                        .find(|s| s.id == sq)
                        .map(|s| (sq, s.name.clone(), s.panes))
                }) {
                    Some(_) if view.term.0 < MIN_ROWS_FOR_STATUS => {
                        view.set_notice("terminal too short for the confirm prompt".into())
                    }
                    Some((sq, name, panes)) => {
                        let last = view.layout.squads.len() == 1;
                        view.open_confirm(ConfirmAction {
                            action: ConfirmKind::RemoveSquad {
                                squad: sq,
                                panes,
                                last,
                            },
                            label: name,
                        });
                    }
                    None => view.set_notice("no action for this row".into()),
                }
            }
            b'J' | b'K' => {
                // Reorder the squad at the cursor down (`J`) / up (`K`) the
                // sideline (x-96e8). The cursor follows the squad via sel_follow
                // on the authoritative next Layout. Tab/other rows notice why.
                let squad = match view.display_rows().get(cur) {
                    Some(DisplayRow::Sel(r)) if r.tab.is_none() => Some(r.squad),
                    _ => None,
                };
                match squad {
                    Some(sq) => {
                        let delta = if k == b'J' { 1 } else { -1 };
                        view.sel_follow = Some(sq);
                        write_msg(
                            sock_w,
                            &ClientMsg::Command(Command::MoveSquad { squad: sq, delta }),
                        )
                        .await
                        .map_err(|e| format!("move-squad send failed: {e}"))?;
                    }
                    None => view.set_notice("only a workspace row can be reordered".into()),
                }
            }
            b'm' => {
                // x-8ccf US2: `m` on an agent row - or (x-1d91) a Backlog card,
                // or (x-7683) a band header whose section menu the mouse path
                // already opens - opens its context menu (mouse-off parity),
                // anchored at the row and sitting over the selector like peek;
                // Esc drops back into the selector. A header that refuses (a
                // band with nothing to clear) already set its notice; the key
                // is still swallowed.
                if matches!(
                    view.display_rows().get(cur),
                    Some(DisplayRow::Agent(_) | DisplayRow::Card(_) | DisplayRow::Header { .. })
                ) {
                    // Screen row = index - sideline_offset (x-cd67: the sideline
                    // owns row 0; there is no TAB_BAR_ROWS offset on this side
                    // of the divider).
                    let arow = (cur.saturating_sub(view.sideline_offset)) as u16;
                    // A row that refuses with its own notice (an all-live band)
                    // keeps it; one that refuses SILENTLY (the Backlog band has
                    // no menu by design and says nothing on the right-press
                    // path) still gets a word here, because a swallowed key
                    // with zero feedback reads as a dead key. Compared against
                    // the notice BEFORE the call, so a stale unrelated notice
                    // within its TTL cannot mask the dead-key feedback.
                    let before = view.notice.clone();
                    if !view.open_row_menu(cur, Anchor::At { row: arow, col: 1 })
                        && view.notice == before
                    {
                        view.set_notice("this row has no menu".into());
                    }
                    continue;
                }
                // Move a tab into another squad (x-96e8): open the numbered
                // picker over the OTHER squads (a squad is moved with J/K, not
                // m). Tab rows left the sideline (x-0090), so `m` on a squad row
                // targets that squad's ACTIVE tab - the one shown in the tab bar.
                // A non-squad row, or nowhere to move to (one squad), notices why.
                let picked = match view.display_rows().get(cur) {
                    Some(DisplayRow::Sel(r)) if r.tab.is_none() => Some(r.squad),
                    _ => None,
                }
                .and_then(|squad| {
                    let sq = view.layout.squads.iter().find(|s| s.id == squad)?;
                    let tid = sq.tabs.get(sq.active_tab).or_else(|| sq.tabs.first())?.id;
                    // Exclude the source AND mission sentinels: a mission id
                    // resolves to no server-side squad, so MoveTab into one is
                    // refused. That rule now lives in `move_dst_squads`, which
                    // the Move-to-workspace menu entry already uses - this site
                    // was a fourth hand-rolled copy of the same list.
                    let dsts: Vec<u64> = view.move_dst_squads(Some(squad));
                    (!dsts.is_empty()).then_some((tid, dsts))
                });
                match picked {
                    Some((tid, dsts)) => view.open_move_pick(MoveSrc::Tab(tid), dsts),
                    None => view.set_notice("no other workspace to move this tab to".into()),
                }
            }
            0x1b | b'q' => view.selector = None,
            _ => {}
        }
    }
    // Follow the (possibly moved) cursor / expanded catalog into the scroll
    // window so a row driven below the fold stays visible (x-a621).
    view.clamp_sideline_offset();
    Ok(StdinFlow::Continue)
}

/// Move-tab / move-pane picker keys (x-96e8; cursored by x-3e17).
///
/// Two axes reach the same destination. A digit `1..=9` commits the numbered
/// squad in one keystroke, exactly as it always has - that is why digits still
/// COMMIT here while in the attach picker they only select. The attach picker
/// needs a second axis (the split direction) and so cannot commit on a digit;
/// this picker has no second axis, so a one-key move is the whole gesture and
/// there is nothing to gain by making it two.
///
/// The cursor is what reaches PAST nine, now that the list is uncapped: hjkl
/// and the arrows move it, Enter/Space commits it. Which means the picker is no
/// longer single-shot - it must survive a keypress that is neither. An unmapped
/// key is now ignored rather than closing the overlay, because "j closed my
/// picker" is exactly the surprise the cursor exists to remove.
///
/// The captured id is re-validated against the CURRENT catalog before sending
/// (stale -> BEL + close; the server refuses a stale id regardless).
async fn move_pick_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let keys = {
        let Some(picker) = view.move_pick.as_mut() else {
            return Ok(StdinFlow::Continue);
        };
        let mut esc = std::mem::take(&mut picker.esc);
        let keys = fold_selector_keys(&mut esc, bytes);
        picker.esc = esc;
        keys
    };

    for key in keys {
        // Cursor motion first: it is the only branch that leaves the picker open
        // and unchanged otherwise.
        let step = match key {
            b'k' | b'h' => Some(-1isize),
            b'j' | b'l' => Some(1isize),
            _ => None,
        };
        if let Some(delta) = step {
            if let Some(picker) = view.move_pick.as_mut() {
                let len = picker.squads.len();
                if len > 0 {
                    let cur = picker.cursor.min(len - 1) as isize;
                    picker.cursor = (cur + delta).clamp(0, len as isize - 1) as usize;
                }
            }
            continue;
        }
        // A digit picks by ordinal, Enter/Space picks whatever the cursor is on.
        // Everything else either cancels or is ignored.
        let idx = match key {
            // A digit past the END OF THE LIST names a row that was never
            // there, so it is a BEL that leaves the picker open - the same
            // answer the attach picker gives. Only an in-range digit is a
            // commit attempt; closing on an out-of-range one would be the
            // "j closed my picker" surprise the cursor exists to remove.
            b'1'..=b'9' => {
                let idx = (key - b'1') as usize;
                let in_range = view
                    .move_pick
                    .as_ref()
                    .is_some_and(|p| idx < p.squads.len());
                if !in_range {
                    let _ = raw_out(b"\x07");
                    continue;
                }
                idx
            }
            b'\r' | b'\n' | b' ' => match view.move_pick.as_ref() {
                Some(p) => p.cursor,
                None => return Ok(StdinFlow::Continue),
            },
            0x1b | b'q' => {
                view.move_pick = None;
                return Ok(StdinFlow::Continue);
            }
            _ => continue,
        };
        // Committing consumes the picker, so a stale overlay can never
        // resurrect a second move.
        let Some(picker) = view.move_pick.take() else {
            return Ok(StdinFlow::Continue);
        };
        let (src, squads) = (picker.src, picker.squads);
        {
            match squads.get(idx) {
                // The captured id must still name a live squad; the server
                // refuses a stale id regardless, but pre-validating saves a
                // round-trip and keeps the BEL local.
                Some(&sq) if view.layout.squads.iter().any(|s| s.id == sq) => match src {
                    MoveSrc::Tab(tab) => {
                        write_msg(
                            sock_w,
                            &ClientMsg::Command(Command::MoveTab { tab, squad: sq }),
                        )
                        .await
                        .map_err(|e| format!("move-tab send failed: {e}"))?;
                    }
                    // A pane move needs an anchor pane in the destination: its
                    // active tab's focus leaf (else the first leaf). MovePane
                    // grafts the mover beside that anchor, de-recruiting it from
                    // the source workspace (the `move_pane_cross_tab` path).
                    // `target` is mandatory for a cross-tab move; without it the
                    // server navigates from the mover, which lives in another
                    // tab, and the move is refused.
                    MoveSrc::Pane(pid) => {
                        // Prefer the active tab's focus leaf; if that tab has no
                        // panes (transient during a close, or an older server with
                        // empty TabMeta.panes), fall back to the first leaf in any
                        // tab so a valid anchor is not missed.
                        let anchor = view
                            .layout
                            .squads
                            .iter()
                            .find(|s| s.id == sq)
                            .and_then(|s| {
                                s.tabs
                                    .get(s.active_tab)
                                    .and_then(|t| t.panes.first())
                                    .or_else(|| s.tabs.iter().flat_map(|t| t.panes.first()).next())
                                    .map(|p| p.id)
                            });
                        match anchor {
                            Some(anchor) => {
                                write_msg(
                                    sock_w,
                                    &ClientMsg::Command(Command::MovePane {
                                        mover: Some(pid),
                                        target: Some(anchor),
                                        dir: Dir::Right,
                                    }),
                                )
                                .await
                                .map_err(|e| format!("move-pane send failed: {e}"))?;
                            }
                            None => view
                                .set_notice("no pane in that workspace to anchor against".into()),
                        }
                    }
                },
                _ => {
                    let _ = raw_out(b"\x07");
                }
            }
        }
        return Ok(StdinFlow::Continue);
    }
    Ok(StdinFlow::Continue)
}

/// One folded search-input token (v12, x-e780). A printable/control byte for the
/// query, or a bare Esc press. Complete arrow sequences are swallowed by the fold
/// (cursor motion is discretionary polish, Discretion 4).
#[derive(Debug, PartialEq, Eq)]
enum SearchKey {
    Byte(u8),
    Esc,
}

/// Fold raw search-mode bytes, carrying escape state in `esc` ACROSS reads so an
/// ESC-prefixed sequence broken at a read boundary never exits the search or
/// leaks its tail into the query. A whole CSI sequence (`ESC [ ` params `x`) is
/// consumed up to and including its final byte (`0x40..=0x7e`) and swallowed, so
/// a MULTI-byte sequence - PageUp `ESC [ 5 ~`, Ctrl-Arrow `ESC [ 1 ; 5 A` - never
/// leaks its param/final tail into the typed query (gemini review, HIGH). A bare
/// Esc surfaces as [`SearchKey::Esc`]; everything else is a [`SearchKey::Byte`].
/// A lone trailing ESC stays pending until the next byte disambiguates it.
/// Query-length ceiling. Far above any real search term; only bounds the scan
/// cost against a held key or a paste. (gemini review, MEDIUM)
const MAX_SEARCH_QUERY: usize = 256;

fn fold_search_input(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<SearchKey> {
    let mut keys = Vec::new();
    for &b in bytes {
        match esc.as_slice() {
            [] => {
                if b == 0x1b {
                    esc.push(0x1b);
                } else {
                    keys.push(SearchKey::Byte(b));
                }
            }
            [0x1b] => {
                if b == b'[' {
                    esc.push(b); // CSI introducer: start accumulating the sequence
                } else {
                    // A lone [ESC] then a non-'[' byte: that ESC was a bare Esc
                    // press. Surface it, then reprocess `b`.
                    esc.clear();
                    keys.push(SearchKey::Esc);
                    if b == 0x1b {
                        esc.push(0x1b); // a new ESC just started
                    } else {
                        keys.push(SearchKey::Byte(b));
                    }
                }
            }
            // Inside a CSI (`ESC [ ...`): keep eating param/intermediate bytes,
            // swallowing the whole sequence at its final byte. Bounded so a
            // pathological stream can never grow `esc` without limit.
            // ponytail: 16-byte ceiling; real CSI sequences are far shorter.
            _ => {
                if b == 0x1b {
                    // ESC aborts any in-progress sequence and starts a fresh
                    // one (standard VT semantics). Without this, an ESC arriving
                    // mid-CSI (a split sequence in the buffer) would be eaten as
                    // a param byte, so pressing Esc to cancel search would
                    // silently fail. (gemini review, HIGH)
                    esc.clear();
                    esc.push(0x1b);
                } else if (0x40..=0x7e).contains(&b) || esc.len() >= 16 {
                    esc.clear();
                } else {
                    esc.push(b);
                }
            }
        }
    }
    keys
}

/// Navigator fold keys. Superset of [`SearchKey`]: the same split-arrow escape
/// fold, but a completed CSI whose final byte is Up/Down/Shift-Tab surfaces as a
/// motion token instead of being swallowed (ab-63b44059). Every other CSI is
/// still consumed whole, so no escape tail leaks into the query or the pane.
enum NavKey {
    Byte(u8),
    Esc,
    Up,
    Down,
    /// (x-e10f) Bare Right: reach the selected row (the Enter/goto arm).
    Right,
    /// (x-e10f) Bare Left: close (the Esc arm) - back to the pane you came
    /// from. The overlay owns every keystroke, so a bare arrow is free.
    Left,
    ShiftTab,
}

/// Fold navigator-mode bytes. Identical escape-carry semantics to
/// [`fold_search_input`] (whole CSI consumed, split sequences carried across
/// reads via `esc`), except the arrow-Up `ESC [ A`, arrow-Down `ESC [ B`,
/// arrow-Right/Left `ESC [ C`/`ESC [ D`, and Shift-Tab `ESC [ Z` finals become
/// [`NavKey::Up`]/[`Down`]/[`Right`]/[`Left`]/[`ShiftTab`] so the navigator can
/// move its cursor, goto, close, and reverse-cycle the state chip. A modified
/// arrow (`ESC [ 1 ; 5 A`) shares the final byte and maps to the same motion -
/// harmless. All other finals are swallowed, same leak-safety as search.
fn fold_nav_input(esc: &mut Vec<u8>, bytes: &[u8]) -> Vec<NavKey> {
    let mut keys = Vec::new();
    for &b in bytes {
        match esc.as_slice() {
            [] => {
                if b == 0x1b {
                    esc.push(0x1b);
                } else {
                    keys.push(NavKey::Byte(b));
                }
            }
            [0x1b] => {
                if b == b'[' {
                    esc.push(b);
                } else {
                    esc.clear();
                    keys.push(NavKey::Esc);
                    if b == 0x1b {
                        esc.push(0x1b);
                    } else {
                        keys.push(NavKey::Byte(b));
                    }
                }
            }
            _ => {
                if b == 0x1b {
                    esc.clear();
                    esc.push(0x1b);
                } else if (0x40..=0x7e).contains(&b) {
                    // CSI complete. Surface the three motion finals; swallow the
                    // rest. Only a BARE `ESC [ X` counts: a parameterised
                    // sequence is a MODIFIED key (Ctrl-Up is `ESC [ 1 ; 5 A`),
                    // and aliasing it onto the unmodified one silently
                    // reinterprets a chord the operator meant as something else.
                    // This fold serves prefix+f, the navigator this change
                    // promotes to the primary route, so it is the last place
                    // that should guess.
                    if esc.len() == 2 {
                        match b {
                            b'A' => keys.push(NavKey::Up),
                            b'B' => keys.push(NavKey::Down),
                            b'C' => keys.push(NavKey::Right),
                            b'D' => keys.push(NavKey::Left),
                            b'Z' => keys.push(NavKey::ShiftTab),
                            _ => {}
                        }
                    }
                    esc.clear();
                } else if esc.len() >= MAX_ESC_CARRY {
                    esc.clear();
                } else {
                    esc.push(b);
                }
            }
        }
    }
    keys
}

/// Search-mode keys (v12, x-e780). Typing: printable append, Backspace pops,
/// Enter submits (send [`ClientMsg::SearchOpen`]), Esc cancels locally. Browsing
/// (post-submit): `n`/`N` send [`ClientMsg::SearchStep`] (older/newer), Esc sends
/// [`ClientMsg::SearchClear`] and exits. Esc ALWAYS exits the mode locally even
/// if the server never replied (AC1-FR: a lost `SearchResult` never wedges the
/// input line). The mode is re-read per key so an Esc mid-chunk swallows the rest.
async fn search_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.search_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.search_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc mid-chunk closes it, and the rest of
        // the chunk must be swallowed, never forwarded.
        let Some(sv) = view.search.as_ref() else {
            break;
        };
        let (pane, submitted) = (sv.pane, sv.submitted);
        match key {
            SearchKey::Esc => {
                view.search = None;
                view.search_esc.clear();
                if submitted {
                    // Browsing: drop the shared server-side highlight + state.
                    write_msg(sock_w, &ClientMsg::SearchClear { pane })
                        .await
                        .map_err(|e| format!("search-clear send failed: {e}"))?;
                }
                break;
            }
            SearchKey::Byte(b) if !submitted => match b {
                b'\r' | b'\n' => {
                    if let Some(sv) = view.search.as_mut() {
                        sv.submitted = true;
                        let query = sv.query.clone();
                        write_msg(sock_w, &ClientMsg::SearchOpen { pane, query })
                            .await
                            .map_err(|e| format!("search-open send failed: {e}"))?;
                    }
                }
                0x7f | 0x08 => {
                    if let Some(sv) = view.search.as_mut() {
                        sv.query.pop();
                    }
                }
                // ASCII printable appends (other control bytes ignored; query is
                // ASCII in v1). Capped so a held key / paste can't grow it unbounded
                // and drive an O(len * scrollback) server scan.
                0x20..=0x7e => {
                    if let Some(sv) = view.search.as_mut() {
                        if sv.query.len() < MAX_SEARCH_QUERY {
                            sv.query.push(b as char);
                        }
                    }
                }
                _ => {}
            },
            SearchKey::Byte(b) => match b {
                b'n' => write_msg(
                    sock_w,
                    &ClientMsg::SearchStep {
                        pane,
                        dir: BlockDir::Prev,
                    },
                )
                .await
                .map_err(|e| format!("search-step send failed: {e}"))?,
                b'N' => write_msg(
                    sock_w,
                    &ClientMsg::SearchStep {
                        pane,
                        dir: BlockDir::Next,
                    },
                )
                .await
                .map_err(|e| format!("search-step send failed: {e}"))?,
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Navigator-overlay keys (x-653d): a client-owned typing overlay like search.
/// Printable bytes edit the text filter (Locked 5: letters are ALWAYS query
/// text, never state keys); Backspace widens; `Tab`/`Shift-Tab` cycle the state
/// chip forward/back; `Up`/`Down` (or `Ctrl-p`/`Ctrl-n`) move the cursor over the
/// filtered rows (clamped, no wrap); Enter or bare `Right` goto's the row;
/// `Esc` or bare `Left` closes (x-e10f). Uses
/// [`fold_nav_input`]'s split-arrow fold (which surfaces the motion finals while
/// swallowing every other escape) and a per-key re-read so an Esc mid-chunk
/// swallows the chunk's remainder (ab-63b44059).
async fn nav_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.nav_esc);
    let keys = fold_nav_input(&mut esc, bytes);
    view.nav_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc mid-chunk closes it and the rest of
        // the chunk must be swallowed, never forwarded.
        if view.nav.is_none() {
            break;
        }
        match key {
            NavKey::Esc | NavKey::Left => {
                view.nav = None;
                view.nav_esc.clear();
                break;
            }
            // Arrows mirror Ctrl-p/Ctrl-n; Shift-Tab reverses the state ring.
            NavKey::Up => view.nav_move_cursor(-1),
            NavKey::Down => view.nav_move_cursor(1),
            // (x-e10f) Bare Right reaches the selected row - the same
            // nav_goto the Enter arm calls, so substrate behavior (attach a
            // thread, focus a pane, refuse with a notice) is the same too.
            NavKey::Right => nav_goto(view, sock_w).await?,
            NavKey::ShiftTab => {
                view.nav_cycle_state_rev();
                view.nav_ring_if_empty();
            }
            NavKey::Byte(b) => match b {
                b'\r' | b'\n' => nav_goto(view, sock_w).await?,
                b'\t' => {
                    view.nav_cycle_state();
                    view.nav_ring_if_empty();
                }
                // Ctrl-n / Ctrl-p move the cursor (readline convention), kept
                // alongside the arrow tokens for muscle memory.
                0x0e => view.nav_move_cursor(1),
                0x10 => view.nav_move_cursor(-1),
                0x7f | 0x08 => {
                    if let Some(n) = view.nav.as_mut() {
                        n.query.pop();
                        n.cursor = 0;
                    }
                    view.nav_ring_if_empty();
                }
                // Printable ASCII edits the query; capped like search so a held
                // key / paste can't grow it unbounded. Cursor re-anchors to 0.
                0x20..=0x7e => {
                    if let Some(n) = view.nav.as_mut() {
                        if n.query.len() < MAX_SEARCH_QUERY {
                            n.query.push(b as char);
                            n.cursor = 0;
                        }
                    }
                    view.nav_ring_if_empty();
                }
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Teleport to the navigator's cursor row (x-653d). Materializes the OWNED
/// target before mutating the view (`nav_rows` borrows the layout), re-reading
/// the filtered catalog at Enter time (per-key re-read; AC4-ERR relies on the
/// server refusing a stale id fail-closed). A refusal (`Notice`: blocked /
/// in-flight card, paneless agent) KEEPS the navigator open and shows the notice
/// (Locked 6), sending nothing. Otherwise it closes the overlay, switches squad
/// when the target lives in another one (a same-squad target collapses to a bare
/// hit), and applies the hit. Existing wire commands only - no new `Command`, no
/// proto bump (Locked 4).
async fn nav_goto(
    view: &mut View,
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<(), String> {
    let target = match view.nav.as_ref() {
        Some(n) => match view.nav_filtered(n).into_iter().nth(n.cursor) {
            Some(r) => r,
            // Empty/stale cursor: BEL, keep the overlay open (never a silent
            // close), matching the selector's stale-cursor BEL.
            None => {
                let _ = raw_out(b"\x07");
                return Ok(());
            }
        },
        None => return Ok(()),
    };
    // A refusal keeps the overlay open (Locked 6), identical to the selector.
    if let ChromeHit::Notice(msg) = &target.hit {
        view.set_notice(msg.clone());
        return Ok(());
    }
    view.nav = None;
    view.nav_esc.clear();
    // Ordered goto prefix (Locked 4: existing wire commands only). An agent/pane
    // row in another squad switches squad first; a pane row then selects its tab
    // (FocusPane alone does not) so the sequence is SelectSquad -> SelectTab ->
    // FocusPane. Squad/tab rows carry their own switch in `hit` (both prefixes
    // None), so no double send; a pane already in the active view collapses to a
    // bare FocusPane.
    let switching_squad = target
        .goto_squad
        .is_some_and(|sq| sq != view.layout.active_squad);
    if let Some(sq) = target.goto_squad.filter(|_| switching_squad) {
        write_msg(sock_w, &ClientMsg::Command(Command::SelectSquad(sq)))
            .await
            .map_err(|e| format!("nav select-squad send failed: {e}"))?;
    }
    if let Some(tid) = target.goto_tab {
        // Skip SelectTab only when the target is already the active view's tab
        // (same squad, same tab); a squad switch always needs it.
        let active_tab_id = view
            .layout
            .squads
            .iter()
            .find(|s| s.id == view.layout.active_squad)
            .and_then(|s| s.tabs.get(s.active_tab))
            .map(|t| t.id);
        if switching_squad || active_tab_id != Some(tid) {
            write_msg(sock_w, &ClientMsg::Command(Command::SelectTab(tid)))
                .await
                .map_err(|e| format!("nav select-tab send failed: {e}"))?;
        }
    }
    apply_hit(view, target.hit, sock_w).await
}

/// New-workspace name-input keys (x-9e5e). Reuses the search input's split-arrow
/// folding: printable ASCII appends, Backspace pops, Enter sends
/// [`Command::NewSquad`] with the typed name (an empty name keeps the overlay
/// open - the server would reject it, and keeping it open avoids the round trip),
/// Esc cancels locally. The whole chunk is swallowed so an arrow's escape tail
/// never leaks into a pane.
async fn create_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.create_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.create_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc mid-chunk closes it, and the rest of
        // the chunk must be swallowed, never forwarded.
        if view.create.is_none() {
            break;
        }
        match key {
            SearchKey::Esc => {
                view.create = None;
                view.create_esc.clear();
                break;
            }
            SearchKey::Byte(b) => match b {
                b'\r' | b'\n' => {
                    // Validate on a reference; only allocate when actually sending.
                    if let Some(name) = view.create.as_deref().map(str::trim) {
                        if !name.is_empty() {
                            write_msg(
                                sock_w,
                                &ClientMsg::Command(Command::NewSquad {
                                    name: name.to_string(),
                                    origin: None,
                                }),
                            )
                            .await
                            .map_err(|e| format!("new-squad send failed: {e}"))?;
                            view.create = None;
                            view.create_esc.clear();
                            break;
                        }
                    }
                    // Empty name: keep the overlay open (AC2-FR shape - a failed
                    // create leaves the input intact).
                }
                0x7f | 0x08 => {
                    if let Some(buf) = view.create.as_mut() {
                        buf.pop();
                    }
                }
                0x20..=0x7e => {
                    if let Some(buf) = view.create.as_mut() {
                        if buf.len() < MAX_SEARCH_QUERY {
                            buf.push(b as char);
                        }
                    }
                }
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Recruit workspace-name keys (x-8f11): the create overlay's shape - printable
/// append, Backspace pops, Esc cancels locally (marks kept), Enter sends
/// [`Command::RecruitAgents`] with the marked ids and CLEARS the marks. An empty
/// name keeps the overlay open (the server would refuse it). An empty mark set
/// falls back to nothing sendable, so Enter just closes (the `R` key already
/// fell back to marking the focused row before opening).
async fn recruit_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.recruit_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.recruit_esc = esc;
    for key in keys {
        if view.recruit.is_none() {
            break;
        }
        match key {
            SearchKey::Esc => {
                view.recruit = None;
                view.recruit_esc.clear();
                break; // marks kept - Esc cancels the prompt only
            }
            SearchKey::Byte(b) => match b {
                b'\r' | b'\n' => {
                    if let Some(name) = view.recruit.as_deref().map(str::trim) {
                        if !name.is_empty() {
                            let ids: Vec<String> = view.marks.iter().cloned().collect();
                            write_msg(
                                sock_w,
                                &ClientMsg::Command(Command::RecruitAgents {
                                    squad: name.to_string(),
                                    ids,
                                }),
                            )
                            .await
                            .map_err(|e| format!("recruit send failed: {e}"))?;
                            view.recruit = None;
                            view.recruit_esc.clear();
                            view.marks.clear(); // submit clears the marks (AC2-HP)
                            break;
                        }
                    }
                    // Empty name: keep the overlay open (server would refuse).
                }
                0x7f | 0x08 => {
                    if let Some(buf) = view.recruit.as_mut() {
                        buf.pop();
                    }
                }
                0x20..=0x7e => {
                    if let Some(buf) = view.recruit.as_mut() {
                        if buf.len() < MAX_SQUAD_NAME {
                            buf.push(b as char);
                        }
                    }
                }
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Rename-tab name-input keys (x-c150). The create overlay's shape (split-arrow
/// folding, printable append, Backspace pops, Esc cancels locally) with one
/// deliberate divergence: Enter ALWAYS sends [`Command::RenameTab`] - an empty
/// buffer is the "reset to auto" verb (blank clears server-side), not a kept-open
/// input. The buffer caps at [`MAX_TAB_NAME`] so the operator sees exactly what
/// the server will store (the server-side cap stays authoritative for the wire).
async fn rename_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.rename_esc);
    let keys = fold_search_input(&mut esc, bytes);
    view.rename_esc = esc;
    for key in keys {
        // Re-read the mode each key: an Esc mid-chunk closes it, and the rest
        // of the chunk must be swallowed, never forwarded.
        if view.rename.is_none() {
            break;
        }
        match key {
            SearchKey::Esc => {
                // AC1-UI: no command sent, chrome restored, no state retained.
                view.rename = None;
                view.rename_esc.clear();
                break;
            }
            SearchKey::Byte(b) => match b {
                b'\r' | b'\n' => {
                    if let Some((target, name)) = view.rename.take() {
                        // An agent label is never derived, so an empty buffer
                        // is NOT the tab/squad "reset to auto": the overlay
                        // stays open and the send never happens.
                        if matches!(&target, RenameTarget::Agent(_)) && name.is_empty() {
                            view.rename = Some((target, name));
                            view.set_notice("label required - type the new registry label".into());
                            break;
                        }
                        view.rename_esc.clear();
                        let cmd = match target {
                            RenameTarget::Tab(tab) => Command::RenameTab { tab, name },
                            RenameTarget::Squad(squad) => Command::RenameSquad { squad, name },
                            RenameTarget::Agent(agent) => Command::RenameAgent {
                                name: agent,
                                new_name: name,
                            },
                        };
                        write_msg(sock_w, &ClientMsg::Command(cmd))
                            .await
                            .map_err(|e| format!("rename send failed: {e}"))?;
                    }
                    break;
                }
                0x7f | 0x08 => {
                    if let Some((_, buf)) = view.rename.as_mut() {
                        buf.pop();
                    }
                }
                0x20..=0x7e => {
                    if let Some((target, buf)) = view.rename.as_mut() {
                        // Cap to the target's stored ceiling so the operator sees
                        // exactly what the server will keep (server stays
                        // authoritative for the wire).
                        let cap = match target {
                            RenameTarget::Tab(_) => MAX_TAB_NAME,
                            RenameTarget::Squad(_) => MAX_SQUAD_NAME,
                            // The registry grammar's own ceiling.
                            RenameTarget::Agent(_) => 64,
                        };
                        // An agent label admits only grammar bytes; a space or
                        // symbol never enters the buffer, so what is typed is
                        // what the server would keep.
                        let legal = match target {
                            RenameTarget::Agent(_) => {
                                b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
                            }
                            _ => true,
                        };
                        if legal && buf.len() < cap {
                            buf.push(b as char);
                        }
                    }
                }
                _ => {}
            },
        }
    }
    Ok(StdinFlow::Continue)
}

/// Move-to-position prompt keys (x-cf97). The rename overlay's shape
/// ([`rename_keys`]: Esc cancels locally, Backspace pops, printable append)
/// with a numeric grammar: only digits enter the buffer, Enter resolves the
/// 1-based ordinal against the tab's squad strip, computes the delta to the
/// tab's current index, and sends ONE `Command::ReorderTab`. An out-of-range
/// ordinal keeps the prompt open with a notice - it never sends a clamped
/// guess, because a move that lands somewhere else than named is worse than
/// no move. The tab id was captured at open, so a tab switch mid-edit cannot
/// retarget the send.
async fn move_to_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    for &b in bytes {
        if view.move_to.is_none() {
            break;
        }
        match b {
            0x1b => {
                view.move_to = None;
                break;
            }
            b'\r' | b'\n' => {
                if let Some((tab, buf)) = view.move_to.take() {
                    let ordinal: usize = buf.parse().unwrap_or(0);
                    let Some((squad, current_idx, _)) = view.find_tab(tab) else {
                        view.set_notice("tab is no longer here".into());
                        break;
                    };
                    let len = view
                        .layout
                        .squads
                        .iter()
                        .find(|s| s.id == squad)
                        .map(|s| s.tabs.len())
                        .unwrap_or(0);
                    if ordinal == 0 || ordinal > len {
                        // Re-open with the typed text intact: the prompt stays
                        // open with a notice, so a typo costs a Backspace, not
                        // the whole gesture.
                        view.move_to = Some((tab, buf));
                        view.set_notice(format!("position is 1..={len}"));
                    } else {
                        let delta = ordinal as i64 - 1 - current_idx as i64;
                        if delta == 0 {
                            view.set_notice(format!("tab already at {ordinal}"));
                        } else {
                            write_msg(
                                sock_w,
                                &ClientMsg::Command(Command::ReorderTab {
                                    squad,
                                    tab,
                                    delta: delta as i32,
                                }),
                            )
                            .await
                            .map_err(|e| format!("reorder-tab send failed: {e}"))?;
                        }
                    }
                }
                break;
            }
            0x7f | 0x08 => {
                if let Some((_, buf)) = view.move_to.as_mut() {
                    buf.pop();
                }
            }
            b'0'..=b'9' => {
                if let Some((_, buf)) = view.move_to.as_mut() {
                    // Four digits is far past any tab strip; the cap keeps the
                    // operator seeing exactly the number Enter will send.
                    if buf.len() < 4 {
                        buf.push(b as char);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// Needs-me overlay keys (x-feec, grown from x-c929; x-f730 folded MINE and
/// live questions in as editable/answerable lanes). A digit answers the
/// selected answerable NEED row (unchanged [`ClientMsg::PaneAnswer`]) or, for
/// a question with options, closes it via `outstanding clear --answer`.
/// `n`/`N` (and j/k/arrows) cycle lanes, Enter routes per kind, q/Esc closes;
/// `x`/`d` toggle/drop a MINE row, `a` opens a text entry that appends an
/// item. Mutations only queue `View::mine_action` / `View::question_action`
/// (each single-flighted): the file is the one writer, so the render updates
/// once the mutation lands, never optimistically. The projection is read once
/// per chunk from [`View::needs_projection`], so cursor and rows never
/// diverge. An empty overlay closes on any key except `a`. Closing bumps the
/// generation token so an in-flight fold result is discarded.
async fn answer_keys(
    view: &mut View,
    bytes: &[u8],
    sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.ans_esc);
    let keys = fold_selector_keys(&mut esc, bytes); // arrows -> hjkl twins
    view.ans_esc = esc;
    let projection = view.needs_projection();
    // Active squad/tab, captured once (the layout is stable within a key chunk):
    // an Enter goto sends SelectSquad/SelectTab only when they would change the
    // view, mirroring the x-653d nav goto so a same-context row emits just
    // FocusPane (no redundant selects).
    let active_squad = view.layout.active_squad;
    let active_tab = view
        .layout
        .squads
        .iter()
        .find(|s| s.id == active_squad)
        .and_then(|s| s.tabs.get(s.active_tab))
        .map(|t| t.id);
    for &k in &keys {
        // The MINE add text entry owns the keyboard ahead of everything below
        // it - a typed letter must never be read as a cycle/answer key.
        if let Some(buf) = view.mine_adding.as_mut() {
            match k {
                b'\r' | b'\n' => {
                    let text = std::mem::take(buf).trim().to_string();
                    view.mine_adding = None;
                    if !text.is_empty() && !view.mine_acting {
                        view.mine_action = Some(crate::needs_overlay::MineMutation::Add(text));
                        view.mine_acting = true;
                    }
                }
                0x1b => view.mine_adding = None,
                0x7f | 0x08 => {
                    buf.pop();
                }
                0x20..=0x7e => buf.push(k as char),
                _ => {}
            }
            continue;
        }
        // Same keyboard-ownership contract for the free-text question answer
        // (x-f730 task 2.3): opened by Enter on a no-options question, so it
        // is checked right alongside `mine_adding`.
        if let Some((qid, buf)) = view.question_answering.as_mut() {
            match k {
                b'\r' | b'\n' => {
                    let text = std::mem::take(buf).trim().to_string();
                    let qid = qid.clone();
                    view.question_answering = None;
                    if !text.is_empty() && !view.question_acting {
                        view.question_action = Some((qid, text));
                        view.question_acting = true;
                    }
                }
                0x1b => view.question_answering = None,
                0x7f | 0x08 => {
                    buf.pop();
                }
                0x20..=0x7e => buf.push(k as char),
                _ => {}
            }
            continue;
        }
        // `a` opens the add entry unconditionally - unlike every other key it
        // has a meaning on an empty overlay (start the operator's first
        // item), so it is handled ahead of the empty-dismisses-all rule.
        if k == b'a' {
            if !view.mine_acting {
                view.mine_adding = Some(String::new());
            }
            continue;
        }
        // The empty "nothing needs you" state: any other key dismisses it
        // (AC4-EDGE).
        if projection.rows.is_empty() {
            view.answers = None;
            view.needs_gen = view.needs_gen.wrapping_add(1);
            break;
        }
        let Some(cur0) = view.answers else {
            break; // closed mid-chunk
        };
        let cur = cur0.min(projection.rows.len() - 1);
        view.answers = Some(cur);
        match k {
            // Cycle: n/N are the documented keys; j/k and folded arrows too.
            b'n' | b'j' => view.answers = Some((cur + 1) % projection.rows.len()),
            b'N' | b'k' => {
                view.answers = Some((cur + projection.rows.len() - 1) % projection.rows.len())
            }
            // A MINE row toggle/drop; a NEED row is not addressable by either
            // key and both are a silent no-op there (only `a`/digit/Enter/q
            // act on a NEED row).
            b'x' if !view.mine_acting => {
                if let NeedsOverlayRow::Mine(item) = &projection.rows[cur] {
                    view.mine_action = Some(crate::needs_overlay::MineMutation::Toggle(item.n));
                    view.mine_acting = true;
                }
            }
            b'd' if !view.mine_acting => {
                if let NeedsOverlayRow::Mine(item) = &projection.rows[cur] {
                    view.mine_action = Some(crate::needs_overlay::MineMutation::Drop(item.n));
                    view.mine_acting = true;
                }
            }
            b'0'..=b'9' => {
                // A question with options answers first (x-f730 task 2.3):
                // the digit picks `options[n-1]`, closed via `outstanding
                // clear --answer`. A no-options question falls through to
                // the BEL below - Enter is its answer path. A MINE row has
                // no `NeedRow` to answer either - `.need()` is None and this
                // always beeps too, same as a non-answerable NEED row.
                if let Some(q) = projection.rows[cur].question() {
                    let n = (k - b'0') as usize;
                    match n.checked_sub(1).and_then(|i| q.options.get(i)) {
                        Some(opt) if !view.question_acting => {
                            view.question_action = Some((q.id.clone(), opt.clone()));
                            view.question_acting = true;
                        }
                        _ => {
                            let _ = raw_out(b"\x07");
                        }
                    }
                    continue;
                }
                let picked = projection.rows[cur].need().and_then(|sel| {
                    sel.answerable
                        .as_ref()
                        .and_then(|a| {
                            a.options
                                .iter()
                                .find(|o| o.idx.as_bytes().first() == Some(&k))
                                .map(|o| (a, o))
                        })
                        .zip(sel.pane_id)
                });
                match picked {
                    Some(((ans, o), pane)) => {
                        // Only ever the daemon-pinned keystroke; focus unchanged.
                        // The answered pane drops from the queue on the next
                        // scrape tick; the overlay stays open to cycle onward.
                        write_msg(
                            sock_w,
                            &ClientMsg::PaneAnswer {
                                pane,
                                fingerprint: ans.fingerprint,
                                region_lines: ans.region_lines as u16,
                                keystroke: o.keystroke.clone(),
                            },
                        )
                        .await
                        .map_err(|e| format!("answer send failed: {e}"))?;
                    }
                    // A digit with no matching option (a MINE row, or a
                    // non-answerable NEED row, e.g. review-wedged / budget /
                    // focus-only) is a local BEL, never a stray key sent to
                    // any pane (x-c929 invariant).
                    None => {
                        let _ = raw_out(b"\x07");
                    }
                }
            }
            b'\r' | b'\n' => {
                // A no-options question (x-f730 task 2.3): Enter opens the
                // free-text answer entry, typed below the row like the MINE
                // add box. A with-options question stays digit-only here -
                // Enter on it falls through to the goto arm below, which
                // (having no pane) shows the same notice a MINE row does.
                if let Some(q) = projection.rows[cur].question() {
                    if q.options.is_empty() {
                        view.question_answering = Some((q.id.clone(), String::new()));
                        continue;
                    }
                }
                // Goto the row's target (x-653d): SelectSquad/SelectTab only when
                // they change the view, then FocusPane; a paneless watch-only row
                // attaches; a squadless live fold row - or a MINE row, which
                // owns no pane at all - has no reachable pane here, so it
                // degrades to a notice (Invariant: every item actionable).
                match projection.rows[cur].need() {
                    Some(row) if row.pane_id.is_some() => {
                        let pane = row.pane_id.expect("checked Some above");
                        let switching = row.squad.is_some_and(|s| s != active_squad);
                        if let Some(sq) = row.squad.filter(|_| switching) {
                            write_msg(sock_w, &ClientMsg::Command(Command::SelectSquad(sq)))
                                .await
                                .map_err(|e| format!("command send failed: {e}"))?;
                        }
                        if let Some(tid) = row.tab.filter(|&t| switching || active_tab != Some(t)) {
                            write_msg(sock_w, &ClientMsg::Command(Command::SelectTab(tid)))
                                .await
                                .map_err(|e| format!("command send failed: {e}"))?;
                        }
                        write_msg(sock_w, &ClientMsg::Command(Command::FocusPane(pane)))
                            .await
                            .map_err(|e| format!("command send failed: {e}"))?;
                    }
                    Some(row) if row.attach_id.is_some() => {
                        let id = row.attach_id.as_ref().expect("checked Some above");
                        write_msg(sock_w, &ClientMsg::Command(Command::attach_agent(id)))
                            .await
                            .map_err(|e| format!("command send failed: {e}"))?;
                    }
                    _ => {
                        view.set_notice("no pane here - focus it manually".into());
                    }
                }
                view.answers = None;
                view.needs_gen = view.needs_gen.wrapping_add(1);
            }
            0x1b | b'q' => {
                view.answers = None;
                view.needs_gen = view.needs_gen.wrapping_add(1);
            }
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// (x-b2bf) Yard-overlay key routing: `n`/`N` (and j/k, folded arrows) move
/// the spotlight over the crowd, `q`/Esc close. Any other key is consumed -
/// an open modal owns the keyboard and never leaks a byte into a pane (the
/// answer-overlay invariant).
async fn yard_keys(
    view: &mut View,
    bytes: &[u8],
    _sock_w: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> Result<StdinFlow, String> {
    let mut esc = std::mem::take(&mut view.yard_esc);
    let keys = fold_selector_keys(&mut esc, bytes);
    view.yard_esc = esc;
    let len = view.layout.agents.len();
    for &k in &keys {
        let Some(yv) = view.yard.as_mut() else {
            break; // closed mid-chunk
        };
        if len == 0 {
            // The empty yard: any key dismisses it, like the needs overlay's
            // "nothing needs you".
            view.yard = None;
            view.yard_gen = view.yard_gen.wrapping_add(1);
            break;
        }
        yv.sel = yv.sel.min(len - 1);
        match k {
            b'n' | b'j' => yv.sel = (yv.sel + 1) % len,
            b'N' | b'k' => yv.sel = (yv.sel + len - 1) % len,
            0x1b | b'q' => {
                view.yard = None;
                view.yard_gen = view.yard_gen.wrapping_add(1);
            }
            _ => {}
        }
    }
    Ok(StdinFlow::Continue)
}

/// Write raw bytes (BEL, ModeSync escapes) straight to the terminal.
fn raw_out(bytes: &[u8]) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()
}

// The exit notice must print AFTER the alternate screen is left, or it is
// erased with the TUI. Thread-local because the select loop returns through
// several arms; a struct field would work too but this stays local to the file.
thread_local! {
    static NOTICE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn exit_with_notice(notice: String) -> i32 {
    NOTICE.with(|n| *n.borrow_mut() = Some(notice));
    0
}

/// Draws frames with a row-level diff against what was actually drawn last -
/// safe precisely because it diffs against its own output, never against a
/// prediction of server state.
struct Compositor {
    last: Option<Frame>,
}

impl Compositor {
    fn new() -> Self {
        Compositor { last: None }
    }

    fn draw(&mut self, frame: &Frame) -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        let full = match &self.last {
            Some(prev) => prev.rows != frame.rows || prev.cols != frame.cols,
            None => true,
        };
        if full {
            queue!(out, terminal::Clear(terminal::ClearType::All))?;
        }
        queue!(out, cursor::Hide)?;
        for r in 0..frame.rows as usize {
            if !full {
                // Row unchanged since we drew it? Skip the write entirely.
                let prev = self.last.as_ref().unwrap();
                let w = frame.cols as usize;
                if prev.cells[r * w..(r + 1) * w] == frame.cells[r * w..(r + 1) * w] {
                    continue;
                }
            }
            self.draw_row(&mut out, frame, r)?;
        }
        queue!(out, cursor::MoveTo(frame.cursor_col, frame.cursor_row))?;
        if frame.cursor_visible {
            queue!(out, cursor::Show)?;
        } else {
            queue!(out, cursor::Hide)?;
        }
        out.flush()?;
        self.last = Some(frame.clone());
        Ok(())
    }

    fn draw_row(&self, out: &mut impl Write, frame: &Frame, r: usize) -> std::io::Result<()> {
        queue!(out, cursor::MoveTo(0, r as u16))?;
        let w = frame.cols as usize;
        let mut style_of: Option<(Color, Color, u8)> = None;
        for cell in &frame.cells[r * w..(r + 1) * w] {
            if cell.flags & proto::cell_flags::WIDE_SPACER != 0 {
                continue; // the wide glyph before it already covers this column
            }
            let key = (cell.fg, cell.bg, cell.flags);
            if style_of != Some(key) {
                apply_style(out, cell)?;
                style_of = Some(key);
            }
            queue!(out, style::Print(cell.c))?;
        }
        // Leave the line in a reset state so scrolling artifacts never bleed.
        queue!(out, style::SetAttribute(style::Attribute::Reset))?;
        Ok(())
    }
}

fn apply_style(out: &mut impl Write, cell: &Cell) -> std::io::Result<()> {
    use proto::cell_flags as cf;
    // Reset first: attribute REMOVAL (e.g. bold -> plain) has no incremental
    // form worth tracking at this scale.
    queue!(out, style::SetAttribute(style::Attribute::Reset))?;
    if cell.flags & cf::BOLD != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Bold))?;
    }
    if cell.flags & cf::ITALIC != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Italic))?;
    }
    if cell.flags & cf::UNDERLINE != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Underlined))?;
    }
    // A SELECTED cell (US2) toggles reverse-video: XOR with the cell's own
    // inverse so the selection is always a visible delta, even over already-
    // inverse text.
    if (cell.flags & cf::INVERSE != 0) ^ (cell.flags & cf::SELECTED != 0) {
        queue!(out, style::SetAttribute(style::Attribute::Reverse))?;
    }
    if cell.flags & cf::DIM != 0 {
        queue!(out, style::SetAttribute(style::Attribute::Dim))?;
    }
    queue!(
        out,
        style::SetForegroundColor(map_color(cell.fg)),
        style::SetBackgroundColor(map_color(cell.bg))
    )?;
    Ok(())
}

fn map_color(c: Color) -> CtColor {
    match c {
        Color::Default => CtColor::Reset,
        Color::Indexed(i) => CtColor::AnsiValue(i),
        Color::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "client_tests/court_block_tests.rs"]
mod court_block_tests;

#[path = "client/court_block.rs"]
mod court_block;
