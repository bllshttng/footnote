//! Live tokio run loop for `fno agents grid` (ab-3c063856, Wave 5.1).
//!
//! Wires the pure FSM + render + layout machinery (built and tested in
//! `grid::pane`, `grid::layout`, `grid::state`) to real I/O:
//!
//! - one watcher WebSocket per agent (`agent.drive` with `mode: "watch"`),
//!   each drained by a spawned reader task that forwards PTY bytes over an
//!   mpsc channel tagged with the pane index;
//! - a `crossterm::event::EventStream` for async key input;
//! - a render tick that paints the tiled grid to stderr;
//! - a take-over path that opens a second `mode: "interactive"` connection
//!   to the focused agent on Enter and routes keystrokes to it.
//!
//! The structure mirrors agentworkforce/relay's `swarm_tui::run_tui`: a
//! `tokio::select!` over (key events, pane updates, render tick), with a
//! `TerminalGuard` restoring raw mode + screen on every exit path.
//!
//! The pure helpers ([`resolve_agent_names`], [`key_to_input`],
//! [`render_to`]) are unit-tested; the async wiring is exercised live (it
//! needs a daemon + agents, covered by the manual run path).

use std::io::{self, Write};
use std::time::Duration;

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::{cursor, queue, style, terminal};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::client::{ensure_daemon, resolve_daemon_bin};
use crate::grid::group::{self as group, GroupKey, RailState};
use crate::grid::layout::{self as layout, LayoutError, PageLayout, TtySize};
use crate::grid::leader::{self, LeaderDecision, LeaderState};
use crate::grid::palette::Palette;
use crate::grid::pane::{CellColor, Pane, PaneSnapshot, RenderCell};
use crate::grid::repo;
use crate::grid::squads;
use crate::grid::state::{
    off_screen_waiting_by_page, Compositor, CompositorAction, ConnAction, ConnEvent, ConnState,
    InputEvent, Mode,
};
use crate::grid::{apply_soft_cap, max_panes, GridArgs};
use crate::paths::AgentsHome;
use crate::protocol::{read_response, write_request, Request, ResponsePayload};
use crate::state::HOST_MODE_INTERACTIVE;

type Ws = WebSocketStream<UnixStream>;
type WsSink = futures_util::stream::SplitSink<Ws, Message>;
type CellStyle = (CellColor, CellColor, bool, bool, bool, bool);
type WatchOpen = (Pane, ConnState, Option<WsSink>);
/// Type alias for the rail-mode paint argument (ab-1fab1fdf, Phase 1).
/// Bundles the rail state + computed groups + badges + registry rows
/// so the `paint` signature stays readable.
type RailPaintArg<'a> = Option<(
    &'a RailState,
    &'a [group::Group],
    &'a [group::GroupBadge],
    &'a [Value],
)>;

const PING_INTERVAL: Duration = Duration::from_secs(3);

/// How long a claude name that failed the `worker.ping` PTY gate is kept out of
/// the live-discover probe set (x-c226). A stable stream-json lane (host_mode=
/// interactive, serves only `stream.*`) fails forever, so without this it is
/// re-probed at 500ms every 3s tick, starving grid input. 60s means a stable
/// lane re-probes once/min (negligible) while a slow-starting real child still
/// tiles within <=60s. Monotonic `Instant`, never wall-clock (NTP-safe).
const FAILED_PROBE_TTL: Duration = Duration::from_secs(60);

/// A message from a per-pane reader task to the main loop.
enum PaneMsg {
    /// PTY output bytes for pane `idx`.
    Bytes(usize, Vec<u8>),
    /// The agent for pane `idx` exited with `code`.
    Exited(usize, i32),
    /// The watcher WS for pane `idx` closed / errored.
    Closed(usize, String),
    /// Result of one detached discovery run (x-87f7). The ping arm spawns the
    /// discovery I/O off the loop task; this carries its resolved output back so
    /// the loop applies the cheap, in-memory tiling without ever awaiting I/O.
    Discovered(DiscoveryResult),
}

/// Output of one [`discover_children`] run, applied on the `rx.recv()` arm
/// (x-87f7). Carries the RESOLVED tiling inputs (name + host_mode + the child's
/// real registry rail row) so the apply arm never re-reads the registry on the
/// loop task (Locked Decision 2). `probed`/`survived` drive the cache write-back
/// applied against the live cache on apply.
struct DiscoveryResult {
    /// Survivors to tile: (name, host_mode, real registry rail row).
    panes: Vec<(String, bool, Value)>,
    /// Fresh `--bg` roster cards (already deduped vs the registry rows by
    /// `roster_bg_cards_from`; the apply arm additionally dedups vs the live
    /// `roster_cards`).
    cards: Vec<RosterBgCard>,
    /// Every name the `worker.ping` probe ran for this run (cache write-back domain).
    probed: Vec<String>,
    /// The subset that passed the probe (cleared from the cache; never cached).
    survived: Vec<String>,
}

/// Resolve the agent names to tile.
///
/// For explicit names, returns them in order. For `--all`, reads the
/// registry and returns every PTY-managed agent (codex / gemini, plus
/// interactive PTY-hosted claude since E2) that is in a live-ish status.
/// This is the cheap host_mode pre-filter; the authoritative drop of the
/// adopted stream-json claude lane (which also carries `host_mode ==
/// "interactive"` and binds a worker socket, but serves only `stream.*`)
/// happens in [`run`] via the `worker.ping` protocol probe ([`survives_pty_gate`]
/// / [`worker_speaks_pty`]). Missing registry ⇒ empty (the caller errors on
/// empty).
pub(crate) fn resolve_agent_names(
    parsed: &GridArgs,
    home: &AgentsHome,
) -> Result<Vec<String>, String> {
    if !parsed.all {
        return Ok(parsed.names.clone());
    }
    let path = home.registry_json();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("registry read failed: {e}")),
    };
    let raw: Value = serde_json::from_str(
        std::str::from_utf8(&bytes).map_err(|e| format!("registry utf8: {e}"))?,
    )
    .map_err(|e| format!("registry json: {e}"))?;
    let rows = raw
        .get("agents")
        .or_else(|| raw.get("entries"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(filter_pty_agents(&rows))
}

/// Decide whether a tiled candidate survives the PTY-protocol gate. Non-claude
/// rows always survive (codex/gemini are always generic PTY workers). A claude
/// row survives only if its worker answered `worker.ping` - the PTY lane. The
/// adopted stream-json lane (serves only `stream.*`) and any socketless row do
/// not. Pure for testability; the I/O lives in [`worker_speaks_pty`].
fn survives_pty_gate(provider: Option<&str>, claude_ping_ok: bool) -> bool {
    provider != Some("claude") || claude_ping_ok
}

/// Probe a worker socket with `worker.ping`: true iff a generic PTY worker
/// answers. The adopted claude stream lane binds the same `worker_sock` path
/// (`stream_worker.rs`) but serves only `stream.*`, so `worker.ping` errors -
/// the authoritative discriminator between the two interactive-marked claude
/// lanes. Connect / timeout / parse failure all read as "not a PTY worker".
async fn worker_speaks_pty(home: &AgentsHome, short_id: &str) -> bool {
    use crate::protocol::{read_response, write_request, Request};
    if short_id.is_empty() {
        return false;
    }
    let sock = home.worker_sock(short_id);
    let probe = Duration::from_millis(500);
    let mut conn = match tokio::time::timeout(probe, UnixStream::connect(&sock)).await {
        Ok(Ok(c)) => c,
        _ => return false,
    };
    if write_request(&mut conn, &Request::new(1, "worker.ping", json!({})))
        .await
        .is_err()
    {
        return false;
    }
    matches!(
        tokio::time::timeout(probe, read_response(&mut conn)).await,
        Ok(Ok(resp)) if !resp.is_err()
    )
}

/// Drop claude names whose worker does not speak the PTY protocol (the adopted
/// stream-json lane), so `--all` never tiles a non-drivable phantom claude pane
/// (codex review P2). codex/gemini pass through without a probe. The registry is
/// read once to resolve each name's provider + short_id.
async fn prune_non_pty_claude(names: Vec<String>, home: &AgentsHome) -> Vec<String> {
    let rows = read_registry_rows(home);
    prune_non_pty_claude_with_rows(names, &rows, home).await
}

/// As [`prune_non_pty_claude`] but over already-parsed registry `rows`, so a
/// caller that already holds them (the x-45e6 poll, which reads the registry
/// once per tick) does not re-read and re-parse the file.
async fn prune_non_pty_claude_with_rows(
    names: Vec<String>,
    rows: &[Value],
    home: &AgentsHome,
) -> Vec<String> {
    let field = |name: &str, key: &str| -> Option<String> {
        rows.iter()
            .find(|r| r.get("name").and_then(Value::as_str) == Some(name))
            .and_then(|r| r.get(key).and_then(Value::as_str))
            .map(str::to_string)
    };
    // Probe the candidates CONCURRENTLY (x-c226): the serial loop stalled the
    // grid select! arm for N x 500ms. `join_all` over borrowed futures caps a
    // burst at one ~500ms window. `join_all` (not `JoinSet`) because each probe
    // borrows `&home` - no `'static + Send` requirement, no socket-context clone.
    // Each future resolves to `(name, survives)` so the survivor/failure split
    // is by the returned name, not a fragile index into a separate vec.
    let probes = names.into_iter().map(|name| {
        // Resolve provider/short_id synchronously so the async block borrows
        // only `home` (the `rows`-borrowing `field` closure stays out of it).
        let provider = field(&name, "provider");
        let short_id = field(&name, "short_id").unwrap_or_default();
        async move {
            let ping_ok = if provider.as_deref() == Some("claude") {
                worker_speaks_pty(home, &short_id).await
            } else {
                false // unused for non-claude (survives_pty_gate ignores it)
            };
            (name, survives_pty_gate(provider.as_deref(), ping_ok))
        }
    });
    futures_util::future::join_all(probes)
        .await
        .into_iter()
        .filter_map(|(name, survives)| survives.then_some(name))
        .collect()
}

/// PTY-driveable providers. claude joins codex/gemini in E2, but ONLY in its
/// interactive face (see `filter_pty_agents`): the daemon PTY-hosts interactive
/// subscription-billed claude via the generic worker path (E1 keystone), while
/// the `claude -p` stream-json lane is headless, Agent-SDK-billed, and not a
/// drivable TUI - it stays out of the grid.
const PTY_PROVIDERS: &[&str] = &["codex", "gemini", "claude"];
/// Registry statuses we will try to tile under `--all` (alive-ish).
const ALIVE_STATUSES: &[&str] = &["ready", "idle", "busy", "live", "spawning"];

/// Filter registry rows to live PTY-managed agent names (pure; testable).
///
/// claude is admitted only when `host_mode == "interactive"`, the cheap
/// pre-filter that drops the `exec` `--bg` lane. NOTE this does NOT by itself
/// separate the two interactive-marked claude lanes: the adopted stream-json
/// lane ALSO sets `host_mode == "interactive"` (to keep reconcile from settling
/// it `exited`, `build_claude_stream_entry`) AND binds a worker socket, but
/// serves only `stream.*`. The authoritative split is the `worker.ping`
/// protocol probe ([`prune_non_pty_claude`]) applied in [`run`]. codex/gemini
/// are unconditional (their grid behavior is unchanged).
fn filter_pty_agents(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .filter_map(|row| {
            let provider = row.get("provider").and_then(Value::as_str)?;
            if !PTY_PROVIDERS.contains(&provider) {
                return None;
            }
            if provider == "claude" {
                let interactive =
                    row.get("host_mode").and_then(Value::as_str) == Some(HOST_MODE_INTERACTIVE);
                if !interactive {
                    return None;
                }
            }
            let status = row.get("status").and_then(Value::as_str).unwrap_or("live");
            if !ALIVE_STATUSES.contains(&status) {
                return None;
            }
            row.get("name").and_then(Value::as_str).map(str::to_string)
        })
        .collect()
}

/// Per-name interactive-ness (host_mode == "interactive"), aligned 1:1 with
/// `names`. A name that is absent from the registry, carries no `host_mode`,
/// or is `exec` reads as `false` - the safe default (exec agents are one-shot
/// and only monitorable, never drivable). Pure for testability.
fn host_modes_from_rows(rows: &[Value], names: &[String]) -> Vec<bool> {
    names
        .iter()
        .map(|name| {
            rows.iter().any(|row| {
                row.get("name").and_then(Value::as_str) == Some(name.as_str())
                    && row.get("host_mode").and_then(Value::as_str) == Some(HOST_MODE_INTERACTIVE)
            })
        })
        .collect()
}

/// Resolve `host_modes_from_rows` against the on-disk registry. A missing or
/// unparseable registry yields all-`false` (every pane watch-only), matching
/// the absent==exec coercion the rest of fno-agents uses.
fn resolve_host_modes(names: &[String], home: &AgentsHome) -> Vec<bool> {
    let bytes = match std::fs::read(home.registry_json()) {
        Ok(b) => b,
        Err(_) => return vec![false; names.len()],
    };
    let raw: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            // Present-but-corrupt registry, distinct from "absent" (the read arm
            // above, a normal pre-daemon state). Surface it so a corrupt store is
            // not silently masked as "every pane watch-only". Printed before the
            // TUI raw-mode guard is entered, so it reaches the terminal cleanly.
            eprintln!("fno-agents grid: registry unparseable ({e}); all panes watch-only");
            return vec![false; names.len()];
        }
    };
    let rows = raw
        .get("agents")
        .or_else(|| raw.get("entries"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    host_modes_from_rows(rows, names)
}

// ── claude --bg roster cards (x-57eb, Option A) ───────────────────────────────
//
// `claude --bg` sessions are interactive threads claude's OWN daemon owns
// (`~/.claude/daemon/roster.json`). They carry no fno worker socket, so the grid
// never enumerated them (`resolve_agent_names` reads the fno registry only). We
// surface each roster-only session as a STATUS CARD (cwd + age + `↵ attach`) and
// route its drive to native `claude attach` - NOT an fno-rendered PTY (that would
// be the rejected Option B). The roster read side is `crate::claude_roster`.

/// One claude `--bg` session resolved into a grid status card. Owned data so it
/// outlives the borrowed roster it was read from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RosterBgCard {
    /// The session short id (first 8-hex segment); the pane name + display key.
    pub name: String,
    /// Full session UUID - what native `claude attach <id>` consumes.
    pub session_id: String,
    /// The worker's cwd; its basename leads the card.
    pub cwd: String,
    /// Worker start time (unix secs) when the roster carried a parseable epoch;
    /// `None` for the human-date-string drift (>=2.1.195), which degrades age to
    /// "?" rather than failing.
    pub proc_start: Option<u64>,
}

/// The roster-only `--bg` cards: claude roster workers whose session is NOT
/// already represented in the fno registry. Dedup by short_id AND full
/// session_id; the registry row WINS on overlap (an fno-spawned interactive
/// claude is already a registry pane, so its roster twin must not double-list).
/// Pure for testability; the I/O wrapper is [`resolve_bg_roster_cards`].
pub(crate) fn roster_bg_cards_from(
    registry_rows: &[Value],
    roster: &[&crate::claude_roster::RosterWorker],
) -> Vec<RosterBgCard> {
    let mut reg_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for r in registry_rows {
        if let Some(s) = r.get("short_id").and_then(Value::as_str) {
            reg_ids.insert(s);
        }
        if let Some(s) = r.get("session_id").and_then(Value::as_str) {
            reg_ids.insert(s);
        }
    }
    roster
        .iter()
        .filter(|w| !reg_ids.contains(w.session_id.as_str()) && !reg_ids.contains(w.short_id()))
        .map(|w| RosterBgCard {
            name: w.short_id().to_string(),
            session_id: w.session_id.clone(),
            cwd: w.cwd.clone(),
            proc_start: w.proc_start,
        })
        .collect()
}

/// Read the fno registry + the claude daemon roster and return the roster-only
/// `--bg` cards. A missing / unparseable roster yields zero cards and never
/// errors (the grid degrades to "no bg sessions", AC-ERR). The registry read
/// mirrors the other enumeration sites (absent => no rows).
fn resolve_bg_roster_cards(home: &AgentsHome) -> Vec<RosterBgCard> {
    let registry_rows: Vec<Value> = std::fs::read(home.registry_json())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| {
            v.get("agents")
                .or_else(|| v.get("entries"))
                .and_then(Value::as_array)
                .cloned()
        })
        .unwrap_or_default();
    // load_default already degrades a missing roster to empty; an unparseable
    // one (Err) also yields zero cards here rather than failing grid startup.
    let roster = match crate::claude_roster::ClaudeRoster::load_default() {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    roster_bg_cards_from(&registry_rows, &roster.workers_deduped())
}

/// Human age label from a `proc_start` unix-secs epoch vs `now`. `None` (the
/// date-string drift) or a future timestamp degrade to "?". Pure.
//
// ponytail: assumes `proc_start` is seconds. In practice the live roster carries
// a date STRING that parses to None, so this branch is rarely hit; a numeric
// epoch unit mismatch would only mis-scale a cosmetic age label, never break.
fn bg_age_label(proc_start: Option<u64>, now: u64) -> String {
    match proc_start {
        Some(t) if now >= t => {
            let secs = now - t;
            if secs < 3600 {
                format!("{}m", secs / 60)
            } else if secs < 86_400 {
                format!("{}h", secs / 3600)
            } else {
                format!("{}d", secs / 86_400)
            }
        }
        _ => "?".to_string(),
    }
}

/// Current wall clock in unix seconds (0 on a pre-epoch clock error).
fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Build a static status-card [`Pane`] for a claude `--bg` session: cwd
/// basename, age, and the `↵ attach` affordance. No worker socket and no WS
/// stream are opened (Option A) - the card text is fed once into the pane's
/// emulator. CRLF line ends because the emulator runs in raw mode.
fn bg_card_pane(card: &RosterBgCard, rows: u16, cols: u16, now: u64) -> Pane {
    let mut pane = Pane::new(rows, cols);
    let base = card
        .cwd
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(card.cwd.as_str());
    let age = bg_age_label(card.proc_start, now);
    let text = format!(
        "\r\n  claude --bg\r\n  {}\r\n  cwd  {base}\r\n  age  {age}\r\n\r\n  \u{21b5} attach\r\n",
        card.name,
    );
    pane.feed(text.as_bytes());
    pane
}

/// Suspend the grid's raw mode + alternate screen, exec native
/// `claude attach <session_id>` with inherited stdio, then RESTORE the terminal
/// so the operator lands back in the grid (not the native detach-to-empty-shell
/// seam). The terminal is restored regardless of the attach outcome; the caller
/// forces a full repaint (`prev_frame = None`) and surfaces any error via the
/// footer hint. (x-57eb, Change 3)
///
/// `async` + `tokio::process` is load-bearing on the current-thread runtime: a
/// synchronous `std::process::Command::status()` would block the SOLE executor
/// thread for the whole (interactive, possibly minutes-long) attach, starving
/// every spawned task - including the watch-sink readers draining the other
/// panes' WS streams (gemini review HIGH on PR #96). Awaiting the child keeps
/// the executor free to drain them while the operator is attached; the grid is
/// paused (alt screen left) but its background tasks stay alive.
async fn attach_bg_session(session_id: &str) -> Result<(), String> {
    use crossterm::{
        cursor,
        event::{DisableMouseCapture, EnableMouseCapture},
        execute, terminal,
    };
    // Suspend (mirror TerminalGuard::drop) so claude attach owns a clean TTY.
    // termios / alt-screen state is process-global, so the sync crossterm calls
    // are correct regardless of which thread later runs the child.
    let _ = execute!(
        io::stderr(),
        DisableMouseCapture,
        terminal::LeaveAlternateScreen,
        cursor::Show
    );
    let _ = terminal::disable_raw_mode();

    // Inherit stdio (the default) so the child owns the real TTY interactively.
    let status = tokio::process::Command::new("claude")
        .arg("attach")
        .arg(session_id)
        .status()
        .await;

    // Restore (mirror TerminalGuard::enter) ALWAYS, even on attach failure, so
    // the grid is never left in raw mode / on the alt screen.
    let _ = terminal::enable_raw_mode();
    let _ = execute!(
        io::stderr(),
        terminal::EnterAlternateScreen,
        EnableMouseCapture,
        cursor::Hide
    );

    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!(
            "claude attach exited {}",
            s.code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "by signal".into())
        )),
        Err(e) => Err(format!("claude attach failed: {e} (is `claude` on PATH?)")),
    }
}

/// Map a bare crossterm key to a [`Keystroke`](InputEvent::Keystroke) for the
/// focused pane's owned PTY.
///
/// Owned-PTY model (x-1356): every bare key forwards to the focused agent -
/// there is no WATCH that eats keys and no Esc-releases-drive. Ctrl-C maps to
/// the `0x03` control byte via [`key_to_bytes`] and reaches the agent (so you
/// can interrupt it); quitting the grid is a leader command ([`leader_command`]),
/// never a bare key. An unmappable key yields `None` (dropped, not guessed).
fn key_to_input(key: KeyEvent) -> Option<InputEvent> {
    key_to_bytes(key).map(InputEvent::Keystroke)
}

/// Map a post-leader key to a multiplexer [`InputEvent`] (focus / page / quit).
///
/// Pure + testable. This is the command set the leader unlocks - the former
/// WATCH keymap minus the promote/scrollback verbs that watch mode removed.
/// A bare key never reaches here (it forwards to the agent); only a key the
/// operator typed AFTER the leader does.
fn leader_command(key: KeyEvent) -> Option<InputEvent> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && matches!(key.code, KeyCode::Char('c')) {
        return Some(InputEvent::Quit);
    }
    match key.code {
        KeyCode::Tab | KeyCode::Right | KeyCode::Down => Some(InputEvent::FocusNext),
        KeyCode::BackTab | KeyCode::Left | KeyCode::Up => Some(InputEvent::FocusPrev),
        // Page navigation. `]`/`[` are the always-available pair; PgDn/PgUp
        // alias them. Inert when single-page (Compositor::page_* clamp).
        KeyCode::Char(']') | KeyCode::PageDown => Some(InputEvent::PageNext),
        KeyCode::Char('[') | KeyCode::PageUp => Some(InputEvent::PagePrev),
        // Scrollback entry, rebound from the former bare `Space` to leader+Space
        // (bare Space now forwards to the agent like any other key, x-1356).
        KeyCode::Char(' ') => Some(InputEvent::EnterScrollback),
        KeyCode::Char('q') => Some(InputEvent::Quit),
        _ => None,
    }
}

/// Map a bare key to a scrollback [`InputEvent`] while the focused pane is
/// frozen ([`Mode::Scrollback`]). `Esc` exits; everything unmapped is inert so
/// a stray key never leaks to the frozen agent. Pure + testable, mirroring
/// [`key_to_input`] for the scrollback keymap.
fn scrollback_key(key: KeyEvent) -> Option<InputEvent> {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => Some(InputEvent::ScrollLineUp),
        KeyCode::Down | KeyCode::Char('j') => Some(InputEvent::ScrollLineDown),
        KeyCode::PageUp => Some(InputEvent::ScrollPageUp),
        KeyCode::PageDown => Some(InputEvent::ScrollPageDown),
        KeyCode::Char('g') | KeyCode::Home => Some(InputEvent::ScrollTop),
        KeyCode::Char('G') | KeyCode::End => Some(InputEvent::ScrollBottom),
        KeyCode::Esc => Some(InputEvent::ExitScrollback),
        _ => None,
    }
}

/// Encode a key event as the raw bytes a PTY expects (DRIVE mode input).
/// Covers the common printable + control keys; anything unmapped yields
/// `None` (dropped) rather than guessing a byte sequence.
fn key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl-<letter> → control byte (Ctrl-A = 0x01 ..).
                let upper = c.to_ascii_uppercase();
                if upper.is_ascii_uppercase() {
                    return Some(vec![(upper as u8) - b'A' + 1]);
                }
            }
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        // PgUp/PgDn alias page keys in WATCH; in DRIVE they forward to the
        // agent like any other key (AC3-ERR). `[`/`]` forward via the
        // Char arm above.
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        _ => None,
    }
}

// ── Launcher (E5b: zero-config front door + one-tap orchestration) ───────

/// What the goal-launcher line should do with a key. Pure + testable,
/// mirroring [`key_to_input`]. The launcher is a modal one-line text input the
/// operator opens to type a goal; on submit the grid spawns a `/target` worker
/// for it and tiles the worker live.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LauncherAction {
    /// Append a printable char to the goal buffer.
    Append(char),
    /// Delete the last char.
    Backspace,
    /// Submit the buffer (a plain goal spawns a `/target` worker; a `>`-prefixed
    /// buffer opens an interactive claude pane -- see [`classify_launch`]).
    Submit,
    /// Close the launcher without spawning (Esc / Ctrl-C).
    Cancel,
    /// Inert key.
    Ignore,
}

/// Map a key to a [`LauncherAction`]. Esc / Ctrl-C cancel; Enter submits;
/// Backspace edits; any non-control printable char accumulates. Everything
/// else is inert (arrows etc. are intentionally not cursor-movement in this
/// minimal single-line input).
fn launcher_key(key: KeyEvent) -> LauncherAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && matches!(key.code, KeyCode::Char('c')) {
        return LauncherAction::Cancel;
    }
    match key.code {
        KeyCode::Esc => LauncherAction::Cancel,
        KeyCode::Enter => LauncherAction::Submit,
        KeyCode::Backspace => LauncherAction::Backspace,
        KeyCode::Char(c) if !ctrl && !c.is_control() => LauncherAction::Append(c),
        _ => LauncherAction::Ignore,
    }
}

/// What the run loop must do after feeding a key to the launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LauncherOutcome {
    /// Buffer changed or key ignored: keep the launcher open.
    Stay,
    /// Operator cancelled: close the launcher.
    Cancelled,
    /// Operator submitted a non-blank goal: close the launcher and spawn it.
    Submitted(String),
}

/// Run-loop state for the goal launcher. Present (`Some`) only while the
/// operator is typing a goal.
struct Launcher {
    buffer: String,
}

impl Launcher {
    fn new() -> Self {
        Launcher {
            buffer: String::new(),
        }
    }

    /// Apply a launcher action to the buffer and report what the run loop
    /// should do. A blank (whitespace-only) Submit is a no-op (AC1-EDGE): the
    /// launcher stays open rather than spawning an empty `/target`.
    fn apply(&mut self, action: LauncherAction) -> LauncherOutcome {
        match action {
            LauncherAction::Append(c) => {
                self.buffer.push(c);
                LauncherOutcome::Stay
            }
            LauncherAction::Backspace => {
                self.buffer.pop();
                LauncherOutcome::Stay
            }
            LauncherAction::Cancel => LauncherOutcome::Cancelled,
            LauncherAction::Submit => {
                let goal = self.buffer.trim().to_string();
                if goal.is_empty() {
                    LauncherOutcome::Stay
                } else {
                    LauncherOutcome::Submitted(goal)
                }
            }
            LauncherAction::Ignore => LauncherOutcome::Stay,
        }
    }
}

/// Modal prompt for recruiting the focused agent into a squad (x-5b3e). Reuses
/// the single-line [`Launcher`] buffer for the squad name; `agent` is the
/// registry name being recruited, captured when `m` opens the prompt so a later
/// pane churn can never retarget the recruit (Concurrency: the recruit binds to
/// the name decided at keypress, not whatever is focused at submit).
struct RecruitPrompt {
    agent: String,
    input: Launcher,
    /// Existing squad names the operator can cycle into the buffer with Up/Down
    /// (x-0175 US1). Empty when no squads exist yet (cycling is then inert).
    candidates: Vec<String>,
    /// Index into `candidates` of the currently-cycled name; seeded onto the
    /// last-recruited squad when present (AC2-HP).
    cursor: usize,
}

/// Derive a legible, unique-ish worker name from a goal + a monotonic counter:
/// `target-<slug>-<n>`. The slug is the goal's leading alphanumeric words,
/// lowercased and dash-joined (capped), so the name reads cleanly in
/// `fno agents list` and the rail. Pure + testable.
fn target_worker_name(goal: &str, n: usize) -> String {
    let mut slug = String::new();
    let mut prev_dash = true; // suppress a leading dash
    for c in goal.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            prev_dash = false;
            if slug.len() >= 24 {
                break;
            }
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "goal" } else { slug };
    format!("target-{slug}-{n}")
}

/// Build the recruit modal's `(candidates, cursor)` (x-0175 US1): existing squad
/// names in store order, with the cursor pre-positioned on `last`'s index when
/// it is still a known squad (Discretion #2 / AC2-HP fast repeat-recruit), else
/// 0. A `last` that no longer exists (squad removed mid-session) falls back to 0
/// without crashing (AC2-FR). Pure + unit-tested.
fn recruit_candidates(store: &squads::SquadStore, last: Option<&str>) -> (Vec<String>, usize) {
    let candidates: Vec<String> = store.squads.iter().map(|s| s.name.clone()).collect();
    let cursor = last
        .and_then(|l| candidates.iter().position(|c| c == l))
        .unwrap_or(0);
    (candidates, cursor)
}

/// Next cursor for an Up/Down cycle over `len` recruit candidates. `on_candidate`
/// is true when the buffer currently shows `candidates[cursor]` (the press
/// advances to the neighbour); false when the buffer diverged - empty or freely
/// typed - so the press snaps back onto the cursor candidate without advancing,
/// guaranteeing the first press always changes the buffer (AC1-UI). Wraps at both
/// ends (Discretion #4); inert with no candidates (AC1-EDGE). Pure + unit-tested.
fn cycle_cursor(len: usize, cursor: usize, on_candidate: bool, down: bool) -> usize {
    if len == 0 {
        return cursor;
    }
    if !on_candidate {
        return cursor.min(len - 1);
    }
    if down {
        (cursor + 1) % len
    } else {
        (cursor + len - 1) % len
    }
}

/// Render the recruit modal's candidate legend: existing squad names with the
/// cursor one wrapped in single guillemets, e.g. `stack ‹crew› ops`, so the
/// operator sees the squads they can pick and which one is current (US1 /
/// AC1-UI). Empty string when no squads exist, so the footer collapses to the
/// bare prompt (AC1-EDGE). Narrow terminals truncate it via the footer's
/// existing hint clip (Discretion #3). Pure + unit-tested.
fn candidate_legend(candidates: &[String], cursor: usize) -> String {
    candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if i == cursor {
                format!("\u{2039}{c}\u{203a}")
            } else {
                c.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The recruit modal's full footer hint: the editable squad-name buffer plus the
/// candidate legend when squads exist. One place so the m-open, typing, and
/// cycle paths render an identical footer.
fn recruit_hint(rp: &RecruitPrompt) -> String {
    let legend = candidate_legend(&rp.candidates, rp.cursor);
    if legend.is_empty() {
        format!("recruit {} into squad> {}", rp.agent, rp.input.buffer)
    } else {
        format!(
            "recruit {} into squad> {}   [{}]",
            rp.agent, rp.input.buffer, legend
        )
    }
}

/// Spawn a `/target` worker for `goal` via the same primitive dispatch uses:
/// `fno agents spawn --provider claude <name> "/target no-merge <goal>"`.
/// `no-merge` keeps an autonomous worker landing a PR for review, never an
/// auto-merge (dispatch Locked Decision 4). `$FNO_BIN` overrides the binary
/// (tests / non-PATH installs). Returns Ok once the worker is launched.
///
/// x-3ab8: `spawn` now defaults to an owned interactive pane, so this launches
/// the `/target` worker INTO the grid as a drivable, take-over-able pane (no
/// `--once`) rather than claude's detached `--bg` thread - the unification that
/// makes a launched worker tile live (Change 4 AC-HP). The spawn command is
/// unchanged; the default flip does the work.
async fn spawn_target_worker(name: &str, goal: &str) -> Result<(), String> {
    // var_os (not var): FNO_BIN is a path and may carry non-UTF-8 bytes;
    // Command::new takes an OsStr, so no lossy conversion is needed.
    let fno = std::env::var_os("FNO_BIN")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "fno".into());
    let cmd = format!("/target no-merge {goal}");
    let out = tokio::process::Command::new(&fno)
        .args(["agents", "spawn", "--provider", "claude", name, &cmd])
        .output()
        .await
        .map_err(|e| format!("spawn exec failed: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let last = stderr.lines().last().unwrap_or("unknown").trim();
        Err(format!("spawn failed: {last}"))
    }
}

/// How an E5b launcher submission should be dispatched (x-1b1c Change 2). A
/// leading `>` opens a BARE interactive claude pane the operator types straight
/// into (the herdr UX this grid is for); anything else dispatches a `/target`
/// worker for the goal. Post-x-3ab8 BOTH tile as owned interactive panes - the
/// distinction is the payload (bare pane vs a pane running `/target`), not a
/// watch-vs-drive tier (there is none). Pure + testable, mirroring
/// [`launcher_key`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum LaunchKind {
    /// Interactive claude pane; the string is the optional first message.
    InteractivePane(String),
    /// Autonomous `/target` worker for this goal.
    TargetWorker(String),
}

fn classify_launch(input: &str) -> LaunchKind {
    match input.strip_prefix('>') {
        Some(rest) => LaunchKind::InteractivePane(rest.trim().to_string()),
        None => LaunchKind::TargetWorker(input.trim().to_string()),
    }
}

/// Name for a launcher-spawned interactive claude pane: `claude-<n>`. Distinct
/// from `target-<slug>-<n>` so the two launch kinds read apart in the rail.
fn interactive_pane_name(n: usize) -> String {
    format!("claude-{n}")
}

/// Spawn an interactive, owned claude pane via the host lane (x-1b1c Change 1):
/// `fno agents host <name> --provider claude --mode interactive [message]`.
/// Unlike [`spawn_target_worker`] (an autonomous `--bg /target` worker), this
/// tiles as a live panel the operator types straight into. `$FNO_BIN` overrides
/// the binary (tests / non-PATH installs). Returns Ok once the pane is hosted.
async fn spawn_interactive_claude_pane(name: &str, message: &str) -> Result<(), String> {
    let fno = std::env::var_os("FNO_BIN")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "fno".into());
    let mut args: Vec<String> = vec![
        "agents".into(),
        "host".into(),
        "--provider".into(),
        "claude".into(),
        "--mode".into(),
        "interactive".into(),
        name.to_string(),
    ];
    // Pass the first message via --message (not as a trailing positional) so a
    // goal that happens to start with `--` is taken as text, not parsed as a
    // flag by the host verb.
    if !message.is_empty() {
        args.push("--message".into());
        args.push(message.to_string());
    }
    let out = tokio::process::Command::new(&fno)
        .args(&args)
        .output()
        .await
        .map_err(|e| format!("host exec failed: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let last = stderr.lines().last().unwrap_or("unknown").trim();
        Err(format!("host failed: {last}"))
    }
}

// ── Rendering ───────────────────────────────────────────────────────────

/// A full-terminal cell grid. The compositor rasterizes one frame into this,
/// then either paints it whole ([`emit_full`]) or diffs it against the
/// previous frame and paints only the cells that changed ([`emit_diff`]).
///
/// Reusing [`RenderCell`] as the cell type lets the pane interior copy its
/// snapshot cells in 1:1 and lets the SGR-diff helper ([`apply_cell_style`])
/// run unchanged over both paint paths. Frame-over-frame diffing is what lets
/// the steady-state repaint drop the per-frame `Clear(All)` that used to make
/// the grid flicker on every PTY byte: an unchanged frame now emits nothing,
/// and a frame where one pane scribbled a line emits only that line's cells.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScreenBuffer {
    rows: u16,
    cols: u16,
    /// Row-major, length exactly `rows * cols`. Index via [`ScreenBuffer::idx`].
    cells: Vec<RenderCell>,
    /// Foreground color applied to chrome cells written via `put_str`.
    /// `CellColor::Default` preserves the pre-palette behavior.
    chrome_fg: CellColor,
    /// Background color applied to chrome cells written via `put_str`.
    /// `CellColor::Default` preserves the pre-palette behavior.
    chrome_bg: CellColor,
}

impl ScreenBuffer {
    /// A blank buffer of the given size (every cell default: empty glyph,
    /// default colors, no attributes). Dimensions are clamped to >= 1 so the
    /// index math never produces a zero-length grid.
    fn blank(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        ScreenBuffer {
            rows,
            cols,
            cells: vec![RenderCell::default(); rows as usize * cols as usize],
            chrome_fg: CellColor::Default,
            chrome_bg: CellColor::Default,
        }
    }

    /// Like `blank`, but bakes a palette's chrome fg/bg into the buffer so
    /// that `put_str` produces palette-colored cells instead of Default.
    /// With `Palette::fixed()` colors (`CellColor::Default`), the result is
    /// identical to `blank()`.
    fn with_chrome(rows: u16, cols: u16, chrome_fg: CellColor, chrome_bg: CellColor) -> Self {
        let mut buf = Self::blank(rows, cols);
        buf.chrome_fg = chrome_fg;
        buf.chrome_bg = chrome_bg;
        buf
    }

    #[inline]
    fn idx(&self, row: u16, col: u16) -> usize {
        row as usize * self.cols as usize + col as usize
    }

    fn get(&self, row: u16, col: u16) -> Option<&RenderCell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.cells.get(self.idx(row, col))
    }

    /// Place a cell. Out-of-bounds writes are dropped (chrome math can run a
    /// column past the edge on a degenerate winsize).
    fn set(&mut self, row: u16, col: u16, cell: RenderCell) {
        if row < self.rows && col < self.cols {
            let i = self.idx(row, col);
            self.cells[i] = cell;
        }
    }

    /// Write a plain chrome string starting at `(row, col)`, one cell per
    /// char, optionally inverse-video (the focused title bar). Uses the
    /// buffer's `chrome_fg`/`chrome_bg` for fg/bg (Default when created via
    /// `blank()`; palette colors when created via `with_chrome()`). Stops at
    /// the right edge.
    fn put_str(&mut self, row: u16, col: u16, s: &str, inverse: bool) {
        // Walk a column cursor rather than `col + i as u16`: the cast would
        // wrap if `s` ever exceeded 65535 chars (it cannot today, but the
        // cursor form removes the cast entirely). (gemini-code-assist, PR #386)
        let chrome_fg = self.chrome_fg;
        let chrome_bg = self.chrome_bg;
        let mut c = col;
        for ch in s.chars() {
            if c >= self.cols {
                break;
            }
            self.set(
                row,
                c,
                RenderCell {
                    text: ch.to_string(),
                    fg: chrome_fg,
                    bg: chrome_bg,
                    inverse,
                    ..RenderCell::default()
                },
            );
            c = c.saturating_add(1);
        }
    }
}

/// Emit one cell's glyph to `out`, prefixing the SGR transition only when the
/// style differs from the previous cell in this run (`last`). Shared by both
/// paint paths so styling is identical whether a cell arrives via a full paint
/// or a diff.
///
/// A wide-char spacer (the second column of a CJK/emoji glyph) emits NOTHING:
/// the preceding wide glyph already advanced the terminal cursor across both
/// columns, so printing anything here would either overwrite the glyph's right
/// half or shift the row. Skipping it (rather than the old "print a space")
/// fixes a diff-path corruption where the spacer's space overwrote an
/// unchanged following cell that the diff then never restored (chatgpt-codex,
/// PR #386). A genuinely blank cell (`text` empty, NOT a spacer) still renders
/// as a space so the diff can erase stale content.
fn emit_cell<W: Write>(
    out: &mut W,
    cell: &RenderCell,
    last: &mut Option<CellStyle>,
) -> io::Result<()> {
    if cell.wide_spacer {
        return Ok(());
    }
    apply_cell_style(out, cell, last)?;
    let ch = if cell.text.is_empty() {
        " "
    } else {
        &cell.text
    };
    queue!(out, style::Print(ch))
}

/// Paint every cell of `frame`, row by row. Used for the first frame and after
/// a resize (paired with a one-time `Clear`), where there is no prior screen to
/// diff against. Each row is one contiguous run, so multi-cell strings stay
/// byte-contiguous in the output (the render tests assert on substrings).
fn emit_full<W: Write>(frame: &ScreenBuffer, out: &mut W) -> io::Result<()> {
    for r in 0..frame.rows {
        queue!(out, cursor::MoveTo(0, r))?;
        let mut last: Option<CellStyle> = None;
        for c in 0..frame.cols {
            if let Some(cell) = frame.get(r, c) {
                emit_cell(out, cell, &mut last)?;
            }
        }
        queue!(out, style::SetAttribute(style::Attribute::Reset))?;
    }
    Ok(())
}

/// Paint only the cells that differ between `prev` and `cur`, grouped into
/// maximal horizontal runs so each contiguous change costs a single `MoveTo`.
/// No `Clear` is emitted: untouched cells keep whatever is already on screen,
/// which is the whole point - an unchanged frame produces zero bytes and a
/// streaming pane no longer triggers a full-screen clear-and-repaint. Caller
/// guarantees matching dimensions (a size change takes the full-paint path).
fn emit_diff<W: Write>(prev: &ScreenBuffer, cur: &ScreenBuffer, out: &mut W) -> io::Result<()> {
    for r in 0..cur.rows {
        let mut c = 0;
        while c < cur.cols {
            if cur.get(r, c) == prev.get(r, c) {
                c += 1;
                continue;
            }
            // Start of a changed run: anchor the cursor once, then walk the
            // run emitting cells (SGR diffed within the run, reset at its end
            // so style never bleeds across the MoveTo gap to the next run).
            queue!(out, cursor::MoveTo(c, r))?;
            let mut last: Option<CellStyle> = None;
            while c < cur.cols && cur.get(r, c) != prev.get(r, c) {
                if let Some(cell) = cur.get(r, c) {
                    emit_cell(out, cell, &mut last)?;
                }
                c += 1;
            }
            queue!(out, style::SetAttribute(style::Attribute::Reset))?;
        }
    }
    Ok(())
}

/// Rasterize one grid frame (borders + titles + pane interiors + footer) into
/// a full-terminal [`ScreenBuffer`]. The terminal size is recovered from the
/// footer rect, which the layout always anchors to the bottom row spanning the
/// full width.
// `vis_snapshots` is `&mut` so `raster_pane_interior` can MOVE cells out of it
// (std::mem::take) instead of cloning every cell's heap String each tick. The
// snapshots are freshly built per paint and dropped right after, so emptying
// them is free (gemini-code-assist, PR #386).
#[allow(clippy::too_many_arguments)]
fn build_frame(
    paged: &PageLayout,
    names: &[String],
    vis_snapshots: &mut [PaneSnapshot],
    states: &[ConnState],
    comp: &Compositor,
    host_interactive: &[bool],
    hint: Option<&str>,
    badges: &[(usize, usize)],
    cap_note: Option<(usize, usize)>,
    palette: &Palette,
) -> ScreenBuffer {
    let rows = paged.footer.row + paged.footer.rows;
    let cols = paged.footer.col + paged.footer.cols;
    let mut frame = ScreenBuffer::with_chrome(rows, cols, palette.border, palette.bg);

    // `names`/`states` are GLOBAL (one entry per agent in the eager fleet);
    // `paged.tiles` and `vis_snapshots` cover only the current page's slots.
    // The global index of slot `i` is `page_start + i`, so focus (a global
    // index) compares against it directly.
    for (slot, tile) in paged.tiles.iter().enumerate() {
        let gidx = paged.page_start + slot;
        let focused = gidx == comp.focus();
        let name = names.get(gidx).map(String::as_str).unwrap_or("?");
        // In scrollback the focused pane's title badges its scroll position, so
        // the operator always sees the view is frozen (AC1-UI: no silent state
        // change). Other panes keep their normal `name · state` title.
        let label = if focused && comp.mode() == Mode::Scrollback {
            let off = vis_snapshots
                .get(slot)
                .map(|s| s.scroll_offset)
                .unwrap_or(0);
            format!("SCROLLBACK -{off}")
        } else {
            states
                .get(gidx)
                .map(ConnState::label)
                .unwrap_or_else(|| "?".to_string())
        };
        raster_border(&mut frame, tile, focused);
        raster_title(&mut frame, tile, name, &label, focused);
        // Cells are MOVED out (mem::take in raster_pane_interior), so take the
        // snapshot mutably here.
        if let Some(snap) = vis_snapshots.get_mut(slot) {
            raster_pane_interior(&mut frame, tile, snap);
        }
    }

    // The focused pane's scroll offset feeds the footer's position indicator
    // (0 unless the operator is scrolling). focus is always on the current page.
    let focus_offset = vis_snapshots
        .get(comp.focus().saturating_sub(paged.page_start))
        .map(|s| s.scroll_offset)
        .unwrap_or(0);
    raster_footer(
        &mut frame,
        paged,
        comp,
        host_interactive,
        hint,
        badges,
        cap_note,
        focus_offset,
    );
    frame
}

/// The "terminal too small" frame (AC4-ERR): the layout error at the top-left
/// of an otherwise blank buffer, sized to the current tty so a diff against a
/// same-size prior frame still works.
fn build_too_small_frame(tty: TtySize, err: &LayoutError) -> ScreenBuffer {
    let mut frame = ScreenBuffer::blank(tty.rows, tty.cols);
    let msg = truncate(&err.to_string(), tty.cols.max(1) as usize);
    frame.put_str(0, 0, &msg, false);
    frame
}

/// Build the E5b zero-config front-door frame, shown whenever the grid has no
/// panes. Centers a title + prompt and renders the live goal buffer the
/// operator is typing. The input line is anchored at a fixed column so the
/// caret area does not jump as characters are typed. Pure over its inputs.
fn build_front_door_frame(tty: TtySize, goal: &str, hint: Option<&str>) -> ScreenBuffer {
    let mut frame = ScreenBuffer::blank(tty.rows, tty.cols);
    let cols = tty.cols.max(1) as usize;
    let title = truncate("footnote grid", cols);
    let prompt = truncate(
        "Enter a goal -> /target run; prefix > -> open a claude pane to type into. Esc quits.",
        cols,
    );
    // Center each line on its own width; the input line is anchored under the
    // prompt's start column so it stays put while typing.
    let col_of = |s: &str| -> u16 { ((cols - s.chars().count().min(cols)) / 2) as u16 };
    let mid = tty.rows / 2;
    let pcol = col_of(&prompt);
    frame.put_str(mid.saturating_sub(2), col_of(&title), &title, false);
    frame.put_str(mid, pcol, &prompt, false);
    let line = truncate(
        &format!("goal> {goal}\u{2588}"),
        cols.saturating_sub(pcol as usize).max(1),
    );
    frame.put_str(mid.saturating_add(2), pcol, &line, true);
    // Status line (e.g. "launch failed: ..."): without it a failed submit on
    // the empty front door would clear back to a blank prompt with no feedback.
    if let Some(h) = hint {
        let h = truncate(h, cols);
        frame.put_str(mid.saturating_add(4), col_of(&h), &h, false);
    }
    frame
}

/// Paint the front-door frame through the same diff/emit machinery as [`paint`]
/// (so it shares the no-re-clear steady state). Called from the run loop's
/// paint sites whenever `panes` is empty.
fn paint_front_door<W: Write>(
    out: &mut W,
    prev_frame: &mut Option<ScreenBuffer>,
    tty: TtySize,
    goal: &str,
    hint: Option<&str>,
) {
    let cur = build_front_door_frame(tty, goal, hint);
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    match prev_frame.as_ref() {
        Some(prev) if prev.rows == cur.rows && prev.cols == cur.cols => {
            let _ = emit_diff(prev, &cur, &mut buf);
        }
        _ => {
            let _ = queue!(buf, terminal::Clear(terminal::ClearType::All));
            let _ = emit_full(&cur, &mut buf);
        }
    }
    let _ = out.write_all(&buf);
    let _ = out.flush();
    *prev_frame = Some(cur);
}

/// Render one frame to `out`, choosing the cheapest correct paint path:
///
/// - **First frame / resize** (`prev_frame` is `None` or its dimensions no
///   longer match): a one-time `Clear(All)` + full paint. This is the ONLY
///   place a clear is emitted - the per-dirty-tick `Clear` + full repaint was
///   the flicker source, and the steady-state path below never clears.
/// - **Steady state** (same-size prior frame): diff against it and emit only
///   the changed cells.
///
/// Either way the frame is built into an in-memory buffer and written to `out`
/// with a single `write_all` + flush, keeping each repaint one atomic syscall.
/// `prev_frame` is updated to the frame just painted.
#[allow(clippy::too_many_arguments)]
fn paint<W: Write>(
    out: &mut W,
    prev_frame: &mut Option<ScreenBuffer>,
    tty: TtySize,
    names: &[String],
    panes: &[Pane],
    states: &[ConnState],
    comp: &Compositor,
    host_interactive: &[bool],
    hint: Option<&str>,
    cap_note: Option<(usize, usize)>,
    // Rail mode: when Some, renders the rail-grouped layout instead of the
    // paginated tiled grid. `rows` is the same registry-row slice group_by
    // consumed (ab-1fab1fdf, Phase 1).
    rail_state: RailPaintArg<'_>,
    // When true, draw the `?` help overlay on top of the frame (E5c AC-3).
    help_open: bool,
    // OSC 10/11 terminal palette for chrome coloring (E5c AC-4).
    // `Palette::fixed()` yields the pre-palette Default behavior.
    palette: &Palette,
) {
    // Domain Pitfall: a rail toggle / g re-partition changes the region map,
    // which invalidates the diff painter's assumption of a stable region. There
    // is no `force_full_paint` flag - the caller forces a full repaint by
    // clearing `prev_frame` (setting it to None) on any region-map change. This
    // function then takes the Clear + emit_full path below whenever `prev_frame`
    // is None or its dimensions differ from the current frame; otherwise it
    // diffs. So the caller's `prev_frame = None` is the full-paint trigger.

    let mut cur = 'build: {
        if let Some((rs, groups, badges, rows)) = rail_state {
            // Rail mode + GroupTile (US3): tile the selected group's current
            // page in the main area. Falls through to the Single render below
            // when no group resolves (empty fleet / selection absent) so
            // `layout::compute` is never called with a 0-member group
            // (AC1-EDGE / AC3-EDGE).
            if matches!(rs.main_mode, group::MainMode::GroupTile) {
                if let Some(sel_group) = rs.selected_group(groups) {
                    // AC3-FR: tile only the LIVE members so a member that exits
                    // mid-session drops its tile and the survivors reflow to fill
                    // the freed space. The full `sel_group` is kept for the rail
                    // list + header `(count)`/`xN` badge (the exited agent stays
                    // visible in the rail) and for the footer group name, but the
                    // main-area tiling, page count, and selected-page derivation
                    // all run over the survivors.
                    let live_group = live_group_of(sel_group, states);
                    let group_size = live_group.members.len();
                    if group_size > 0 {
                        // The rendered page is the one holding the selected
                        // member, so the accented tile is always on screen
                        // (selection drives the page - no off-page drive target).
                        // A selection that just exited is absent from the live
                        // members, so the page derivation defaults to 0 and no
                        // tile is accented until the next nav re-anchors it.
                        let page = rs.selected_group_page(&live_group, main_capacity(tty));
                        if let Ok(rail_page) =
                            layout::compute_with_rail_page(tty, layout::RAIL_COLS, group_size, page)
                        {
                            // Build the page's global pane indices and their
                            // snapshots in lockstep so slot/snapshot stay aligned
                            // even if a pane lookup ever misses (membership is 1:1
                            // with panes, so in practice every lookup hits).
                            let mut page_members: Vec<usize> = Vec::new();
                            let mut vis_snapshots: Vec<PaneSnapshot> = Vec::new();
                            for slot in 0..rail_page.main.tiles.len() {
                                let member_pos = rail_page.main.page_start + slot;
                                if let Some(&gidx) = live_group.members.get(member_pos) {
                                    // Push in lockstep so page_members[slot] stays
                                    // aligned with tiles[slot]. A missing pane (should
                                    // not happen - membership is 1:1 with panes) gets
                                    // an empty snapshot rather than being skipped, which
                                    // would shift every later tile left and leave the
                                    // last tiles unrendered (gemini HIGH, PR #399).
                                    page_members.push(gidx);
                                    vis_snapshots.push(
                                        panes
                                            .get(gidx)
                                            .map(Pane::snapshot)
                                            .unwrap_or_else(empty_pane_snapshot),
                                    );
                                }
                            }
                            break 'build build_frame_rail_group(
                                &rail_page,
                                names,
                                &mut vis_snapshots,
                                states,
                                groups,
                                badges,
                                rs,
                                rows,
                                &live_group,
                                &page_members,
                                hint,
                                palette,
                            );
                        }
                    }
                }
                // GroupTile with no resolvable / fitting / all-exited group:
                // fall through to the Single render (which now reports the active
                // main_mode in its footer, so the fall-through reads `· tile`).
            }
            // Rail mode: compute the rail layout and build the rail frame.
            if let Ok(rail_layout) = layout::compute_with_rail(tty, layout::RAIL_COLS, 1) {
                // Snapshot only the focused pane for the single-pane main area.
                let mut vis_snapshots: Vec<PaneSnapshot> = if let Some(fidx) = rs.selected_agent_idx
                {
                    if fidx < panes.len() {
                        vec![panes[fidx].snapshot()]
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                };
                break 'build build_frame_rail(
                    &rail_layout,
                    names,
                    &mut vis_snapshots,
                    states,
                    groups,
                    badges,
                    rs,
                    rows,
                    hint,
                    palette,
                );
            }
            // Terminal too narrow for the rail + a min pane, but a railless grid
            // may still fit (e.g. ~30 cols): degrade to the tiled grid rather than
            // blanking to a "too small" error (design lines 70/213, sigma-review
            // finding). rail_state is left intact - this is a transient render
            // fallback, so the rail returns automatically when the terminal widens.
        }
        // Railless mode (or rail degraded by width): paginated tiled grid.
        match layout::compute_page(tty, panes.len(), comp.current_page()) {
            Ok(paged) => {
                // Attention scan over ALL panes (eager); only the visible slice is
                // snapshotted for rendering (bounds per-tick alloc to one page).
                let waiting: Vec<bool> = panes
                    .iter()
                    .zip(states.iter())
                    .map(|(p, s)| p.is_waiting(s))
                    .collect();
                let badges_page =
                    off_screen_waiting_by_page(&waiting, paged.capacity, paged.current_page);
                let mut vis_snapshots: Vec<PaneSnapshot> = paged
                    .tiles
                    .iter()
                    .enumerate()
                    .map(|(slot, _)| {
                        let gidx = paged.page_start + slot;
                        panes[gidx].snapshot()
                    })
                    .collect();
                build_frame(
                    &paged,
                    names,
                    &mut vis_snapshots,
                    states,
                    comp,
                    host_interactive,
                    hint,
                    &badges_page,
                    cap_note,
                    palette,
                )
            }
            Err(e) => build_too_small_frame(tty, &e),
        }
    };

    // Overlay the `?` help box AFTER the base frame is built but BEFORE the
    // diff / emit step. The diff path handles overlay appear/disappear naturally:
    // cells that change are re-emitted; the rest are not. (E5c AC-3)
    if help_open {
        let rail_on = rail_state.is_some();
        let lines = help_overlay_lines(rail_on);
        raster_help_overlay(&mut cur, &lines);
    }

    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    match prev_frame.as_ref() {
        Some(prev) if prev.rows == cur.rows && prev.cols == cur.cols => {
            let _ = emit_diff(prev, &cur, &mut buf);
        }
        _ => {
            let _ = queue!(buf, terminal::Clear(terminal::ClearType::All));
            let _ = emit_full(&cur, &mut buf);
        }
    }
    let _ = out.write_all(&buf);
    let _ = out.flush();
    *prev_frame = Some(cur);
}

/// Build a complete frame and paint it whole (with a leading `Clear`). Pure
/// over its inputs - unit-tested against a `Vec<u8>`. Production paints go
/// through [`paint`] (which diffs via [`build_frame`] + [`emit_full`] /
/// [`emit_diff`]); this from-scratch helper exists only to exercise the
/// rasterize-then-full-paint path directly, so it is test-only.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn render_to<W: Write>(
    out: &mut W,
    paged: &PageLayout,
    names: &[String],
    vis_snapshots: &[PaneSnapshot],
    states: &[ConnState],
    comp: &Compositor,
    host_interactive: &[bool],
    hint: Option<&str>,
    badges: &[(usize, usize)],
    cap_note: Option<(usize, usize)>,
) -> io::Result<()> {
    // build_frame now consumes its snapshots (moves cells out). This test-only
    // helper takes a borrowed slice, so clone into an owned vec it can hand
    // over mutably - the perf path that matters is the production `paint`.
    let mut owned = vis_snapshots.to_vec();
    let frame = build_frame(
        paged,
        names,
        &mut owned,
        states,
        comp,
        host_interactive,
        hint,
        badges,
        cap_note,
        &Palette::fixed(), // ponytail: test helper always uses fixed palette
    );
    queue!(out, terminal::Clear(terminal::ClearType::All))?;
    emit_full(&frame, out)?;
    out.flush()
}

/// Box-drawing border around a tile; the focused tile uses a heavy border.
fn raster_border(frame: &mut ScreenBuffer, tile: &layout::TileRect, focused: bool) {
    let (tl, tr, bl, br, h, v) = if focused {
        ('┏', '┓', '┗', '┛', '━', '┃')
    } else {
        ('┌', '┐', '└', '┘', '─', '│')
    };
    let top: String = std::iter::once(tl)
        .chain(std::iter::repeat_n(h, tile.cols.saturating_sub(2) as usize))
        .chain(std::iter::once(tr))
        .collect();
    let bottom: String = std::iter::once(bl)
        .chain(std::iter::repeat_n(h, tile.cols.saturating_sub(2) as usize))
        .chain(std::iter::once(br))
        .collect();
    frame.put_str(tile.row, tile.col, &top, false);
    frame.put_str(
        tile.row + tile.rows.saturating_sub(1),
        tile.col,
        &bottom,
        false,
    );
    let v = v.to_string();
    for r in 1..tile.rows.saturating_sub(1) {
        frame.put_str(tile.row + r, tile.col, &v, false);
        frame.put_str(
            tile.row + r,
            tile.col + tile.cols.saturating_sub(1),
            &v,
            false,
        );
    }
}

/// Title bar inside the top border: `name · state`, truncated to fit. Focused
/// tiles render the title inverse-video.
fn raster_title(
    frame: &mut ScreenBuffer,
    tile: &layout::TileRect,
    name: &str,
    label: &str,
    focused: bool,
) {
    let inner = tile.cols.saturating_sub(4) as usize;
    let title = truncate(&format!("{name} · {label}"), inner);
    frame.put_str(tile.row, tile.col + 2, &format!(" {title} "), focused);
}

/// Move the pane's snapshot cells into the tile interior (inside the border),
/// clipped to the interior dimensions. The snapshot is expected to be sized to
/// the interior already (the run loop sizes Panes that way).
///
/// Cells are MOVED out with `std::mem::take` rather than cloned: the snapshot
/// is consumed by this paint and dropped after, so emptying it avoids a
/// heap-String clone per cell per tick (gemini-code-assist, PR #386).
fn raster_pane_interior(
    frame: &mut ScreenBuffer,
    tile: &layout::TileRect,
    snap: &mut PaneSnapshot,
) {
    let inner_rows = tile.rows.saturating_sub(2);
    let inner_cols = tile.cols.saturating_sub(2);
    let cols = snap.cols as usize;
    for r in 0..inner_rows.min(snap.rows) {
        for c in 0..inner_cols.min(snap.cols) {
            let idx = (r as usize) * cols + (c as usize);
            if let Some(cell) = snap.cells.get_mut(idx) {
                frame.set(tile.row + 1 + r, tile.col + 1 + c, std::mem::take(cell));
            }
        }
    }
}

/// Emit crossterm style commands for a cell only when its style differs
/// from the previously-emitted one (SGR diffing, mirroring relay's snapshot
/// renderer). Tracks `(fg, bg, bold, italic, underline, reverse)`.
fn apply_cell_style<W: Write>(
    out: &mut W,
    cell: &RenderCell,
    last: &mut Option<CellStyle>,
) -> io::Result<()> {
    let want = (
        cell.fg,
        cell.bg,
        cell.bold,
        cell.italic,
        cell.underline,
        cell.inverse,
    );
    if *last == Some(want) {
        return Ok(());
    }
    queue!(out, style::SetAttribute(style::Attribute::Reset))?;
    queue!(out, style::SetForegroundColor(to_ct_color(cell.fg)))?;
    queue!(out, style::SetBackgroundColor(to_ct_color_bg(cell.bg)))?;
    if cell.bold {
        queue!(out, style::SetAttribute(style::Attribute::Bold))?;
    }
    if cell.italic {
        queue!(out, style::SetAttribute(style::Attribute::Italic))?;
    }
    if cell.underline {
        queue!(out, style::SetAttribute(style::Attribute::Underlined))?;
    }
    if cell.inverse {
        queue!(out, style::SetAttribute(style::Attribute::Reverse))?;
    }
    *last = Some(want);
    Ok(())
}

fn to_ct_color(c: CellColor) -> style::Color {
    match c {
        CellColor::Default => style::Color::Reset,
        CellColor::Indexed(i) => style::Color::AnsiValue(i),
        CellColor::Rgb(r, g, b) => style::Color::Rgb { r, g, b },
    }
}

/// Background uses Reset for Default too; split out for symmetry / clarity.
fn to_ct_color_bg(c: CellColor) -> style::Color {
    to_ct_color(c)
}

/// Footer: a transient hint when present, otherwise mode indicator +
/// `Page n/P` + off-screen attention badges (fu-grid-pagination, task 4.1) +
/// soft-cap note. Anchored at the layout's footer row, budgeted to a single
/// line (FOOTER_ROWS stays 1) via `truncate`.
///
/// In WATCH the Enter affordance is host_mode-aware (ab-7fd7ae49): it reads
/// "Enter drive" only when the focused pane is an interactive host, and
/// "Enter: exec - watch only" when it is a one-shot exec agent. A transient
/// operator hint (e.g. the watch-only message raised when Enter hits an exec
/// pane) overrides the whole footer for one frame; it clears on the next key.
///
/// Single-page case renders NO pagination chrome (AC1-UI). Badges encode page
/// number (1-indexed) + waiting count, e.g. `▸p2●1`; the current page never
/// carries a badge (it is directly visible).
#[allow(clippy::too_many_arguments)]
fn raster_footer(
    frame: &mut ScreenBuffer,
    paged: &PageLayout,
    comp: &Compositor,
    // Owned-PTY: the footer hint no longer branches on host_mode (no
    // "Enter drive" vs "watch only"); kept in the signature for the caller.
    _host_interactive: &[bool],
    transient_hint: Option<&str>,
    badges: &[(usize, usize)],
    cap_note: Option<(usize, usize)>,
    scroll_offset: usize,
) {
    let line = if let Some(h) = transient_hint {
        // A transient operator message overrides the footer for one frame.
        truncate(h, paged.footer.cols as usize)
    } else if comp.mode() == Mode::Scrollback {
        // Scrollback mode owns the footer: show the offset + the scroll/exit
        // affordance (the pane is frozen; Esc snaps back to the live tail).
        truncate(
            &format!("SCROLLBACK -{scroll_offset} · ↑↓ scroll · Esc live"),
            paged.footer.cols as usize,
        )
    } else {
        // Pagination chrome is the load-bearing footer content (the operator's
        // awareness of off-screen agents), so it leads the line and survives
        // truncation on a narrow terminal; the mux hint fills whatever space is
        // left.
        let mut line = String::new();
        if !paged.is_single_page() {
            // 1-indexed page display for humans.
            line.push_str(&format!(
                "Page {}/{}",
                paged.current_page + 1,
                paged.page_count
            ));
            for (page, count) in badges {
                line.push_str(&format!("  ▸p{}●{}", page + 1, count));
            }
            line.push_str("  ·  ");
        }
        // Owned-PTY model (x-1356): every pane is live - type straight into the
        // focused one; the leader unlocks mux commands. No WATCH/DRIVE split,
        // no take-over, no per-pane "exec - watch only".
        let hint = if paged.is_single_page() {
            "type into the focused pane  ·  leader then: ↹ focus · q quit"
        } else {
            "type into the focused pane  ·  leader then: ↹ focus · ] [ page · q quit"
        };
        line.push_str(hint);
        if let Some((shown, total)) = cap_note {
            line.push_str(&format!("  ·  {shown}/{total} shown"));
        }
        truncate(&line, paged.footer.cols as usize)
    };
    // Footer cells default to blank in a fresh frame, so no explicit line clear
    // is needed; the diff path erases any leftover trailing chars on its own.
    frame.put_str(paged.footer.row, paged.footer.col, &line, false);
}

// ── Rail renderer (ab-1fab1fdf, Phase 1) ─────────────────────────────────────

/// Render the left navigation rail into `frame` at `rail_rect`.
///
/// Each group header is rendered as a bold/inverse line showing the group key
/// value and member count: `group-header (N)`. If the group has an attention
/// badge, a compact badge glyph follows the count:
/// - `!` for needs-input agents (distinct from the count)
/// - `x` for exited agents
///
/// Each member (agent) is rendered below its header as a indented line.
/// The selected member is inverse-video.
///
/// Wide-char safety: all text goes through `truncate_rail` (char-boundary +
/// column-width aware), reusing the `put_str` column-cursor convention from
/// `ScreenBuffer::put_str` (Domain Pitfall: port #386/#387 wide-glyph discipline).
///
/// Full-paint only: callers must ensure this function is called on a fresh frame
/// (not the diff path) whenever the rail content changes (Domain Pitfall).
fn raster_rail(
    frame: &mut ScreenBuffer,
    rail_rect: &layout::TileRect,
    groups: &[group::Group],
    badges: &[group::GroupBadge],
    rail_state: &group::RailState,
    rows: &[serde_json::Value],
) {
    let max_cols = rail_rect.cols as usize;
    let visible = rail_rect.rows as usize;

    // Build the flat list of rail lines (group headers + indented members) in
    // render order, tracking the flat index of the selected member so the view
    // can scroll to keep it on screen for fleets taller than the rail (codex P2:
    // each group adds a header row, so the 32-agent cap can exceed the height).
    let mut lines: Vec<(String, bool)> = Vec::new();
    let mut selected_flat: Option<usize> = None;
    for (gi, grp) in groups.iter().enumerate() {
        // Header line: "header (N)[badge]"
        let badge = badges.get(gi).cloned().unwrap_or_default();
        let count = grp.members.len();
        let badge_str = if badge.needs_input > 0 && badge.exited > 0 {
            format!("!{}x{}", badge.needs_input, badge.exited)
        } else if badge.needs_input > 0 {
            format!("!{}", badge.needs_input)
        } else if badge.exited > 0 {
            format!("x{}", badge.exited)
        } else {
            String::new()
        };
        let header_line = if badge_str.is_empty() {
            format!("{} ({})", grp.header, count)
        } else {
            format!("{} ({}) {}", grp.header, count, badge_str)
        };
        lines.push((truncate_rail(&header_line, max_cols), false));

        // Member lines: indented, selected member is inverse.
        let cwd_grouped = rail_state.group_key == group::GroupKey::Cwd;
        for &member_idx in &grp.members {
            let row = rows.get(member_idx);
            let agent_name = || {
                row.and_then(|r| r.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
            };
            // Under repo-rollup (Cwd grouping), label the member by its worktree
            // (`main`, `e5c-layout`) so a repo's checkouts are distinguishable
            // (US2); fall back to the agent name (other group keys, or a non-git
            // agent that carries no `_worktree`).
            let worktree = if cwd_grouped {
                row.and_then(|r| r.get(repo::WORKTREE_FIELD))
                    .and_then(serde_json::Value::as_str)
            } else {
                None
            };
            let label: String = match worktree {
                // Two agents can share a checkout (e.g. several goals launched in
                // the same cwd), so the same `_worktree` repeats within a group.
                // A non-unique label would render duplicate bullets and an
                // ambiguous selection marker, so disambiguate with the agent name
                // (codex P2). Names are registry-unique.
                Some(w)
                    if grp
                        .members
                        .iter()
                        .filter(|&&mi| {
                            rows.get(mi)
                                .and_then(|r| r.get(repo::WORKTREE_FIELD))
                                .and_then(serde_json::Value::as_str)
                                == Some(w)
                        })
                        .count()
                        > 1 =>
                {
                    format!("{w} ({})", agent_name())
                }
                Some(w) => w.to_string(),
                None => agent_name().to_string(),
            };
            // Occurrence-aware: accent only the copy in the selected group, not
            // every group a shared (multi-squad) agent appears in (x-8a6a).
            let selected = rail_state.is_selected_occurrence(grp, member_idx);
            if selected {
                selected_flat = Some(lines.len());
            }
            // Indent by 2 spaces; leave room for the selection marker.
            let prefix = if selected { "> " } else { "  " };
            let available = max_cols.saturating_sub(prefix.len());
            let name_part = truncate_rail(&label, available);
            let padded = pad_rail(&format!("{prefix}{name_part}"), max_cols);
            lines.push((padded, selected));
        }
    }

    // Scroll so the selected line stays visible: when it would fall past the
    // bottom, shift the window so it lands on the last visible row. Stateless -
    // recomputed each frame from the selected flat index.
    let scroll = match selected_flat {
        Some(f) if visible > 0 && f >= visible => f + 1 - visible,
        _ => 0,
    };

    let mut row = rail_rect.row;
    let last_row = rail_rect.row + rail_rect.rows;
    for (text, inverse) in lines.iter().skip(scroll) {
        if row >= last_row {
            break;
        }
        frame.put_str(row, rail_rect.col, text, *inverse);
        row += 1;
    }
    // Fill remaining rows with blanks so stale content is overwritten.
    let blank = " ".repeat(max_cols);
    while row < last_row {
        frame.put_str(row, rail_rect.col, &blank, false);
        row += 1;
    }
}

/// Truncate a string to fit within `max_cols` columns, using the same
/// char-boundary-aware logic as `truncate`. Wide characters are treated as
/// single-column for now (full Unicode column-width tracking is Phase 2;
/// this matches the existing `put_str` single-cell-per-char model).
fn truncate_rail(s: &str, max_cols: usize) -> String {
    truncate(s, max_cols)
}

/// Pad (or truncate) a string to exactly `width` columns for the rail so
/// the inverse-video selection highlight fills the full cell width.
fn pad_rail(s: &str, width: usize) -> String {
    let count = s.chars().count();
    if count >= width {
        truncate(s, width)
    } else {
        format!("{s}{}", " ".repeat(width - count))
    }
}

/// Rail-mode footer: always shows `WATCH|DRIVE | group-by: <key>`.
/// The focus axis indicator distinguishes the two states (Locked Decision 6).
fn raster_footer_rail(
    frame: &mut ScreenBuffer,
    footer: &layout::TileRect,
    rail_state: &group::RailState,
    attn: Option<&str>,
    hint: Option<&str>,
) {
    let (axis_label, keymap) = if rail_state.nav_mode {
        (
            "NAV",
            "↑↓ select · Enter/d drive · Tab tile · g regroup · a attn · t hide · Esc drive",
        )
    } else {
        ("DRIVE", "type to drive the pane · leader for rail commands")
    };
    let key_label = rail_state.group_key.label();
    // The mode token names the active main-area mode (US3 footer invariant). It
    // tracks `main_mode`, not a hardcoded `single`, so a GroupTile render that
    // falls through to this Single raster (empty / all-exited / too-narrow group)
    // reads `· tile` rather than a stale `· single` (ab-2b55fc77).
    let mode_label = main_mode_label(rail_state.main_mode);
    let filter_seg = attention_filter_label(rail_state.attention_filter);
    // A transient hint (exec/exited/drive-denied feedback) replaces the keymap
    // tail so the operator sees why a key did nothing; mode + group-by + filter +
    // the global attention summary stay visible (AC4-UI; codex P2: rail footer
    // dropped the hint entirely).
    let tail = hint.unwrap_or(keymap);
    let attn_seg = attn.map(|a| format!("{a}  ·  ")).unwrap_or_default();
    let line = format!(
        "{axis_label} · {mode_label} | group-by: {key_label}{filter_seg}  ·  {attn_seg}{tail}"
    );
    let trimmed = truncate(&line, footer.cols as usize);
    frame.put_str(footer.row, footer.col, &trimmed, false);
}

/// The footer mode token for a `MainMode` (`single` / `tile`). Shared by both
/// rail footers so the active main-area mode is named consistently.
fn main_mode_label(mode: group::MainMode) -> &'static str {
    match mode {
        group::MainMode::Single => "single",
        group::MainMode::GroupTile => "tile",
    }
}

/// The footer filter token: ` · filter: attention` when the attention filter is
/// on, empty otherwise. Appended after the `group-by` key in both rail footers
/// so the operator always sees when the rail is showing a waiting-only subset
/// (never wonders why agents seem to be missing).
fn attention_filter_label(filter_on: bool) -> &'static str {
    if filter_on {
        " · filter: attention"
    } else {
        ""
    }
}

/// Format the global attention summary token from per-group badge totals:
/// `!N` (agents waiting for input) and `xM` (agents exited), space-joined,
/// omitting a zero component. Returns `None` when nothing needs attention so the
/// footer stays quiet. This is the fleet-wide roll-up (Open Q1) that complements
/// the per-header badges already shown beside each group in the rail.
fn attention_summary(badges: &[group::GroupBadge]) -> Option<String> {
    let needs: usize = badges.iter().map(|b| b.needs_input).sum();
    let exited: usize = badges.iter().map(|b| b.exited).sum();
    match (needs, exited) {
        (0, 0) => None,
        (n, 0) => Some(format!("!{n}")),
        (0, x) => Some(format!("x{x}")),
        (n, x) => Some(format!("!{n} x{x}")),
    }
}

/// Build a complete rail-mode frame. Analogous to `build_frame` but uses
/// `compute_with_rail` geometry and includes the rail column on the left.
///
/// The main area renders the single focused agent at full width (Single mode).
/// GroupTile mode (US3) is handled by [`build_frame_rail_group`], dispatched on
/// `rail_state.main_mode` in [`paint`] before this is reached.
#[allow(clippy::too_many_arguments)]
fn build_frame_rail(
    rail_layout: &layout::RailLayout,
    names: &[String],
    vis_snapshots: &mut [PaneSnapshot],
    states: &[ConnState],
    groups: &[group::Group],
    badges: &[group::GroupBadge],
    rail_state: &group::RailState,
    rows: &[serde_json::Value],
    hint: Option<&str>,
    palette: &Palette,
) -> ScreenBuffer {
    let footer = &rail_layout.footer;
    let total_rows = footer.row + footer.rows;
    let total_cols = footer.col + footer.cols;
    let mut frame = ScreenBuffer::with_chrome(total_rows, total_cols, palette.border, palette.bg);

    // 1. Rail.
    raster_rail(
        &mut frame,
        &rail_layout.rail,
        groups,
        badges,
        rail_state,
        rows,
    );

    // 2. Main area: the focused agent's pane at full main-area width (Single mode).
    if let Some(focused_idx) = rail_state.selected_agent_idx {
        if rail_layout.main.tiles.len() == 1 {
            let tile = &rail_layout.main.tiles[0];
            let focused = true; // Single mode: the one tile is always focused.
            let name = names.get(focused_idx).map(String::as_str).unwrap_or("?");
            let label = states
                .get(focused_idx)
                .map(ConnState::label)
                .unwrap_or_else(|| "?".to_string());
            // Owned-PTY: the focused pane is always the drive target, so the
            // accent keys off plain focus (the PaneDrive axis is gone, x-1356).
            raster_border(&mut frame, tile, focused);
            raster_title(&mut frame, tile, name, &label, focused);
            if let Some(snap) = vis_snapshots.first_mut() {
                raster_pane_interior(&mut frame, tile, snap);
            }
        }
    }

    // 3. Rail footer (always shows axis + group-by key + global attention
    //    summary; surfaces any transient hint).
    let attn = attention_summary(badges);
    raster_footer_rail(&mut frame, footer, rail_state, attn.as_deref(), hint);

    frame
}

/// Build a GroupTile-mode frame (US3): the rail plus the selected group's
/// members tiled side-by-side in the main area. `page_members[slot]` is the
/// global pane index rendered in `rail_page.main.tiles[slot]`, and
/// `vis_snapshots[slot]` is that pane's snapshot. The member holding the rail
/// selection is accented so the operator can tell which pane `Enter`/`d` drives.
#[allow(clippy::too_many_arguments)]
fn build_frame_rail_group(
    rail_page: &layout::RailPageLayout,
    names: &[String],
    vis_snapshots: &mut [PaneSnapshot],
    states: &[ConnState],
    groups: &[group::Group],
    badges: &[group::GroupBadge],
    rail_state: &group::RailState,
    rows: &[serde_json::Value],
    sel_group: &group::Group,
    page_members: &[usize],
    hint: Option<&str>,
    palette: &Palette,
) -> ScreenBuffer {
    let footer = &rail_page.footer;
    let total_rows = footer.row + footer.rows;
    let total_cols = footer.col + footer.cols;
    let mut frame = ScreenBuffer::with_chrome(total_rows, total_cols, palette.border, palette.bg);

    // 1. Rail (unchanged from Single mode).
    raster_rail(
        &mut frame,
        &rail_page.rail,
        groups,
        badges,
        rail_state,
        rows,
    );

    // 2. Main area: tile each member of the selected group's current page.
    for (slot, tile) in rail_page.main.tiles.iter().enumerate() {
        let Some(&gidx) = page_members.get(slot) else {
            break;
        };
        let selected = rail_state.selected_agent_idx == Some(gidx);
        let name = names.get(gidx).map(String::as_str).unwrap_or("?");
        let label = states
            .get(gidx)
            .map(ConnState::label)
            .unwrap_or_else(|| "?".to_string());
        raster_border(&mut frame, tile, selected);
        raster_title(&mut frame, tile, name, &label, selected);
        if let Some(snap) = vis_snapshots.get_mut(slot) {
            raster_pane_interior(&mut frame, tile, snap);
        }
    }

    // 3. Footer: axis + group-by key (always), the global attention summary,
    //    plus the tiled group's name and (when paginated) the page position
    //    (AC3-ERR).
    let attn = attention_summary(badges);
    raster_footer_rail_group(
        &mut frame,
        footer,
        rail_state,
        &sel_group.header,
        rail_page.main.current_page,
        rail_page.main.page_count,
        attn.as_deref(),
        hint,
    );

    frame
}

/// GroupTile footer: `WATCH · tile | group-by: <key> · group <name> [page p/k] · <tail>`.
/// Always carries the focus axis + group-by key (the rail-footer invariant); the
/// `page p/k` chrome appears only when the group spans more than one page.
#[allow(clippy::too_many_arguments)]
fn raster_footer_rail_group(
    frame: &mut ScreenBuffer,
    footer: &layout::TileRect,
    rail_state: &group::RailState,
    group_header: &str,
    current_page: usize,
    page_count: usize,
    attn: Option<&str>,
    hint: Option<&str>,
) {
    let (axis_label, keymap) = if rail_state.nav_mode {
        (
            "NAV",
            // `Enter` drills into the selected tile (-> Single); `d` drives it
            // directly; `Tab` also toggles back to Single (Open Q2 / drill-down).
            "↑↓ select · Enter focus · Tab single · d drive · ]/[ page · g regroup · a attn · t hide · Esc drive",
        )
    } else {
        ("DRIVE", "type to drive the pane · leader for rail commands")
    };
    let key_label = rail_state.group_key.label();
    let filter_seg = attention_filter_label(rail_state.attention_filter);
    let group_label = if page_count > 1 {
        format!(
            "group {group_header} page {}/{}",
            current_page + 1,
            page_count
        )
    } else {
        format!("group {group_header}")
    };
    let tail = hint.unwrap_or(keymap);
    let attn_seg = attn.map(|a| format!("{a}  ·  ")).unwrap_or_default();
    let line = format!(
        "{axis_label} · tile | group-by: {key_label}{filter_seg}  ·  {attn_seg}{group_label}  ·  {tail}"
    );
    let trimmed = truncate(&line, footer.cols as usize);
    frame.put_str(footer.row, footer.col, &trimmed, false);
}

/// Truncate a display string to `max` columns with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let keep: String = s.chars().take(max - 1).collect();
    format!("{keep}…")
}

// ── Connection helpers ──────────────────────────────────────────────────

/// Open a drive WebSocket to `name` in the given `mode` ("watch" or
/// "interactive"). Mirrors `drive_client::drive`'s connect → RPC → upgrade
/// sequence. Returns the upgraded WS stream.
async fn open_drive_ws(home: &AgentsHome, name: &str, mode: &str) -> Result<Ws, String> {
    let mut conn = UnixStream::connect(home.supervisor_sock())
        .await
        .map_err(|e| format!("cannot reach daemon: {e}"))?;
    let req = Request::new(1, "agent.drive", json!({"name": name, "mode": mode}));
    write_request(&mut conn, &req)
        .await
        .map_err(|e| format!("drive request failed: {e}"))?;
    let ack = read_response(&mut conn)
        .await
        .map_err(|e| format!("no drive ack: {e}"))?;
    match ack.payload {
        ResponsePayload::Err(err) => return Err(err.message),
        ResponsePayload::Ok(_) => {}
    }
    let (ws, _resp) = tokio_tungstenite::client_async("ws://localhost/drive", conn)
        .await
        .map_err(|e| format!("drive upgrade failed: {e}"))?;
    Ok(ws)
}

async fn send_control(sink: &mut WsSink, payload: Value) -> Result<(), ()> {
    sink.send(Message::Text(payload.to_string().into()))
        .await
        .map_err(|_| ())
}

async fn send_resize(sink: &mut WsSink, rows: u16, cols: u16) -> Result<(), ()> {
    send_control(sink, json!({"t": "resize", "rows": rows, "cols": cols})).await
}

async fn send_ping(sink: &mut WsSink) -> Result<(), ()> {
    send_control(sink, json!({"t": "ping"})).await
}

async fn close_driver_sink(mut sink: WsSink, reason: &str) {
    let _ = send_control(&mut sink, json!({"t": "detach", "reason": reason})).await;
    let _ = sink.close().await;
}

/// Target inner `(rows, cols)` for pane `gidx` under the current page layout.
/// Multi-page: the uniform C-tile size for EVERY pane (warm flips - off-screen
/// panes are pre-sized to their eventual visible size). Single-page: the
/// pane's own tile (all panes visible; `gidx == slot` since `page_start == 0`).
fn target_pane_inner(paged: &PageLayout, gidx: usize) -> (u16, u16) {
    if let Some(sz) = paged.uniform_pane_inner() {
        return sz;
    }
    // Single-page case: gidx == slot (page_start == 0) and tiles.len() ==
    // pane_count, so `get(gidx)` always hits. Fall back to the first tile
    // (never (1,1), which would silently crush an off-screen Term to a 1x1
    // grid and blind the attention scanner) for defense in depth.
    paged
        .tiles
        .get(gidx)
        .or_else(|| paged.tiles.first())
        .map(|t| (t.rows.saturating_sub(2), t.cols.saturating_sub(2)))
        .unwrap_or((1, 1))
}

async fn open_watch_pane(
    home: &AgentsHome,
    name: &str,
    idx: usize,
    rows: u16,
    cols: u16,
    tx: &mpsc::Sender<PaneMsg>,
) -> WatchOpen {
    let pane = Pane::new(rows, cols);
    // Owned-PTY model (x-1356): every pane renders over a cheap "watch"
    // connection (no drive slot - the daemon caps interactive drives at
    // DEFAULT_MAX_CONCURRENT_DRIVES). The single interactive connection that
    // carries keystrokes is opened lazily for the pane actually being driven
    // (see `ensure_drive_sink`), so only one drive slot is ever held.
    match open_drive_ws(home, name, "watch").await {
        Ok(ws) => {
            let (mut sink, mut source) = ws.split();
            if send_resize(&mut sink, rows, cols).await.is_err() {
                return (
                    pane,
                    ConnState::Disconnected {
                        reason: "initial resize failed".into(),
                    },
                    None,
                );
            }
            let tx = tx.clone();
            tokio::spawn(async move {
                while let Some(msg) = source.next().await {
                    match msg {
                        Ok(Message::Binary(b)) => {
                            // tungstenite 0.24+ yields `Bytes`; the pane channel
                            // carries an owned Vec<u8>.
                            if tx.send(PaneMsg::Bytes(idx, b.to_vec())).await.is_err() {
                                break;
                            }
                        }
                        Ok(Message::Text(t)) if t.contains("child_exited") => {
                            let _ = tx.send(PaneMsg::Exited(idx, 0)).await;
                            break;
                        }
                        Ok(Message::Close(_)) | Err(_) => {
                            let _ = tx
                                .send(PaneMsg::Closed(idx, "connection lost".into()))
                                .await;
                            break;
                        }
                        _ => {}
                    }
                }
            });
            (pane, ConnState::Connecting, Some(sink))
        }
        Err(e) => (pane, ConnState::Disconnected { reason: e }, None),
    }
}

/// Resize every (eager) pane to its target tile size and push the winsize to
/// the daemon for each open watcher / driver sink. Eager connection policy
/// (Locked Decision 3): panes are NEVER opened or closed on resize - all stay
/// open the whole session, so pane indices are stable and off-screen Terms
/// keep draining. Only dimensions change here. The target size is uniform
/// across panes in the multi-page case (every page reuses one C-tile geometry)
/// and per-tile in the single-page case (see [`target_pane_inner`]).
async fn resize_all_panes(
    paged: &PageLayout,
    panes: &mut [Pane],
    watch_sinks: &mut [Option<WsSink>],
) {
    for (idx, pane) in panes.iter_mut().enumerate() {
        let (rows, cols) = target_pane_inner(paged, idx);
        pane.resize(rows, cols);
        if let Some(sink) = watch_sinks.get_mut(idx).and_then(Option::as_mut) {
            let _ = send_resize(sink, rows, cols).await;
        }
    }
}

/// Inner `(rows, cols)` of the rail's single-pane main area for `tty`, or
/// `None` if the rail does not fit (degraded to the railless grid). The main
/// area is `compute_with_rail`'s one tile minus its 1-cell border on each edge.
fn rail_main_inner(tty: TtySize) -> Option<(u16, u16)> {
    layout::compute_with_rail(tty, layout::RAIL_COLS, 1)
        .ok()
        .and_then(|rl| {
            rl.main
                .tiles
                .first()
                .map(|t| (t.rows.saturating_sub(2), t.cols.saturating_sub(2)))
        })
}

/// Resize the rail's focused pane to the main-area size and push the winsize to
/// its open sinks, so the agent renders at the full rail-main width/height
/// rather than the smaller tiled-grid tile it was last sized to (gemini HIGH).
/// In Single mode only the focused pane is rendered, so resizing only it is
/// sufficient (and within Claude's Discretion #2: lazily size the focused pane).
/// Off-screen panes keep their last size and are re-tiled by `resize_all_panes`
/// when the rail toggles off. A no-op when the rail does not fit or the index
/// is out of range.
async fn resize_rail_focus(
    tty: TtySize,
    focused_idx: usize,
    panes: &mut [Pane],
    watch_sinks: &mut [Option<WsSink>],
) {
    let Some((rows, cols)) = rail_main_inner(tty) else {
        return;
    };
    let Some(pane) = panes.get_mut(focused_idx) else {
        return;
    };
    pane.resize(rows, cols);
    if let Some(sink) = watch_sinks.get_mut(focused_idx).and_then(Option::as_mut) {
        let _ = send_resize(sink, rows, cols).await;
    }
}

/// Resize every member of a tiled group (GroupTile, US3) to its tile size and
/// push the winsize to open sinks, so each pane renders at its tile dimensions
/// rather than the full-width Single size or a stale tiled-grid size. In the
/// multi-page case every member is sized to the uniform per-page inner size so a
/// page flip is warm (no resize churn); in the single-page case each member is
/// sized to its own tile (member position == page slot when `page_start == 0`).
/// A no-op for indices out of range.
async fn resize_rail_group(
    rail_page: &layout::RailPageLayout,
    members: &[usize],
    panes: &mut [Pane],
    watch_sinks: &mut [Option<WsSink>],
) {
    let main = &rail_page.main;
    let uniform = main.uniform_pane_inner();
    for (pos, &gidx) in members.iter().enumerate() {
        let (rows, cols) = match uniform {
            Some(sz) => sz,
            None => main
                .tiles
                .get(pos)
                .map(|t| (t.rows.saturating_sub(2), t.cols.saturating_sub(2)))
                .unwrap_or((1, 1)),
        };
        let Some(pane) = panes.get_mut(gidx) else {
            continue;
        };
        pane.resize(rows, cols);
        if let Some(sink) = watch_sinks.get_mut(gidx).and_then(Option::as_mut) {
            let _ = send_resize(sink, rows, cols).await;
        }
    }
}

/// A zero-size snapshot, used as an alignment placeholder when a GroupTile
/// member's pane lookup misses (membership is 1:1 with panes, so this is purely
/// defensive). `raster_pane_interior` renders nothing for it, leaving the tile's
/// border + title but an empty body, rather than shifting later tiles.
fn empty_pane_snapshot() -> PaneSnapshot {
    PaneSnapshot {
        rows: 0,
        cols: 0,
        cursor_row: 0,
        cursor_col: 0,
        scroll_offset: 0,
        cells: vec![],
    }
}

/// Per-page tile capacity of the rail's main area (right of the rail) for `tty`.
/// Used to derive which page of a tiled group holds the selected member. Returns
/// 1 when the main area is too small to compute a capacity (defensive; the rail
/// would have degraded to the railless grid before this matters).
fn main_capacity(tty: TtySize) -> usize {
    let main_cols = tty.cols.saturating_sub(layout::RAIL_COLS);
    layout::capacity(TtySize::new(tty.rows, main_cols)).unwrap_or(1)
}

/// A copy of `sel_group` with exited members filtered out (AC3-FR). The header /
/// key_value are preserved (so the rail + footer still name the group), but the
/// member list is the LIVE survivors derived from the per-pane `ConnState`. Both
/// the GroupTile render (`paint`) and the tile resize (`apply_group_tile_resize`)
/// build this so a member that exits mid-session drops its tile and the
/// survivors reflow consistently. `states` is indexed the same way members are.
fn live_group_of(sel_group: &group::Group, states: &[ConnState]) -> group::Group {
    let exited: Vec<bool> = states
        .iter()
        .map(|s| matches!(s, ConnState::Exited { .. }))
        .collect();
    group::Group {
        header: sel_group.header.clone(),
        key_value: sel_group.key_value.clone(),
        members: group::live_members(sel_group, &exited),
    }
}

/// The unfiltered base groups for the active view: the manual squads from the
/// store when `group_key == Squad` (x-5b3e), otherwise the derived
/// `group::group_by` partition. The single chokepoint both the nav path
/// ([`rail_view_groups`]) and the paint path (`rail_groups_and_badges`) route
/// through, so the Squad view behaves identically everywhere.
fn base_groups(
    rail_rows: &[Value],
    group_key: group::GroupKey,
    squads: &squads::SquadStore,
) -> Vec<group::Group> {
    match group_key {
        group::GroupKey::Squad => squads::squad_groups(rail_rows, squads),
        // Union (x-fef5): the derived `Cwd` sidelines AND the manual squads in
        // one rail, an agent in both reachable as two occurrences. Prefix the
        // sideline keys with `cwd:` so the two halves occupy disjoint key
        // namespaces; `squad_groups` already prefixes its keys with `squad:`.
        // That keeps the occurrence cursor's (key_value, agent) identity unique
        // even for the adversarial case of a squad named after a repo path
        // (x-8a6a invariant). Only `key_value` (cursor identity) is namespaced;
        // `header` (display) is left untouched.
        group::GroupKey::Union => {
            let mut sidelines = group::group_by(rail_rows, group::GroupKey::Cwd);
            for g in &mut sidelines {
                g.key_value = format!("cwd:{}", g.key_value);
            }
            sidelines.extend(squads::squad_groups(rail_rows, squads));
            sidelines
        }
        // Row-field partitions go through `group_by`. Enumerated (not `_`) so a
        // future non-row-field variant fails to compile here until it gets an
        // arm, rather than silently routing to `group_by` (which short-circuits
        // it to an empty, blank rail).
        group::GroupKey::Cwd
        | group::GroupKey::Session
        | group::GroupKey::Provider
        | group::GroupKey::Status => group::group_by(rail_rows, group_key),
    }
}

/// The rail's current view groups: [`base_groups`] on the active key, then the
/// attention filter (`a`) applied when active so only agents waiting for input
/// remain (empty groups dropped). Navigation, selection, and the GroupTile
/// tiling all resolve groups through this, so the `a` filter hides non-waiting
/// agents everywhere consistently. With the filter off it is exactly
/// `base_groups`.
fn rail_view_groups(
    rail_rows: &[Value],
    rs: &group::RailState,
    panes: &[Pane],
    states: &[ConnState],
    squads: &squads::SquadStore,
) -> Vec<group::Group> {
    let groups = base_groups(rail_rows, rs.group_key, squads);
    if rs.attention_filter {
        // Resolve through the 3-tier inside-leg authority so the filtered view
        // matches the badges (E3.3, codex P2): not the raw scraper `waiting`.
        let waiting: Vec<bool> = panes
            .iter()
            .zip(states.iter())
            .map(|(p, s)| p.is_waiting(s))
            .collect();
        let exited: Vec<bool> = states
            .iter()
            .map(|s| matches!(s, ConnState::Exited { .. }))
            .collect();
        let inside_leg: Vec<Option<crate::state::InsideLegReport>> = rail_rows
            .iter()
            .map(|row| {
                row.get("inside_leg")
                    .filter(|v| !v.is_null())
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
            })
            .collect();
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let attn = group::needs_input_after_authority(&waiting, &exited, &inside_leg, now_secs);
        group::attention_view(&groups, &attn)
    } else {
        groups
    }
}

/// Re-size the panes of the selected agent's group to their GroupTile tile size
/// (US3) and push the winsize to open sinks. Resolves the current group plus the
/// page holding the selection, then delegates to [`resize_rail_group`]. Called on
/// every GroupTile state change (Tab into tile, selection move, `g` regroup, page
/// jump) so the visible panes always render at tile dimensions rather than the
/// full-width Single size. A no-op when no group resolves.
#[allow(clippy::too_many_arguments)]
async fn apply_group_tile_resize(
    rs: &group::RailState,
    tty: TtySize,
    rail_rows: &[Value],
    states: &[ConnState],
    squads: &squads::SquadStore,
    panes: &mut [Pane],
    watch_sinks: &mut [Option<WsSink>],
) {
    let groups = rail_view_groups(rail_rows, rs, panes, states, squads);
    let Some(sel_group) = rs.selected_group(&groups) else {
        return;
    };
    // AC3-FR: size only the live members so the PTY winsizes match the reflowed
    // tile layout `paint` renders (an exited member drops its tile; survivors
    // grow to fill it). A wholly-exited group resolves to no tiles - return
    // rather than ask `compute_with_rail_page` for a 0-pane layout.
    let live_group = live_group_of(sel_group, states);
    let n = live_group.members.len();
    if n == 0 {
        return;
    }
    let page = rs.selected_group_page(&live_group, main_capacity(tty));
    if let Ok(rp) = layout::compute_with_rail_page(tty, layout::RAIL_COLS, n, page) {
        resize_rail_group(&rp, &live_group.members, panes, watch_sinks).await;
    }
}

async fn ping_open_sinks(watch_sinks: &mut [Option<WsSink>], states: &mut [ConnState]) {
    for (idx, sink) in watch_sinks.iter_mut().enumerate() {
        if let Some(open) = sink.as_mut() {
            if send_ping(open).await.is_err() {
                *sink = None;
                if let Some(state) = states.get_mut(idx) {
                    state.step(ConnEvent::WsClosed {
                        reason: "ping failed".into(),
                    });
                }
            }
        }
    }
}

// ── Live-add + poll-discovery (x-45e6) ────────────────────────────────────

/// Registry names present in `fresh` but not already tiled in `known` (the poll
/// diff, by name - AC2-EDGE: a name already tiled is never double-added). Pure.
fn new_registry_names(fresh: Vec<String>, known: &[String]) -> Vec<String> {
    let known: std::collections::HashSet<&str> = known.iter().map(String::as_str).collect();
    fresh
        .into_iter()
        .filter(|n| !known.contains(n.as_str()))
        .collect()
}

/// Drop candidate names whose last `worker.ping` failure is still within
/// `ttl` (x-c226). A name is eligible to (re)probe when it is unseen, or its
/// cached failure is at least `ttl` old (`>=`, so the TTL boundary re-probes -
/// AC3-EDGE). Pure: the probe runs in [`discover_children`]; the cache
/// write-back applies on the `PaneMsg::Discovered` arm (x-87f7).
fn cache_candidates(
    new: Vec<String>,
    cache: &std::collections::HashMap<String, std::time::Instant>,
    now: std::time::Instant,
    ttl: Duration,
) -> Vec<String> {
    new.into_iter()
        .filter(|name| match cache.get(name) {
            // saturating_*: Instant is monotonic, but the std docs warn a future
            // version may reintroduce duration_since's panic; saturate to be safe.
            Some(&failed_at) => now.saturating_duration_since(failed_at) >= ttl,
            None => true,
        })
        .collect()
}

/// Roster cards whose session is not already carried by a tiled card (the poll
/// diff, by session_id - AC3-EDGE). `known` is the grid's `roster_cards` vector
/// (`None` for real fno panes, skipped). Pure.
fn new_roster_cards(fresh: Vec<RosterBgCard>, known: &[Option<RosterBgCard>]) -> Vec<RosterBgCard> {
    let known: std::collections::HashSet<&str> = known
        .iter()
        .flatten()
        .map(|c| c.session_id.as_str())
        .collect();
    fresh
        .into_iter()
        .filter(|c| !known.contains(c.session_id.as_str()))
        .collect()
}

/// Read the fno registry rows (absent / unparseable => empty). Mirrors the
/// other enumeration sites; used by the poll to recover a discovered child's
/// real provider/cwd for rail grouping.
fn read_registry_rows(home: &AgentsHome) -> Vec<Value> {
    std::fs::read(home.registry_json())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| {
            v.get("agents")
                .or_else(|| v.get("entries"))
                .and_then(Value::as_array)
                .cloned()
        })
        .unwrap_or_default()
}

/// Assert every parallel vector is 1:1 with `names` (the load-bearing
/// invariant: every reader indexes these by pane index == names index). Cheap
/// `debug_assert` so a desync trips in debug/tests, never silently mislabels a
/// pane in release.
fn debug_assert_vectors_aligned(
    names: &[String],
    panes: &[Pane],
    states: &[ConnState],
    watch_sinks: &[Option<WsSink>],
    host_interactive: &[bool],
    roster_cards: &[Option<RosterBgCard>],
    rail_rows: &[Value],
) {
    debug_assert_eq!(panes.len(), names.len(), "panes 1:1 names");
    debug_assert_eq!(states.len(), names.len(), "states 1:1 names");
    debug_assert_eq!(watch_sinks.len(), names.len(), "watch_sinks 1:1 names");
    debug_assert_eq!(
        host_interactive.len(),
        names.len(),
        "host_interactive 1:1 names"
    );
    debug_assert_eq!(roster_cards.len(), names.len(), "roster_cards 1:1 names");
    debug_assert_eq!(rail_rows.len(), names.len(), "rail_rows 1:1 names");
}

/// Live-add ONE registry PTY pane at the next global index, growing every
/// parallel vector in lockstep. The launcher and the x-45e6 poll share this so
/// the parallel-vector 1:1 invariant lives in exactly one place (Change 1).
/// Opens a watcher WS, re-tiles, stamps the caller-supplied rail row. `focus`
/// steals focus to the new pane (the launcher path, where the human just
/// launched it); the poll passes `false` so a discovered pane never yanks the
/// operator's focus. Returns `true` if the pane tiled; `false` if the terminal
/// is too small to add another tile (the worker is running, just not shown yet).
#[allow(clippy::too_many_arguments)]
async fn live_add_pane(
    home: &AgentsHome,
    name: &str,
    host_mode: bool,
    tx: &mpsc::Sender<PaneMsg>,
    tty: TtySize,
    comp: &mut Compositor,
    names: &mut Vec<String>,
    panes: &mut Vec<Pane>,
    states: &mut Vec<ConnState>,
    watch_sinks: &mut Vec<Option<WsSink>>,
    host_interactive: &mut Vec<bool>,
    roster_cards: &mut Vec<Option<RosterBgCard>>,
    rail_rows: &mut Vec<Value>,
    repo_cache: &mut repo::RepoCache,
    mut rail_row: Value,
    focus: bool,
) -> bool {
    let new_idx = panes.len();
    let Ok(paged) = layout::compute_page(tty, new_idx + 1, comp.current_page()) else {
        // Worker is running; the terminal just can't tile another pane yet.
        return false;
    };
    let (rows, cols) = target_pane_inner(&paged, new_idx);
    let (pane, state, sink) = open_watch_pane(home, name, new_idx, rows, cols, tx).await;
    // Grow every parallel vector in lockstep (Invariants: parallel-vector 1:1).
    host_interactive.push(host_mode);
    roster_cards.push(None); // a real fno pane, never a bg card (x-57eb)
    panes.push(pane);
    states.push(state);
    watch_sinks.push(sink);
    names.push(name.to_string());
    // Resolve the new worker's repo so it rolls up under the right sideline on
    // the next frame (x-cb89). Caller owns the row's provenance (launcher
    // synthesizes claude+cwd; the poll passes the child's real registry row).
    repo::stamp_row(&mut rail_row, repo_cache);
    rail_rows.push(rail_row);
    comp.set_pane_count(panes.len());
    comp.recompute_pagination(paged.capacity);
    if focus {
        comp.set_focus(new_idx);
    }
    resize_all_panes(&paged, panes, watch_sinks).await;
    debug_assert_vectors_aligned(
        names,
        panes,
        states,
        watch_sinks,
        host_interactive,
        roster_cards,
        rail_rows,
    );
    true
}

/// Live-add ONE claude `--bg` roster card at the next global index (x-45e6
/// poll, Change 3). A roster card has NO fno worker socket, so it NEVER opens a
/// watcher WS (that is the rejected Option B) - the static card text is fed
/// once and the state is `BgRoster`. Mirrors the startup roster merge and keeps
/// every parallel vector 1:1 with `names`. Never steals focus. Returns `true`
/// if the card tiled; `false` if the terminal is too small.
#[allow(clippy::too_many_arguments)]
async fn live_add_roster_card(
    card: RosterBgCard,
    tty: TtySize,
    comp: &mut Compositor,
    names: &mut Vec<String>,
    panes: &mut Vec<Pane>,
    states: &mut Vec<ConnState>,
    watch_sinks: &mut Vec<Option<WsSink>>,
    host_interactive: &mut Vec<bool>,
    roster_cards: &mut Vec<Option<RosterBgCard>>,
    rail_rows: &mut Vec<Value>,
    repo_cache: &mut repo::RepoCache,
) -> bool {
    let new_idx = panes.len();
    let Ok(paged) = layout::compute_page(tty, new_idx + 1, comp.current_page()) else {
        return false; // claude's daemon still runs it; just can't tile a card yet
    };
    let (rows, cols) = target_pane_inner(&paged, new_idx);
    // NEVER open_watch_pane (Boundaries: no owned PTY for a roster card).
    panes.push(bg_card_pane(&card, rows, cols, unix_now_secs()));
    states.push(ConnState::BgRoster);
    watch_sinks.push(None);
    host_interactive.push(false); // Enter attaches; no keystrokes to an fno PTY
    names.push(card.name.clone());
    let mut rail_row = json!({
        "name": card.name.as_str(),
        "provider": "claude",
        "cwd": card.cwd.as_str(),
    });
    repo::stamp_row(&mut rail_row, repo_cache);
    rail_rows.push(rail_row);
    roster_cards.push(Some(card)); // move last; the bg card IS a card row
    comp.set_pane_count(panes.len());
    comp.recompute_pagination(paged.capacity);
    resize_all_panes(&paged, panes, watch_sinks).await;
    debug_assert_vectors_aligned(
        names,
        panes,
        states,
        watch_sinks,
        host_interactive,
        roster_cards,
        rail_rows,
    );
    true
}

/// Reapply the active rail-mode sizing after a live-add changed the pane set, so
/// the visible pane(s) render at the rail-region size, not the tiled-grid size
/// `live_add_pane` / `live_add_roster_card` just pushed (codex PR #98 P2). No-op
/// in the railless grid. Mirrors the startup rail-sizing block and the
/// `Tab`-toggle resize paths.
async fn reapply_rail_sizing(
    rail_state: Option<&group::RailState>,
    tty: TtySize,
    rail_rows: &[Value],
    states: &[ConnState],
    squad_store: &squads::SquadStore,
    focus: usize,
    panes: &mut [Pane],
    watch_sinks: &mut [Option<WsSink>],
) {
    let Some(rs) = rail_state else {
        return;
    };
    match rs.main_mode {
        group::MainMode::GroupTile => {
            apply_group_tile_resize(rs, tty, rail_rows, states, squad_store, panes, watch_sinks)
                .await
        }
        group::MainMode::Single => resize_rail_focus(tty, focus, panes, watch_sinks).await,
    }
}

/// Detached discovery task body (x-87f7, Approach B). All of the discovery
/// I/O that used to run inline in the ping arm - the registry read, the
/// `worker.ping` probe, the claude `--bg` roster load - moved off the loop
/// task so no I/O latency can starve the key-event arm. Owns its inputs so the
/// future is `'static + Send` (the explicit reversal of x-c226's `join_all`
/// borrow, which is kept INSIDE the probe here). Splits at the I/O / `&mut`
/// line (Locked Decision 1): this does only reads + the probe and resolves the
/// tiling inputs; every `&mut` pane-vector mutation stays on the loop task in
/// the `PaneMsg::Discovered` apply arm.
///
/// ALWAYS returns a `DiscoveryResult` (empty on a registry/roster error), so
/// the caller's single `tx.send` after this `.await` fires unconditionally and
/// the in-flight guard can never wedge (AC3-FR). `--all` gating happens at the
/// ping arm; this is only spawned when there is something to discover.
async fn discover_children(
    home: AgentsHome,
    names_snapshot: Vec<String>,
    cache_snapshot: std::collections::HashMap<String, std::time::Instant>,
) -> DiscoveryResult {
    // Read + parse the registry ONCE and reuse the rows for both the pane diff
    // and the roster diff (gemini PR #98). Absent / unparseable => empty (AC1-ERR /
    // read_registry_rows' empty-on-error contract), and we still return below.
    let rows = read_registry_rows(&home);

    // x-c226: skip candidates whose `worker.ping` failed within FAILED_PROBE_TTL
    // (stable stream-json lanes), so the steady state truly probes nothing and a
    // busy fleet never starves input. A single `now` for the whole run; the cache
    // write-back recomputes its own `now` on the apply arm (negligible skew).
    let now = std::time::Instant::now();
    let candidates = cache_candidates(
        new_registry_names(filter_pty_agents(&rows), &names_snapshot),
        &cache_snapshot,
        now,
        FAILED_PROBE_TTL,
    );

    let mut panes: Vec<(String, bool, Value)> = Vec::new();
    let mut probed: Vec<String> = Vec::new();
    let mut survived: Vec<String> = Vec::new();
    if !candidates.is_empty() {
        // Same authoritative PTY-protocol gate as startup, run concurrently
        // (join_all) over the rows we already hold. The borrow of the owned
        // `home`/`rows` stays inside the probe (Domain Pitfall: do NOT hoist it
        // across the spawn).
        probed = candidates.clone();
        let survivors = prune_non_pty_claude_with_rows(candidates, &rows, &home).await;
        let host_modes = host_modes_from_rows(&rows, &survivors);
        for (name, host_mode) in survivors.iter().zip(host_modes) {
            // The child's REAL registry row so rail grouping uses its true
            // provider/cwd (a `spawn --cwd` child may live in another repo). Carry
            // it in the result so the apply arm never re-reads the registry on the
            // loop (Locked Decision 2).
            let rail_row = rows
                .iter()
                .find(|r| r.get("name").and_then(Value::as_str) == Some(name.as_str()))
                .cloned()
                .unwrap_or_else(|| json!({ "name": name }));
            panes.push((name.clone(), host_mode, rail_row));
        }
        survived = survivors;
    }

    // Roster diff: roster_bg_cards_from dedups against the registry rows, so a
    // session that became an fno pane above is NOT returned (AC3-EDGE). A missing /
    // unparseable roster yields zero cards (AC2-ERR). The apply arm additionally
    // dedups vs the live `roster_cards`.
    let cards = match crate::claude_roster::ClaudeRoster::load_default() {
        Ok(r) => roster_bg_cards_from(&rows, &r.workers_deduped()),
        Err(_) => Vec::new(),
    };

    DiscoveryResult {
        panes,
        cards,
        probed,
        survived,
    }
}

/// Indices of a [`DiscoveryResult`]'s survivor `panes` whose name is not already
/// in the live `names` (x-87f7). The one new race Approach B introduces: a
/// survivor may have been tiled (startup / manual recruit / a prior discovery)
/// between the task's `names` snapshot and the apply arm. Re-filtering here closes
/// it so the apply arm never double-adds a pane (AC2-FR), keeping the seven
/// parallel pane vectors aligned. Pure.
fn fresh_survivors(panes: &[(String, bool, Value)], names: &[String]) -> Vec<usize> {
    let known: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();
    panes
        .iter()
        .enumerate()
        .filter(|(_, (name, _, _))| !known.contains(name.as_str()))
        .map(|(i, _)| i)
        .collect()
}

// ── Run loop ──────────────────────────────────────────────────────────────

/// Entry point for `fno agents grid` (wired from `run_grid` in
/// [`crate::grid`]).
pub async fn run(parsed: GridArgs, home: &AgentsHome) -> i32 {
    let names = match resolve_agent_names(&parsed, home) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("fno-agents grid: {e}");
            return 1;
        }
    };
    // Authoritative PTY-protocol gate: drop adopted stream-json claude rows
    // (host_mode=interactive + a worker socket, but only `stream.*` RPCs) so
    // `--all` never tiles a non-drivable phantom claude pane (codex review P2).
    // codex/gemini pass through without a probe.
    let names = prune_non_pty_claude(names, home).await;
    // E5b zero-config front door: an empty fleet no longer exits. The grid
    // opens with no panes and the goal launcher active; the operator types a
    // goal and the grid spawns + tiles a `/target` worker live. `names` and
    // `host_interactive` are now mutable so a live-added pane can append.

    // Soft fleet cap (Locked Decision 5): the eager connection policy opens
    // one watcher WS per agent for the whole session, so an unbounded fleet
    // would open unbounded connections. Above the cap we render the first N
    // and warn explicitly (Open Question 1: warn-and-truncate, so the grid
    // still works). The note rides the footer so it stays visible on the
    // alternate screen; the eprintln lands in the operator's scrollback.
    //
    // Cap BEFORE resolving host modes: host_interactive must be resolved on the
    // CAPPED names so it stays 1:1 with names/panes. Resolving first then
    // truncating leaves stale tail entries, and a live-added pane at panes.len()
    // would then read a dropped agent's host mode (codex peer-review P2).
    let total_requested = names.len();
    let (mut names, soft_warn) = apply_soft_cap(names, max_panes());
    if let Some(w) = &soft_warn {
        eprintln!("fno-agents grid: {w}");
    }
    let cap_note: Option<(usize, usize)> = soft_warn.map(|_| (names.len(), total_requested));

    // Per-pane host_mode (interactive => Enter drives it; exec => watch only).
    // Resolved once per agent on the capped names; live-added panes push their
    // own entry. Alignment is load-bearing: every reader indexes
    // host_interactive by pane index == names index, so assert the 1:1 mapping
    // (now post-cap) trips in debug/tests rather than silently mislabeling panes.
    let mut host_interactive = resolve_host_modes(&names, home);
    debug_assert_eq!(
        host_interactive.len(),
        names.len(),
        "host_interactive must be 1:1 with names"
    );

    // Merge claude --bg roster sessions as status cards (x-57eb, Option A).
    // These are interactive threads claude's own daemon owns; they carry NO fno
    // worker socket, so they open zero WS connections and are exempt from the
    // soft cap (the cap bounds eager connections, of which a card has none).
    // `roster_cards` is aligned 1:1 with `names`/`panes`: None for a real fno
    // pane, Some(card) for a bg card. Drive routes to native `claude attach`
    // (the focused-card Enter affordance), never an fno PTY. Gated to --all (the
    // fleet view / bare front door); explicit-name invocations are unaffected.
    let mut roster_cards: Vec<Option<RosterBgCard>> = vec![None; names.len()];
    if parsed.all {
        for card in resolve_bg_roster_cards(home) {
            names.push(card.name.clone());
            host_interactive.push(false); // not an fno PTY: Enter attaches, no keystrokes
            roster_cards.push(Some(card));
        }
    }
    debug_assert_eq!(
        roster_cards.len(),
        names.len(),
        "roster_cards must be 1:1 with names"
    );
    debug_assert_eq!(
        host_interactive.len(),
        names.len(),
        "host_interactive must stay 1:1 with names after the roster merge"
    );

    let daemon_bin = resolve_daemon_bin();
    if let Err(e) = ensure_daemon(home, &daemon_bin).await {
        eprintln!("fno-agents grid: {e}");
        return 1;
    }

    // Initial page layout from the current terminal size. `tty` is kept
    // mutable and re-read on every SIGWINCH; `paint` recomputes the page
    // layout from it each frame so the visible slice always tracks
    // `comp.current_page()`.
    let (tty_cols, tty_rows) = terminal::size().unwrap_or((80, 24));
    let mut tty = TtySize::new(tty_rows, tty_cols);
    // Zero-config front door: with no panes there is nothing to tile, so skip
    // the layout entirely - the launcher renderer truncates to any terminal
    // size, and a too-small terminal must NOT exit the bare front door
    // (codex peer-review P2). `paged0` is None then; downstream uses guard it.
    let paged0: Option<PageLayout> = if names.is_empty() {
        None
    } else {
        match layout::compute_page(tty, names.len(), 0) {
            Ok(p) => Some(p),
            Err(LayoutError::TerminalTooSmall { rows, cols }) => {
                eprintln!("fno-agents grid: terminal too small ({rows}x{cols})");
                return 2;
            }
            Err(LayoutError::ZeroPanes) => {
                eprintln!("fno-agents grid: no agents to tile");
                return 2;
            }
        }
    };

    let (tx, mut rx) = mpsc::channel::<PaneMsg>(256);
    // Eager connection policy (Locked Decision 3): one watcher WS per agent
    // for the WHOLE fleet, open the whole session. panes/states/watch_sinks
    // are indexed by GLOBAL pane index (stable for the session); the page
    // layout decides which slice renders. Off-screen panes keep draining
    // their Term so flips are warm and the attention scanner has live state.
    let mut watch_sinks: Vec<Option<WsSink>> = Vec::with_capacity(names.len());
    let mut panes: Vec<Pane> = Vec::with_capacity(names.len());
    let mut states: Vec<ConnState> = Vec::with_capacity(names.len());

    for (idx, name) in names.iter().enumerate() {
        // Non-empty fleet => paged0 is Some (only the empty front door is None).
        let layout0 = paged0.as_ref().expect("non-empty fleet has a layout");
        let (rows, cols) = target_pane_inner(layout0, idx);
        if let Some(card) = roster_cards.get(idx).and_then(Option::as_ref) {
            // Roster --bg card: no worker socket exists, so NEVER open_watch_pane
            // (AC-EDGE). A static card is fed once; the pane has no live stream.
            panes.push(bg_card_pane(card, rows, cols, unix_now_secs()));
            states.push(ConnState::BgRoster);
            watch_sinks.push(None);
        } else {
            let (pane, state, sink) = open_watch_pane(home, name, idx, rows, cols, &tx).await;
            panes.push(pane);
            states.push(state);
            watch_sinks.push(sink);
        }
    }

    let mut comp = Compositor::new(panes.len());
    // Seed pagination with the real capacity so page_count / current_page are
    // correct from the first frame. The empty front door (paged0 None) has no
    // panes; capacity 1 is a harmless placeholder until the first live-add.
    comp.recompute_pagination(paged0.as_ref().map(|p| p.capacity).unwrap_or(1));
    // The single interactive drive connection, opened lazily for the pane being
    // driven (one slot against the daemon's interactive-drive cap).
    let mut drive_sink: DriveSink = None;

    // Terminal setup via crossterm. Using `crossterm::terminal::enable_raw_mode`
    // (rather than a hand-rolled libc cfmakeraw) is load-bearing: it
    // initializes crossterm's internal event subsystem, which `EventStream`
    // depends on. Bypassing it leaves the event source uninitialized and can
    // panic with "reader source not set" on the first poll (observed in real
    // terminals). The guard restores raw mode + screen + cursor on every exit
    // path (normal return, `?`, panic unwind, signal).
    let _guard = match TerminalGuard::enter() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("fno-agents grid: cannot set up terminal: {e}");
            return 1;
        }
    };
    // Query the terminal's default fg/bg colors via OSC 10/11.
    // Must run AFTER enable_raw_mode (raw mode is required to read the response)
    // and BEFORE the EventStream loop (which consumes stdin). Degrades to
    // Palette::fixed() (all-Default, unchanged chrome) on any failure or timeout.
    let palette = crate::grid::palette::query_terminal_palette();
    let mut stderr = io::stderr();

    let mut reader = EventStream::new();
    // 30fps render cap. The frame is painted ONLY when `dirty` is set (a real
    // state change: key, pane bytes, resize, pane-state transition). An idle
    // grid never repaints, so it never fills the PTY output buffer - which
    // matters because the synchronous `stderr` write below would otherwise
    // block the current_thread executor when the terminal stops draining
    // (full buffer, or Ctrl-S flow control), starving input. The tick only
    // coalesces rapid changes into at most one paint per frame; it does NOT
    // force a repaint on its own.
    let mut tick = tokio::time::interval(Duration::from_millis(33));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Transient one-line footer message (e.g. the exec watch-only hint).
    // Cleared on the next operator key so it never lingers. (ab-7fd7ae49)
    let mut hint: Option<String> = None;

    // E5b launcher: Some while the operator is typing a goal. Starts open on
    // the zero-config front door (no panes); otherwise opened with `n` in
    // WATCH. `launch_seq` makes each spawned worker name unique.
    let mut launcher: Option<Launcher> = if names.is_empty() {
        Some(Launcher::new())
    } else {
        None
    };
    let mut launch_seq: usize = 0;
    // Open recruit prompt (x-5b3e): `Some` while the operator is typing the
    // squad name to recruit the focused agent into. Modal like `launcher`.
    let mut recruit: Option<RecruitPrompt> = None;
    // Last squad recruited into this session (x-0175 US2): pre-seeds the next
    // recruit prompt so building one squad from several agents is `m`-Enter,
    // `m`-Enter. In-memory / session-scoped (Locked Decision 3).
    // ponytail: session-scoped; persist to squads.json only if operators ask
    // for cross-restart memory.
    let mut last_squad: Option<String> = None;

    // ── Rail state (ab-1fab1fdf, Phase 1) ────────────────────────────────
    // `rail_state`: Some when rail mode is active (`--rail` flag or `t` toggle).
    // None means the default railless tiled grid (Locked Decision 5).
    //
    // `rail_rows`: the registry rows used for grouping. For `--rail` mode these
    // are read once from the registry at startup and re-used each frame; future
    // work can refresh them on a poll cadence. For now they shadow the `names`
    // list: one synthetic row per name (name only; cwd/provider/status come from
    // a real registry read when --all is used; for explicit names we use the
    // basic row). The group_by key still works - missing fields -> "unknown".
    //
    // This is a conservative Phase 1 choice (Discretion #2: keep all panes
    // subscribed; no lazy subscription). The rail groups the same names the
    // existing grid already shows - zero new I/O.
    // Repo-rollup resolution cache (x-cb89): cwd -> canonical repo root,
    // resolved once per distinct cwd. Lives for the whole run so the rail
    // snapshot AND every live-added worker share it - git never re-spawns for a
    // cwd already seen (the Concurrency failure mode: no per-frame git).
    let mut repo_cache = repo::RepoCache::new();
    // x-c226 negative-probe cache: claude name -> Instant its last `worker.ping`
    // gate probe failed. Lives for the whole run so the live-discover poll skips
    // re-probing stable stream-json lanes (host_mode=interactive but no
    // worker.ping) every tick, which was starving grid input on a busy fleet.
    let mut failed_probe_cache: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();
    // x-87f7 in-flight guard: true while a detached discover_children task is
    // running. Set when the ping arm spawns it; reset on the PaneMsg::Discovered
    // apply arm (always, even on an error/empty result - the task ALWAYS sends -
    // so discovery can never permanently wedge, AC3-FR). At most one task at a
    // time, so the cache write-back has no lost-update race (Invariant).
    let mut discover_in_flight = false;
    let mut initial_rail_rows: Vec<Value> = {
        // Read registry rows once for richer grouping (cwd, provider, status).
        // Fall back to name-only synthetic rows if the registry is absent.
        let reg_path = home.registry_json();
        let maybe_rows: Vec<Value> = std::fs::read(&reg_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| {
                v.get("agents")
                    .or_else(|| v.get("entries"))
                    .and_then(Value::as_array)
                    .cloned()
            })
            .unwrap_or_default();

        // Build a row for each name: prefer registry row when present, else
        // synthesize a minimal one so grouping produces "unknown" for missing fields.
        names
            .iter()
            .map(|n| {
                maybe_rows
                    .iter()
                    .find(|r| r.get("name").and_then(Value::as_str) == Some(n.as_str()))
                    .cloned()
                    .unwrap_or_else(|| json!({"name": n}))
            })
            .collect()
    };
    // Stamp each snapshot row with its repo-rollup identity (`_repo_root` +
    // `_worktree`) so the very first `group_by` below already rolls a repo's
    // worktrees up under one sideline (x-cb89).
    repo::stamp_rows(&mut initial_rail_rows, &mut repo_cache);

    // Manual squads (x-5b3e), read once from the GLOBAL ~/.fno/squads.json and
    // reloaded after a recruit. Held in memory so the Squad rail view and the
    // per-frame paint never read the file in the render loop. A corrupt store
    // degrades to empty (squads::load never panics).
    let squads_path = squads::squads_path();
    let (mut squad_store, squad_warn) = squads::load_reporting(&squads_path);
    // Surface a corrupt-store warning as a startup hint, never via eprintln
    // (the compositor owns the terminal; a stderr write would garble the TUI).
    if let Some(w) = squad_warn {
        hint = Some(format!("squads: {w}"));
    }

    // Active rail state; initialized from `--rail` flag.
    let mut rail_state: Option<RailState> = if parsed.rail {
        let mut rs = RailState::new(parsed.initial_group_key());
        let groups = base_groups(&initial_rail_rows, rs.group_key, &squad_store);
        // Seed compositor focus from the first selected agent so the main pane
        // and drive target are aligned from the first frame (AC1-HP: main shows
        // the first group's first agent), matching the `t`-toggle-on path.
        if let Some(sel) = rs.re_anchor(&groups) {
            comp.set_focus(sel);
        }
        Some(rs)
    } else {
        None
    };
    // Leader-key input model for the railless tiled grid (x-b563, Phase 1).
    // Resolved once from `config.grid.leader_key` (default Ctrl-Space). The
    // leader only routes input on the tiled compositor path; the rail keeps its
    // own RailNav/PaneDrive model (Phase 2, x-d97d). `leader` carries the
    // Normal/Pending sub-state across key events.
    let leader_cfg = leader::resolve_leader_key(&std::env::current_dir().unwrap_or_default());
    let mut leader = LeaderState::Normal;

    // Registry rows shadowing the current names list (rebuilt on `t` toggle
    // and after registry refreshes; stable within a session for Phase 1).
    // Mutable so an E5b live-added worker pushes a row in lockstep with its
    // pane, keeping rail_rows 1:1 with panes (codex peer-review P2).
    let mut rail_rows: Vec<Value> = initial_rail_rows;
    // If launched with the rail, size the panes for the INITIAL main_mode so
    // the first frame is correct without waiting for a resize event. GroupTile
    // is the E5c default (AC-2), so a space with >1 live agent must tile its
    // members from frame one; Single sizes the focused pane to the full main
    // area (gemini HIGH). Mirrors the `Tab`-toggle resize paths (codex P2).
    if let Some(rs) = rail_state.as_ref() {
        match rs.main_mode {
            group::MainMode::GroupTile => {
                apply_group_tile_resize(
                    rs,
                    tty,
                    &rail_rows,
                    &states,
                    &squad_store,
                    &mut panes,
                    &mut watch_sinks,
                )
                .await;
            }
            group::MainMode::Single => {
                resize_rail_focus(tty, comp.focus(), &mut panes, &mut watch_sinks).await;
            }
        }
    }

    /// Helper: compute groups + LIVE attention badges. Groups partition the
    /// frozen `rail_rows` snapshot (their cwd/session/provider/status fields are
    /// stable), but badges derive from the live per-pane signals - `is_waiting`
    /// (the readiness scan) and `ConnState::Exited` - NOT the registry status
    /// string, which never carries needs-input/exited for a running agent
    /// (sigma-review: badges read from the stale snapshot would never fire).
    fn rail_groups_and_badges(
        rail_rows: &[Value],
        rs: &RailState,
        panes: &[Pane],
        states: &[ConnState],
        squads: &squads::SquadStore,
    ) -> (Vec<group::Group>, Vec<group::GroupBadge>) {
        let waiting: Vec<bool> = panes
            .iter()
            .zip(states.iter())
            .map(|(p, s)| p.is_waiting(s))
            .collect();
        let exited: Vec<bool> = states
            .iter()
            .map(|s| matches!(s, ConnState::Exited { .. }))
            .collect();
        // Inside-leg authority (E3.3): the middle tier between PTY-exit and the
        // scraper. Parsed from `rail_rows` (the same index `waiting`/`exited`
        // use), so it is snapshot-bound to startup like the rest of `rail_rows`;
        // the TTL gate (`now_secs`) makes that safe - a stale `working` ages out
        // and the scraper takes over rather than pinning forever. A live
        // per-frame registry refresh is the run loop's already-noted follow-up.
        let inside_leg: Vec<Option<crate::state::InsideLegReport>> = rail_rows
            .iter()
            .map(|row| {
                row.get("inside_leg")
                    .filter(|v| !v.is_null())
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
            })
            .collect();
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // The attention filter (`a`) reduces the rendered groups to those needing
        // the operator; badges then derive from the visible set, so the global
        // footer summary matches what the rail shows (`!N`, no `xM` once exited
        // are filtered out). The filter uses the SAME 3-tier-resolved signal as
        // the badges (E3.3, codex P2) - not the raw scraper `waiting` - so a live
        // `working` is not falsely surfaced and a `blocked` is not hidden.
        let mut groups = base_groups(rail_rows, rs.group_key, squads);
        if rs.attention_filter {
            let attn = group::needs_input_after_authority(&waiting, &exited, &inside_leg, now_secs);
            groups = group::attention_view(&groups, &attn);
        }
        let badges =
            group::compute_badges_from_live(&groups, &waiting, &exited, &inside_leg, now_secs);
        (groups, badges)
    }

    // First frame: prev_frame is None, so this takes the full-paint path
    // (Clear + emit_full). Every subsequent dirty tick diffs against the frame
    // recorded here, so the steady-state repaint never re-clears the screen.
    let mut prev_frame: Option<ScreenBuffer> = None;
    if panes.is_empty() {
        // E5b front door: no panes yet, render the goal launcher.
        paint_front_door(
            &mut stderr,
            &mut prev_frame,
            tty,
            launcher.as_ref().map(|l| l.buffer.as_str()).unwrap_or(""),
            hint.as_deref(),
        );
    } else {
        let rail_arg = rail_state.as_ref().map(|rs| {
            let (groups, badges) =
                rail_groups_and_badges(&rail_rows, rs, &panes, &states, &squad_store);
            (rs, groups, badges)
        });
        // Borrow-split: paint needs &[Value] for rows, but groups/badges own their data.
        // Pass None for railless, or reconstruct inside paint via closure approach.
        // Simpler: compute groups/badges outside and pass refs.
        match rail_arg {
            Some((rs, groups, badges)) => paint(
                &mut stderr,
                &mut prev_frame,
                tty,
                &names,
                &panes,
                &states,
                &comp,
                &host_interactive,
                hint.as_deref(),
                cap_note,
                Some((rs, &groups, &badges, &rail_rows)),
                false, // help_open: overlay always starts closed
                &palette,
            ),
            None => paint(
                &mut stderr,
                &mut prev_frame,
                tty,
                &names,
                &panes,
                &states,
                &comp,
                &host_interactive,
                hint.as_deref(),
                cap_note,
                None,
                false, // help_open: overlay always starts closed
                &palette,
            ),
        }
    }
    let mut dirty = false;
    // `?` help overlay (E5c AC-3): toggled by `?` in WATCH; false = hidden.
    let mut help_open = false;

    loop {
        tokio::select! {
            maybe_event = reader.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        // ── E5b launcher (modal) ──────────────────────────
                        // While the launcher is open every key edits the goal
                        // buffer; nothing reaches the panes or compositor. With
                        // panes present the buffer shows as a footer line; on
                        // the empty front door the front-door paint renders it.
                        // Checked FIRST so the modal front door owns input (e.g.
                        // `?` is typed into the goal, not stolen by the help gate).
                        if launcher.is_some() {
                            match launcher.as_mut().unwrap().apply(launcher_key(key)) {
                                LauncherOutcome::Stay => {
                                    if !panes.is_empty() {
                                        hint = Some(format!(
                                            "goal> {}",
                                            launcher.as_ref().unwrap().buffer
                                        ));
                                    }
                                }
                                LauncherOutcome::Cancelled => {
                                    launcher = None;
                                    if panes.is_empty() {
                                        break; // front door: nothing else to show
                                    }
                                    hint = None;
                                    prev_frame = None; // clear the footer overlay
                                }
                                LauncherOutcome::Submitted(input) => {
                                    launcher = None;
                                    launch_seq += 1;
                                    // `>`-prefixed input opens an interactive
                                    // claude pane (type straight in); a plain goal
                                    // dispatches an autonomous `/target` worker.
                                    // Both land an interactive claude row, so the
                                    // live-add tail below is shared.
                                    let (name, spawn_res) = match classify_launch(&input) {
                                        LaunchKind::InteractivePane(msg) => {
                                            let name = interactive_pane_name(launch_seq);
                                            let r =
                                                spawn_interactive_claude_pane(&name, &msg).await;
                                            (name, r)
                                        }
                                        LaunchKind::TargetWorker(goal) => {
                                            let name = target_worker_name(&goal, launch_seq);
                                            let r = spawn_target_worker(&name, &goal).await;
                                            (name, r)
                                        }
                                    };
                                    match spawn_res {
                                        Ok(()) => {
                                            // Live-add the worker's pane via the shared
                                            // helper (x-45e6 Change 1). The launcher
                                            // FOCUSES it (the human just launched it);
                                            // the worker spawns in this grid's cwd, so a
                                            // synthetic claude+cwd rail row groups it with
                                            // its siblings (x-cb89).
                                            let rail_row = json!({
                                                "name": name.as_str(),
                                                "provider": "claude",
                                                "cwd": std::env::current_dir()
                                                    .ok()
                                                    .and_then(|p| p.to_str().map(str::to_string)),
                                            });
                                            let tiled = live_add_pane(
                                                home, &name, true, &tx, tty, &mut comp,
                                                &mut names, &mut panes, &mut states,
                                                &mut watch_sinks, &mut host_interactive,
                                                &mut roster_cards, &mut rail_rows,
                                                &mut repo_cache, rail_row, true,
                                            )
                                            .await;
                                            hint = Some(if tiled {
                                                format!("launched {name}")
                                            } else {
                                                // Worker is running; the terminal just
                                                // can't tile another pane yet.
                                                format!("{name} launched (terminal too small to tile it)")
                                            });
                                            if tiled {
                                                // Rail mode: size the new pane for the
                                                // rail region, not the tiled grid
                                                // live_add_pane just applied (codex
                                                // PR #98 P2).
                                                reapply_rail_sizing(
                                                    rail_state.as_ref(),
                                                    tty,
                                                    &rail_rows,
                                                    &states,
                                                    &squad_store,
                                                    comp.focus(),
                                                    &mut panes,
                                                    &mut watch_sinks,
                                                )
                                                .await;
                                            }
                                            prev_frame = None; // region map changed -> full repaint
                                        }
                                        Err(e) => {
                                            // No pane added; keep the operator informed.
                                            hint = Some(format!("launch failed: {e}"));
                                            prev_frame = None;
                                        }
                                    }
                                    // Front door: if the submit added no pane,
                                    // reopen the launcher so input stays modal -
                                    // otherwise a stray nav key would reach
                                    // comp.step(_, &[]) and Quit the grid. The
                                    // hint renders on the front-door status line.
                                    if panes.is_empty() {
                                        launcher = Some(Launcher::new());
                                    }
                                }
                            }
                            dirty = true;
                            continue;
                        }
                        // ── Recruit prompt (x-5b3e, modal) ─────────────────
                        // While open every key edits the squad-name buffer;
                        // nothing reaches the rail or panes. Submit creates the
                        // squad if new and recruits the captured agent (AC1-HP /
                        // AC1-UI); the outcome toast makes a re-recruit a visible
                        // no-op. After the launcher so only one modal owns input.
                        if recruit.is_some() {
                            // Up/Down cycle existing squad names into the buffer
                            // (US1/AC1-UI). Inert with zero candidates but still
                            // consumed so the key never leaks to the rail
                            // (AC1-EDGE). Handled before `apply` because arrows map
                            // to Ignore in the launcher keymap.
                            if matches!(key.code, KeyCode::Up | KeyCode::Down) {
                                let rp = recruit.as_mut().unwrap();
                                if !rp.candidates.is_empty() {
                                    let on = rp.input.buffer == rp.candidates[rp.cursor];
                                    rp.cursor = cycle_cursor(
                                        rp.candidates.len(),
                                        rp.cursor,
                                        on,
                                        matches!(key.code, KeyCode::Down),
                                    );
                                    rp.input.buffer = rp.candidates[rp.cursor].clone();
                                }
                                hint = Some(recruit_hint(recruit.as_ref().unwrap()));
                                dirty = true;
                                continue;
                            }
                            match recruit.as_mut().unwrap().input.apply(launcher_key(key)) {
                                LauncherOutcome::Stay => {
                                    hint = Some(recruit_hint(recruit.as_ref().unwrap()));
                                }
                                LauncherOutcome::Cancelled => {
                                    recruit = None;
                                    hint = None;
                                    prev_frame = None; // clear the footer overlay
                                }
                                LauncherOutcome::Submitted(squad_name) => {
                                    let agent = recruit.take().unwrap().agent;
                                    let now = crate::events::now_rfc3339();
                                    match squads::update(&squads_path, |s| {
                                        // create-if-absent then recruit: a new name
                                        // is created, an existing one is reused, and
                                        // the agent is added (deduped) either way.
                                        s.create(&squad_name, &now);
                                        s.recruit(&squad_name, &agent)
                                    }) {
                                        Ok(outcome) => {
                                            // Reload so the Squad view reflects the
                                            // new membership on the next frame.
                                            squad_store = squads::load(&squads_path);
                                            // Remember this squad so the next `m`
                                            // pre-seeds it (AC2-HP). Set on any Ok
                                            // (create-if-absent means the squad now
                                            // exists, even on AlreadyMember).
                                            last_squad = Some(squad_name.clone());
                                            hint = Some(match outcome {
                                                squads::RecruitOutcome::Recruited => {
                                                    format!("recruited {agent} into *{squad_name}")
                                                }
                                                squads::RecruitOutcome::AlreadyMember => {
                                                    format!("{agent} already in *{squad_name}")
                                                }
                                                squads::RecruitOutcome::NoSuchSquad => {
                                                    format!("could not recruit {agent}")
                                                }
                                            });
                                        }
                                        Err(e) => {
                                            hint = Some(format!("recruit failed: {e}"));
                                        }
                                    }
                                    prev_frame = None;
                                }
                            }
                            dirty = true;
                            continue;
                        }
                        // ── ? help overlay key gate (E5c AC-3) ──────────────
                        // Checked when the launcher is closed. BEFORE the rail
                        // handler and key_to_input so the overlay intercepts keys
                        // without altering the normal keymap. Ctrl-C passes through
                        // (Passthrough); when open, Inert swallows keys incl. `n`.
                        match help_key_action(key, help_open) {
                            HelpAction::Close => {
                                help_open = false;
                                prev_frame = None; // overlay disappears -> full-paint
                                dirty = true;
                                continue;
                            }
                            HelpAction::Inert => {
                                // Key swallowed; overlay stays open.
                                continue;
                            }
                            HelpAction::Passthrough => {
                                // Fall through to the normal key handling below.
                            }
                        }
                        // E5b: the goal launcher opens via leader+n now (bare `n`
                        // types to the focused agent in the owned-PTY model); see the
                        // tiled leader dispatch below. The empty front door still
                        // starts the launcher open on its own.
                        // ── Rail-mode key handling (ab-1fab1fdf, Phase 1) ──
                        // Intercept rail keys BEFORE key_to_input so `g`/`t`/`d`/
                        // Up/Down/Enter/Esc are not forwarded to the existing
                        // compositor when the rail is active. In PaneDrive, only
                        // Esc and Ctrl-C are consumed here; everything else passes
                        // through to the PTY forwarding path.
                        let mut rail_consumed = false;
                        // Rail key handling applies ONLY when the rail actually
                        // renders. When the terminal is too narrow, paint degrades to
                        // the tiled grid, so keys must behave as the tiled grid too -
                        // not a confusing half-rail where arrows/Enter are intercepted
                        // by the (invisible) rail (codex P2). rail_state is left intact;
                        // the rail returns automatically when the terminal widens.
                        let rail_fits = layout::compute_with_rail(tty, layout::RAIL_COLS, 1).is_ok();
                        if rail_fits {
                        if let Some(rs) = rail_state.as_mut() {
                            let ctrl = key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL);
                            // Owned-PTY rail (x-1356): the resting state is DRIVING the
                            // focused pane. The leader opens a sustained "rail-nav" browse
                            // sub-mode (like scrollback); its keys then read un-prefixed
                            // until Esc/Enter. `nav_key` is Some when this key runs the rail
                            // keymap below - fed either by the sustained sub-mode or by the
                            // leader command that just entered it. When None, a bare key
                            // falls through to the PTY-forward path (rail_consumed stays
                            // false), exactly like the tiled grid.
                            let nav_key: Option<KeyCode> = if rs.nav_mode {
                                Some(key.code)
                            } else {
                                let (next, decision) = leader::step(leader, &key, &leader_cfg);
                                leader = next;
                                match decision {
                                    LeaderDecision::EnterPending => {
                                        let lk = leader_cfg.format_compact();
                                        hint = Some(format!(
                                            "LEADER ({lk}) - rail: \u{2191}\u{2193} select \u{b7} g group \u{b7} Tab zoom \u{b7} a attn \u{b7} ] [ page \u{b7} m recruit \u{b7} r remove \u{b7} t hide \u{b7} Esc drive"
                                        ));
                                        dirty = true;
                                        rail_consumed = true;
                                        None
                                    }
                                    LeaderDecision::SendPrefix => {
                                        // Double-tap leader -> the literal leader byte to the
                                        // focused PTY (no key is permanently stolen).
                                        let act = comp.step(
                                            InputEvent::Keystroke(leader::leader_bytes(&leader_cfg)),
                                            &states,
                                        );
                                        if handle_action(act, &mut drive_sink, &names, home).await {
                                            break;
                                        }
                                        dirty = true;
                                        rail_consumed = true;
                                        None
                                    }
                                    LeaderDecision::Command(cmdkey) => {
                                        // Enter the sustained browse sub-mode, seeded by this key.
                                        rs.enter_nav();
                                        Some(cmdkey.code)
                                    }
                                    // Bare key while driving: forward to the focused PTY below.
                                    LeaderDecision::Forward => None,
                                }
                            };
                            if let Some(code) = nav_key {
                                rail_consumed = true;
                                match code {
                                // Ctrl-C always quits.
                                KeyCode::Char('c') if ctrl => {
                                    break;
                                }
                                // Esc returns to driving the focused pane (no claim to
                                // release - focus already IS the drive target).
                                KeyCode::Esc => {
                                    rs.exit_nav();
                                    prev_frame = None; // mode change -> full-paint
                                    hint = None;
                                    dirty = true;
                                }
                                // `t` hides the rail (back to the tiled grid).
                                KeyCode::Char('t') => {
                                    rail_state = None;
                                    // The rail enlarged the focused pane to the main
                                    // area; re-tile every pane so the railless grid
                                    // renders them at their (smaller) tile sizes.
                                    if let Ok(paged) =
                                        layout::compute_page(tty, panes.len(), comp.current_page())
                                    {
                                        resize_all_panes(&paged, &mut panes, &mut watch_sinks).await;
                                    }
                                    prev_frame = None; // region map changed -> full-paint
                                    hint = None;
                                    dirty = true;
                                    rail_consumed = true;
                                }
                                // nav: `g` cycles the group-by key. Selection
                                // re-anchors to the same agent and the compositor focus
                                // follows it so the main pane and drive target stay on
                                // that agent across the re-partition (AC4-FR).
                                (KeyCode::Char('g')) => {
                                    rs.cycle_group_key();
                                    let groups = rail_view_groups(&rail_rows, rs, &panes, &states, &squad_store);
                                    if let Some(sel) = rs.re_anchor(&groups) {
                                        comp.set_focus(sel);
                                        match rs.main_mode {
                                            group::MainMode::GroupTile => {
                                                // Focus follows the agent into its new
                                                // group; size that group's tiles (AC4-FR).
                                                apply_group_tile_resize(rs, tty, &rail_rows, &states, &squad_store,
                                                    &mut panes, &mut watch_sinks).await;
                                            }
                                            group::MainMode::Single => {
                                                resize_rail_focus(tty, sel, &mut panes,
                                                    &mut watch_sinks).await;
                                            }
                                        }
                                    }
                                    prev_frame = None; // re-partition -> full-paint
                                    hint = None;
                                    dirty = true;
                                    rail_consumed = true;
                                }
                                // nav: Up/Down move the selection; the compositor
                                // focus follows so the selected agent fills the main
                                // area (AC1-UI / AC2-HP).
                                (KeyCode::Up) => {
                                    let groups = rail_view_groups(&rail_rows, rs, &panes, &states, &squad_store);
                                    if let Some(sel) = rs.move_up(&groups) {
                                        comp.set_focus(sel);
                                        match rs.main_mode {
                                            group::MainMode::GroupTile => {
                                                // Selection move can cross a page; size the
                                                // group's tiles + force a full repaint.
                                                apply_group_tile_resize(rs, tty, &rail_rows, &states, &squad_store,
                                                    &mut panes, &mut watch_sinks).await;
                                                prev_frame = None;
                                            }
                                            group::MainMode::Single => {
                                                resize_rail_focus(tty, sel, &mut panes,
                                                    &mut watch_sinks).await;
                                            }
                                        }
                                    }
                                    hint = None;
                                    dirty = true;
                                    rail_consumed = true;
                                }
                                (KeyCode::Down) => {
                                    let groups = rail_view_groups(&rail_rows, rs, &panes, &states, &squad_store);
                                    if let Some(sel) = rs.move_down(&groups) {
                                        comp.set_focus(sel);
                                        match rs.main_mode {
                                            group::MainMode::GroupTile => {
                                                apply_group_tile_resize(rs, tty, &rail_rows, &states, &squad_store,
                                                    &mut panes, &mut watch_sinks).await;
                                                prev_frame = None;
                                            }
                                            group::MainMode::Single => {
                                                resize_rail_focus(tty, sel, &mut panes,
                                                    &mut watch_sinks).await;
                                            }
                                        }
                                    }
                                    hint = None;
                                    dirty = true;
                                    rail_consumed = true;
                                }
                                // nav: Enter on a tile in GroupTile drills INTO it -
                                // drop to Single focused on the selected tile (Open Q2 /
                                // drill-down). This guarded arm precedes the Enter/`d`
                                // drive arm, so Enter-in-GroupTile zooms while Enter-in-
                                // Single (one tile already) and `d`-in-either still drive.
                                // The progression reads Enter-deeper: GroupTile -> Single
                                // -> Drive; Tab/Esc back out. Mirrors the Tab->Single path
                                // (resize the focused pane to the full main area, then
                                // full-paint the region-map change).
                                (KeyCode::Enter)
                                    if matches!(rs.main_mode, group::MainMode::GroupTile) =>
                                {
                                    // codex P2: if the selection is on a member that
                                    // exited (filtered out of the visible tiles by the
                                    // AC3-FR reflow), drill into a visible survivor
                                    // rather than zooming Single onto a dead pane that
                                    // was never on screen. Re-anchor to the selected
                                    // group's first live member; if the whole group is
                                    // dead, leave the selection (Single then shows the
                                    // exited pane - nothing live to focus).
                                    let view = rail_view_groups(&rail_rows, rs, &panes, &states, &squad_store);
                                    if let Some(sel_group) = rs.selected_group(&view) {
                                        let live = live_group_of(sel_group, &states);
                                        let on_live = rs
                                            .selected_agent_idx
                                            .is_some_and(|s| live.members.contains(&s));
                                        if !on_live {
                                            if let Some(&first) = live.members.first() {
                                                // Keep the (agent, group key) pair
                                                // coherent: `first` lives in this
                                                // same group, so pin the key too
                                                // rather than leaving it stale.
                                                rs.selected_agent_idx = Some(first);
                                                // `live` is unused after this; move
                                                // its key out rather than cloning.
                                                rs.selected_group_key = Some(live.key_value);
                                            }
                                        }
                                    }
                                    rs.toggle_main_mode(); // GroupTile -> Single (guarded)
                                    if let Some(sel) = rs.selected_agent_idx {
                                        comp.set_focus(sel);
                                        resize_rail_focus(tty, sel, &mut panes,
                                            &mut watch_sinks).await;
                                    }
                                    prev_frame = None; // region map changed -> full-paint
                                    hint = None;
                                    dirty = true;
                                    rail_consumed = true;
                                }
                                // Enter / `d`: select the agent and return to driving it.
                                // Owned-PTY: no promote, no claim, no denial - aligning focus
                                // IS driving. A dead or exec pane can't be driven, so surface
                                // a cue and stay in nav rather than dropping into a pane that
                                // eats keystrokes.
                                (KeyCode::Enter)
                                | (KeyCode::Char('d')) => {
                                    let sel = rs.selected_agent_idx;
                                    let drivable = sel
                                        .and_then(|i| states.get(i))
                                        .map(ConnState::is_drivable)
                                        .unwrap_or(false);
                                    let interactive = sel
                                        .map(|i| host_interactive.get(i).copied().unwrap_or(false))
                                        .unwrap_or(false);
                                    let who = sel
                                        .and_then(|i| names.get(i))
                                        .map(String::as_str)
                                        .unwrap_or("agent");
                                    if !drivable {
                                        hint = Some(format!("{who}: not drivable (exited / disconnected)"));
                                    } else if !interactive {
                                        // Exec agents are one-shot and watch-only (host_mode
                                        // marker); their pane connection is read-only.
                                        hint = Some(format!(
                                            "{who} is an exec agent - watch only (host_mode=interactive drives)"
                                        ));
                                    } else if let Some(idx) = sel {
                                        // Align focus to the selected agent and drop back to
                                        // driving it - bare keys now reach its owned PTY.
                                        comp.set_focus(idx);
                                        rs.exit_nav();
                                    }
                                    prev_frame = None; // mode change -> full-paint
                                    dirty = true;
                                    rail_consumed = true;
                                }
                                // nav: `m` recruits the selected agent into a
                                // squad (x-5b3e). Opens the modal squad-name prompt
                                // seeded with the agent's name; the recruit itself
                                // runs on submit. The agent is captured now so a
                                // later selection move cannot retarget it. (When the
                                // x-d97d rail leader lands, this rebinds to
                                // `leader m` - the recruit verb is unchanged.)
                                (KeyCode::Char('m')) => {
                                    if let Some(agent) =
                                        rs.selected_agent_idx.and_then(|i| names.get(i)).cloned()
                                    {
                                        let (candidates, cursor) =
                                            recruit_candidates(&squad_store, last_squad.as_deref());
                                        let mut input = Launcher::new();
                                        // Pre-seed with the last-recruited squad (AC2-HP) so
                                        // Enter immediately re-recruits into it.
                                        if let Some(seed) = last_squad.as_deref() {
                                            input.buffer = seed.to_string();
                                        }
                                        let rp = RecruitPrompt {
                                            agent,
                                            input,
                                            candidates,
                                            cursor,
                                        };
                                        hint = Some(recruit_hint(&rp));
                                        recruit = Some(rp);
                                    } else {
                                        hint = Some("no agent selected to recruit".to_string());
                                    }
                                    dirty = true;
                                    rail_consumed = true;
                                }
                                // nav: `r` removes the selected agent from the
                                // selected squad (x-0175 US3). Squad view only
                                // (Locked Decision 4): elsewhere a toast says to
                                // switch (AC1-ERR). Squad + agent are captured at
                                // keypress so a later selection move cannot retarget
                                // the mutation (Concurrency), mirroring `m`. Always
                                // emits a toast - never a silent no-op.
                                (KeyCode::Char('r')) => {
                                    if rs.group_key != group::GroupKey::Squad {
                                        hint = Some(
                                            "switch to squad view (g) to remove".to_string(),
                                        );
                                    } else {
                                        let groups = rail_view_groups(
                                            &rail_rows, rs, &panes, &states, &squad_store,
                                        );
                                        let squad = rs
                                            .selected_group(&groups)
                                            .and_then(|g| g.key_value.strip_prefix("squad:"))
                                            .map(str::to_string);
                                        let agent = rs
                                            .selected_agent_idx
                                            .and_then(|i| names.get(i))
                                            .cloned();
                                        match (squad, agent) {
                                            (Some(squad), Some(agent)) => {
                                                match squads::update(&squads_path, |s| {
                                                    s.remove_member(&squad, &agent)
                                                }) {
                                                    Ok(true) => {
                                                        squad_store = squads::load(&squads_path);
                                                        hint = Some(format!(
                                                            "removed {agent} from *{squad}"
                                                        ));
                                                        // Membership changed -> the rail
                                                        // layout shifts; force a full paint.
                                                        // The no-op paths only change the
                                                        // footer hint, which `dirty` repaints.
                                                        prev_frame = None;
                                                    }
                                                    Ok(false) => {
                                                        // No local change, but `update`
                                                        // re-read the file under lock: a
                                                        // concurrent instance may have moved
                                                        // the store on, so reload to converge
                                                        // (codex P2). No full paint - the rail
                                                        // membership the operator sees is
                                                        // unchanged by THIS no-op.
                                                        squad_store = squads::load(&squads_path);
                                                        hint = Some(format!(
                                                            "{agent} not in *{squad}"
                                                        ));
                                                    }
                                                    Err(e) => {
                                                        hint =
                                                            Some(format!("remove failed: {e}"));
                                                    }
                                                }
                                            }
                                            _ => {
                                                hint = Some("no agent selected".to_string());
                                            }
                                        }
                                    }
                                    dirty = true;
                                    rail_consumed = true;
                                }
                                // nav: `q` quits.
                                (KeyCode::Char('q')) => {
                                    break;
                                }
                                // nav: Tab toggles the main area between Single and
                                // GroupTile (US3). The region map changes (one big pane
                                // <-> a tiled group), so force a full repaint and re-size
                                // the now-visible panes - the flip is atomic (AC3-UI).
                                (KeyCode::Tab) => {
                                    rs.toggle_main_mode();
                                    match rs.main_mode {
                                        group::MainMode::GroupTile => {
                                            apply_group_tile_resize(rs, tty, &rail_rows, &states, &squad_store,
                                                &mut panes, &mut watch_sinks).await;
                                        }
                                        group::MainMode::Single => {
                                            // Back to one big pane: re-size the focused
                                            // pane to the full main area (gemini HIGH).
                                            if let Some(sel) = rs.selected_agent_idx {
                                                resize_rail_focus(tty, sel, &mut panes,
                                                    &mut watch_sinks).await;
                                            }
                                        }
                                    }
                                    prev_frame = None; // region map changed -> full-paint
                                    hint = None;
                                    dirty = true;
                                    rail_consumed = true;
                                }
                                // nav: page within a tiled group (GroupTile only).
                                // ]/PageDown -> next page, [/PageUp -> previous. Moves the
                                // selection by a page (the rendered page follows it), so
                                // the accented/drive target stays on screen. Inert in
                                // Single mode (AC3-ERR).
                                (KeyCode::Char(']'))
                                | (KeyCode::PageDown)
                                | (KeyCode::Char('['))
                                | (KeyCode::PageUp) => {
                                    if matches!(rs.main_mode, group::MainMode::GroupTile) {
                                        let forward = matches!(
                                            key.code,
                                            KeyCode::Char(']') | KeyCode::PageDown
                                        );
                                        let groups = rail_view_groups(&rail_rows, rs, &panes, &states, &squad_store);
                                        if let Some(sel_group) = rs.selected_group(&groups) {
                                            // AC3-FR: page over the LIVE members so
                                            // `]`/`[` step by the same survivor pages
                                            // the reflowed tile layout renders, never
                                            // landing the selection on an exited slot.
                                            let live_group = live_group_of(sel_group, &states);
                                            rs.page_jump(&live_group, forward, main_capacity(tty));
                                        }
                                        if let Some(sel) = rs.selected_agent_idx {
                                            comp.set_focus(sel);
                                        }
                                        apply_group_tile_resize(rs, tty, &rail_rows, &states, &squad_store,
                                            &mut panes, &mut watch_sinks).await;
                                        prev_frame = None; // page slice changed -> full-paint
                                        hint = None;
                                        dirty = true;
                                    }
                                    rail_consumed = true;
                                }
                                // nav: `a` toggles the attention filter - the rail
                                // lists only agents waiting for input (idle + exited
                                // hidden). The visible member set changes, so re-anchor
                                // selection onto a still-visible agent (or surface an
                                // empty-state hint when nothing is waiting) and
                                // full-repaint, mirroring the `g` regroup discipline.
                                (KeyCode::Char('a')) => {
                                    rs.toggle_attention_filter();
                                    let groups = rail_view_groups(&rail_rows, rs, &panes, &states, &squad_store);
                                    match rs.re_anchor(&groups) {
                                        Some(sel) => {
                                            comp.set_focus(sel);
                                            match rs.main_mode {
                                                group::MainMode::GroupTile => {
                                                    apply_group_tile_resize(rs, tty, &rail_rows, &states, &squad_store,
                                                        &mut panes, &mut watch_sinks).await;
                                                }
                                                group::MainMode::Single => {
                                                    resize_rail_focus(tty, sel, &mut panes,
                                                        &mut watch_sinks).await;
                                                }
                                            }
                                            hint = None;
                                        }
                                        None => {
                                            // Filter on with nothing waiting (or an empty
                                            // fleet): name why the rail is blank instead of
                                            // leaving the operator staring at nothing.
                                            hint = rs.attention_filter.then(|| {
                                                "no agents waiting for input (a to show all)".to_string()
                                            });
                                        }
                                    }
                                    prev_frame = None; // member set changed -> full-paint
                                    dirty = true;
                                    rail_consumed = true;
                                }
                                // All other keys are consumed while browsing - the
                                // rail-nav sub-mode owns the keyboard until Esc/Enter.
                                _ => {}
                                } // end match code
                            } // end if let Some(code) = nav_key
                        }
                        // Rail entry from the tiled grid is a leader command (leader+t);
                        // bare `t` now types to the focused agent. Handled in the tiled
                        // leader dispatch below.
                        } // end if rail_fits

                        if !rail_consumed {
                        // ── Leader-key model (tiled path only; x-b563 Phase 1) ──
                        // The rail owns its own input (RailNav/PaneDrive); the
                        // leader routes input ONLY when the tiled grid is the
                        // surface (no rail, or a rail too narrow to render). The
                        // leader is additive here: a bare key still flows through
                        // the existing key_to_input dispatch below.
                        let tiled_input = rail_state.is_none() || !rail_fits;
                        if tiled_input {
                            let (next, decision) = leader::step(leader, &key, &leader_cfg);
                            leader = next;
                            match decision {
                                LeaderDecision::EnterPending => {
                                    let lk = leader_cfg.format_compact();
                                    hint = Some(format!(
                                        "LEADER ({lk}) - next: \u{21b9} focus \u{b7} ] [ page \u{b7} Enter drive \u{b7} Space scrollback \u{b7} ? help \u{b7} q quit \u{b7} ({lk} again sends it)"
                                    ));
                                    dirty = true;
                                    continue;
                                }
                                LeaderDecision::SendPrefix => {
                                    // Double-tap: send the literal leader byte to the
                                    // focused agent (no key is permanently stolen).
                                    // Eaten by the per-pane gate if not driving.
                                    hint = None;
                                    let act = comp.step(
                                        InputEvent::Keystroke(leader::leader_bytes(&leader_cfg)),
                                        &states,
                                    );
                                    if handle_action(act, &mut drive_sink, &names, home).await {
                                        break;
                                    }
                                    dirty = true;
                                    continue;
                                }
                                LeaderDecision::Command(cmdkey) => {
                                    hint = None;
                                    let ctrl_cmd = cmdkey.modifiers.contains(KeyModifiers::CONTROL);
                                    if !ctrl_cmd && matches!(cmdkey.code, KeyCode::Char('?')) {
                                        // leader + ? toggles the help overlay. Owned-PTY:
                                        // no drive to release first - focus is always live.
                                        help_open = !help_open;
                                        prev_frame = None; // overlay toggled -> full-paint
                                        dirty = true;
                                        continue;
                                    }
                                    if !ctrl_cmd && matches!(cmdkey.code, KeyCode::Char('t')) {
                                        // leader + t shows the rail (bare `t` now types to
                                        // the focused agent). Mirrors the former bare-`t`
                                        // entry; only meaningful when the rail fits.
                                        if rail_fits && rail_state.is_none() {
                                            let mut rs = RailState::new(GroupKey::Cwd);
                                            let groups =
                                                base_groups(&rail_rows, rs.group_key, &squad_store);
                                            if let Some(sel) = rs.re_anchor(&groups) {
                                                comp.set_focus(sel);
                                                resize_rail_focus(tty, sel, &mut panes, &mut watch_sinks)
                                                    .await;
                                            }
                                            rail_state = Some(rs);
                                            prev_frame = None; // region map changed -> full-paint
                                            hint = None;
                                        } else {
                                            hint = Some("rail does not fit this terminal".to_string());
                                        }
                                        dirty = true;
                                        continue;
                                    }
                                    if !ctrl_cmd && matches!(cmdkey.code, KeyCode::Char('n')) {
                                        // leader + n opens the goal launcher (spawn a worker);
                                        // bare `n` now types to the focused agent.
                                        launcher = Some(Launcher::new());
                                        hint = Some("goal> ".to_string());
                                        dirty = true;
                                        continue;
                                    }
                                    // The leader unlocks the mux command set; an unbound
                                    // key is reported and NOT forwarded to the agent.
                                    match leader_command(cmdkey) {
                                        Some(InputEvent::Quit) => {
                                            if handle_action(CompositorAction::Quit, &mut drive_sink, &names, home).await {
                                                break;
                                            }
                                        }
                                        Some(InputEvent::EnterScrollback) => {
                                            // Enter scrollback on the focused pane, unless it
                                            // has no overflowed history to freeze onto.
                                            let no_history = panes
                                                .get(comp.focus())
                                                .map(|p| p.history_size())
                                                .unwrap_or(0)
                                                == 0;
                                            if no_history {
                                                hint = Some("no scrollback history".to_string());
                                            } else if let CompositorAction::Scroll { pane_idx, cmd } =
                                                comp.step(InputEvent::EnterScrollback, &states)
                                            {
                                                // Entry scrolls up one line to freeze the viewport.
                                                if let Some(p) = panes.get_mut(pane_idx) {
                                                    p.apply_scroll(cmd);
                                                }
                                            }
                                            dirty = true;
                                        }
                                        Some(
                                            input @ (InputEvent::FocusNext
                                            | InputEvent::FocusPrev
                                            | InputEvent::PageNext
                                            | InputEvent::PagePrev),
                                        ) => {
                                            // Owned-PTY: focus IS drive, so moving focus or
                                            // page just relocates the live cursor - no claim
                                            // to release on the old pane or acquire on the new.
                                            let act = comp.step(input, &states);
                                            if handle_action(act, &mut drive_sink, &names, home).await {
                                                break;
                                            }
                                            dirty = true;
                                        }
                                        _ => {
                                            hint = Some(format!(
                                                "unknown leader command ({} then: \u{21b9} focus \u{b7} ] [ page \u{b7} Space scrollback \u{b7} ? help \u{b7} q quit)",
                                                leader_cfg.format_compact()
                                            ));
                                            dirty = true;
                                        }
                                    }
                                    continue;
                                }
                                LeaderDecision::Forward => {
                                    // Bare key: fall through to the existing dispatch.
                                }
                            }
                        }
                        // Owned-PTY dispatch: in `Drive` every bare key forwards to
                        // the focused agent's PTY; in `Scrollback` the pane is frozen
                        // and the keymap pages its history instead.
                        let input = match comp.mode() {
                            Mode::Scrollback => scrollback_key(key),
                            Mode::Drive => key_to_input(key),
                        };
                        if let Some(input) = input {
                            // Any operator key clears a prior transient hint.
                            hint = None;
                            let focus = comp.focus();
                            // Roster --bg card (x-57eb): a keystroke on a focused bg
                            // card never reaches an fno PTY (there is none). Enter
                            // shells out to native `claude attach`; any other key
                            // hints. Non-keystroke inputs (focus / page / scrollback /
                            // quit) fall through to the normal dispatch unchanged. A bg
                            // card is also host_interactive=false, so this branch must
                            // precede `exec_refuses` below.
                            let bg_keystroke = matches!(input, InputEvent::Keystroke(_))
                                && roster_cards.get(focus).map(Option::is_some).unwrap_or(false);
                            // Exec panes refuse keystrokes (host_interactive marker):
                            // surface a hint rather than leak bytes the daemon's
                            // watch-only connection would drop anyway (ab-7fd7ae49).
                            let exec_refuses = matches!(input, InputEvent::Keystroke(_))
                                && !host_interactive.get(focus).copied().unwrap_or(true);
                            if bg_keystroke {
                                let is_enter = matches!(
                                    &input,
                                    InputEvent::Keystroke(b) if b.as_slice() == b"\r"
                                );
                                if is_enter {
                                    if let Some(card) =
                                        roster_cards.get(focus).and_then(Option::as_ref)
                                    {
                                        // Attach takes over the terminal; on return the
                                        // screen is dirty, so force a full repaint.
                                        if let Err(e) = attach_bg_session(&card.session_id).await {
                                            hint = Some(e);
                                        }
                                        prev_frame = None;
                                    }
                                } else {
                                    let who =
                                        names.get(focus).map(String::as_str).unwrap_or("?");
                                    hint = Some(format!(
                                        "{who} is a claude --bg session - press Enter to attach"
                                    ));
                                }
                                dirty = true;
                            } else if exec_refuses {
                                let who = names.get(focus).map(String::as_str).unwrap_or("?");
                                hint = Some(format!(
                                    "{who} is an exec agent - watch only (host_mode=interactive drives)"
                                ));
                                dirty = true;
                            } else {
                                match comp.step(input, &states) {
                                    // Scroll mutates the focused pane's terminal;
                                    // apply it here where `panes` is mutable
                                    // (handle_action does not borrow it).
                                    CompositorAction::Scroll { pane_idx, cmd } => {
                                        if let Some(p) = panes.get_mut(pane_idx) {
                                            p.apply_scroll(cmd);
                                        }
                                    }
                                    other => {
                                        if handle_action(other, &mut drive_sink, &names, home).await {
                                            break; // Quit
                                        }
                                    }
                                }
                                dirty = true;
                            }
                        }
                        } // end !rail_consumed
                    }
                    Some(Ok(Event::Mouse(m))) => {
                        // ── Mouse-native input (E5a, x-2264) ──
                        // v1 scope: the default tiled grid. Left-click focuses the
                        // pane under the cursor; clicking the already-focused pane
                        // drives it (click to focus, click again to drive). The
                        // wheel scrolls that pane's scrollback. Rail mode and DRIVE
                        // keep their keyboard semantics here; mouse there (drive
                        // passthrough, drag-to-split, rail-mode mouse) is tracked as
                        // follow-up carveouts.
                        // Compute the page layout lazily, only inside the arms
                        // that hit-test (left-click, WATCH wheel). EnableMouseCapture
                        // turns on all-motion tracking, so a Moved event fires on
                        // every cursor move; those must NOT allocate a tile Vec
                        // (codex efficiency note).
                        //
                        // Gate on whether the TILED grid is actually rendered, not on
                        // rail_state alone: a --rail session too narrow for the rail
                        // falls back to tiles, and the key path treats input as tiled
                        // (rail_fits=false). Mirror that here so mouse works in the
                        // fallback too (codex P2).
                        let tiled_grid = rail_state.is_none()
                            || layout::compute_with_rail(tty, layout::RAIL_COLS, 1).is_err();
                        if tiled_grid {
                            match m.kind {
                                MouseEventKind::Down(MouseButton::Left)
                                    if comp.mode() == Mode::Drive =>
                                {
                                    // Owned-PTY: a click focuses the pane under the cursor,
                                    // and focus IS drive - there is no separate
                                    // click-again-to-drive step. Re-clicking the focused
                                    // pane is a harmless no-op.
                                    let hit = layout::compute_page(
                                        tty,
                                        panes.len(),
                                        comp.current_page(),
                                    )
                                    .ok()
                                    .and_then(|p| p.pane_at(m.column, m.row));
                                    if let Some(idx) = hit {
                                        hint = None;
                                        comp.set_focus(idx);
                                        dirty = true;
                                    }
                                }
                                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                                    // In WATCH the wheel targets the pane under the
                                    // cursor: re-focus it, or ignore the wheel entirely
                                    // when it falls on the footer / inter-tile gutter
                                    // (no pane) — scrolling dead space scrolls nothing
                                    // (gemini review). In SCROLLBACK the operator is
                                    // pinned to the entry pane (Locked Decision 5), so
                                    // the cursor position is not consulted.
                                    let proceed = if comp.mode() == Mode::Drive {
                                        let hit = layout::compute_page(
                                            tty,
                                            panes.len(),
                                            comp.current_page(),
                                        )
                                        .ok()
                                        .and_then(|p| p.pane_at(m.column, m.row));
                                        match hit {
                                            Some(idx) => {
                                                // A wheel that moves focus must
                                                // repaint the focus border even if the
                                                // scroll itself is a no-op (e.g.
                                                // wheel-down in WATCH); otherwise the
                                                // border desyncs from where keyboard
                                                // input lands (codex P2).
                                                if comp.focus() != idx {
                                                    comp.set_focus(idx);
                                                    dirty = true;
                                                }
                                                true
                                            }
                                            None => false,
                                        }
                                    } else {
                                        true
                                    };
                                    let has_history = panes
                                        .get(comp.focus())
                                        .map(|p| p.history_size())
                                        .unwrap_or(0)
                                        > 0;
                                    if proceed {
                                    match mouse_to_input(m.kind, comp.mode(), has_history) {
                                        Some(input) => {
                                            let action = comp.step(input, &states);
                                            apply_scroll_action(action, &mut panes);
                                            hint = None;
                                            dirty = true;
                                        }
                                        None => {
                                            // Wheel-up on a pane with no history: same
                                            // hint as the keyboard Space path. Wheel-down
                                            // in WATCH is already at the live tail → a
                                            // silent no-op.
                                            if matches!(m.kind, MouseEventKind::ScrollUp)
                                                && comp.mode() == Mode::Drive
                                            {
                                                hint =
                                                    Some("no scrollback history".to_string());
                                                dirty = true;
                                            }
                                        }
                                    }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some(Ok(Event::Resize(cols, rows))) => {
                        // Single-threaded select serializes resize vs page-flip
                        // events, so this recompute + clamp runs exactly once
                        // per SIGWINCH - no double-recompute race (AC4-FR). A
                        // resize below one min pane leaves capacity() Err; the
                        // panes keep their sizes and paint() renders the "too
                        // small" message (AC4-ERR), never a corrupt paint.
                        tty = TtySize::new(rows, cols);
                        if let Ok(cap) = layout::capacity(tty) {
                            comp.recompute_pagination(cap); // clamp + anchor (AC1-FR/AC4-HP)
                            if let Ok(paged) =
                                layout::compute_page(tty, panes.len(), comp.current_page())
                            {
                                resize_all_panes(
                                    &paged,
                                    &mut panes,
                                    &mut watch_sinks,
                                )
                                .await;
                            }
                            // In rail mode the tiled resize above sizes off-screen
                            // panes (warm for a future `t`-off); override the
                            // rail-visible panes to their rail sizes so they stay
                            // correct on resize. Must dispatch on main_mode: Single
                            // sizes the focused pane to the full main area (gemini
                            // HIGH); GroupTile sizes the selected group's tiles, else
                            // a SIGWINCH after Tab would full-size the selected pane
                            // while the renderer tiles the group (codex P2, PR #399).
                            if let Some(rs) = rail_state.as_ref() {
                                match rs.main_mode {
                                    group::MainMode::GroupTile => {
                                        apply_group_tile_resize(
                                            rs,
                                            tty,
                                            &rail_rows,
                                            &states,
                                            &squad_store,
                                            &mut panes,
                                            &mut watch_sinks,
                                        )
                                        .await;
                                    }
                                    group::MainMode::Single => {
                                        resize_rail_focus(
                                            tty,
                                            comp.focus(),
                                            &mut panes,
                                            &mut watch_sinks,
                                        )
                                        .await;
                                    }
                                }
                            }
                        }
                        dirty = true;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            maybe_msg = rx.recv() => {
                match maybe_msg {
                    Some(PaneMsg::Bytes(idx, b)) => {
                        if idx >= states.len() {
                            continue;
                        }
                        let action = states[idx].step(ConnEvent::BytesReceived(b));
                        if let ConnAction::FeedRenderer(bytes) = action {
                            panes[idx].feed(&bytes);
                        }
                        dirty = true;
                    }
                    Some(PaneMsg::Exited(idx, code)) => {
                        if idx >= states.len() {
                            continue;
                        }
                        states[idx].step(ConnEvent::AgentExited { code });
                        // Quit fires only if pane_count hit zero. In the eager
                        // model panes are retained as placeholders so this never
                        // happens today, but honor the signal defensively.
                        if matches!(comp.observe_pane_states(&states), CompositorAction::Quit) {
                            break;
                        }
                        // Owned-PTY: no drive claim to release. Drop the lazy
                        // interactive connection if it pointed at this pane (its
                        // daemon-side slot is gone with the agent). Focus re-anchors
                        // via the rail's existing re_anchor; a dead pane renders
                        // "exited" and eats keystrokes (no axis to revert).
                        if drive_sink.as_ref().map(|(i, _)| *i) == Some(idx) {
                            drive_sink = None;
                        }
                        // codex P2: this exit just shrank the tiled survivor set
                        // (AC3-FR reflow). The next paint redraws the survivors at
                        // their new LARGER tile size, but their PTYs keep the old
                        // (smaller) winsize until a nav key fires a resize - leaving
                        // full-screen / line-wrapping programs sized to the pre-exit
                        // tiles. Resize the survivors here so the winsize matches the
                        // reflowed layout on the same frame the exit lands. A no-op
                        // outside GroupTile (Single fills the area regardless) and
                        // when no group resolves.
                        if let Some(rs) = rail_state.as_ref() {
                            if matches!(rs.main_mode, group::MainMode::GroupTile) {
                                apply_group_tile_resize(rs, tty, &rail_rows, &states, &squad_store,
                                    &mut panes, &mut watch_sinks).await;
                                prev_frame = None; // tile sizes changed -> full-paint
                            }
                        }
                        dirty = true;
                    }
                    Some(PaneMsg::Closed(idx, reason)) => {
                        if idx >= states.len() {
                            continue;
                        }
                        states[idx].step(ConnEvent::WsClosed { reason });
                        if matches!(comp.observe_pane_states(&states), CompositorAction::Quit) {
                            break;
                        }
                        // Owned-PTY: the watch sink dropped; the pane renders
                        // "disconnected - r to retry". Drop the interactive drive
                        // connection too if it pointed here.
                        if drive_sink.as_ref().map(|(i, _)| *i) == Some(idx) {
                            drive_sink = None;
                        }
                        dirty = true;
                    }
                    Some(PaneMsg::Discovered(result)) => {
                        // x-87f7 apply arm: the detached task finished; do the
                        // cheap, in-memory tiling here on the single loop task. No
                        // file/network I/O runs here (Domain Pitfall) - the only
                        // awaits are the LOCAL resize ioctls inside live_add_*.
                        // Clear the guard FIRST so even an early break below can't
                        // wedge discovery (AC3-FR).
                        discover_in_flight = false;

                        // Cache write-back against the LIVE cache (not the task's
                        // snapshot): a name that newly tiled is cleared, a name that
                        // newly failed is inserted with the apply-time `now`. Same
                        // logic the old inline path ran (x-c226), now here.
                        let now = std::time::Instant::now();
                        let survived: std::collections::HashSet<&str> =
                            result.survived.iter().map(String::as_str).collect();
                        for name in &result.probed {
                            if survived.contains(name.as_str()) {
                                failed_probe_cache.remove(name);
                            } else {
                                failed_probe_cache.insert(name.clone(), now);
                            }
                        }
                        // Sweep expired failures so the cache stays bounded and a
                        // departed session self-heals (x-c226 AC1-FR).
                        failed_probe_cache.retain(|_, failed_at| {
                            now.saturating_duration_since(*failed_at) < FAILED_PROBE_TTL
                        });

                        // The soft cap counts only real fno panes (the `None` slots
                        // in roster_cards); roster cards open no socket and are
                        // exempt (codex PR #98 P2).
                        let cap = max_panes();
                        let pane_count = |roster_cards: &[Option<RosterBgCard>]| -> usize {
                            roster_cards.iter().filter(|c| c.is_none()).count()
                        };
                        let mut added = false;

                        // Re-filter survivors against the CURRENT names (AC2-FR): a
                        // survivor may have tiled since the snapshot. focus=false:
                        // a poll-discovered pane never steals focus (AC2-UI).
                        for i in fresh_survivors(&result.panes, &names) {
                            if pane_count(&roster_cards) >= cap {
                                break; // honor the cap (AC2-EDGE)
                            }
                            let (name, host_mode, rail_row) = &result.panes[i];
                            added |= live_add_pane(
                                home,
                                name,
                                *host_mode,
                                &tx,
                                tty,
                                &mut comp,
                                &mut names,
                                &mut panes,
                                &mut states,
                                &mut watch_sinks,
                                &mut host_interactive,
                                &mut roster_cards,
                                &mut rail_rows,
                                &mut repo_cache,
                                rail_row.clone(),
                                false,
                            )
                            .await;
                        }

                        // Roster cards: dedup against the live roster_cards by
                        // session_id (AC3-EDGE). Cards open no socket, so they are
                        // NOT capped (matching the startup roster merge).
                        let new_cards = new_roster_cards(result.cards, &roster_cards);
                        for card in new_cards {
                            added |= live_add_roster_card(
                                card,
                                tty,
                                &mut comp,
                                &mut names,
                                &mut panes,
                                &mut states,
                                &mut watch_sinks,
                                &mut host_interactive,
                                &mut roster_cards,
                                &mut rail_rows,
                                &mut repo_cache,
                            )
                            .await;
                        }

                        // A live-add sized the new pane(s) for the tiled grid; in
                        // rail mode reapply the rail-region sizing so the visible
                        // pane(s) don't render shrunk (codex PR #98 P2).
                        if added {
                            reapply_rail_sizing(
                                rail_state.as_ref(),
                                tty,
                                &rail_rows,
                                &states,
                                &squad_store,
                                comp.focus(),
                                &mut panes,
                                &mut watch_sinks,
                            )
                            .await;
                            dirty = true;
                        }
                    }
                    None => break,
                }
            }
            _ = tick.tick() => {
                // Paint at most once per frame, and only if something
                // changed. No change -> no write -> no buffer pressure.
                if dirty && panes.is_empty() {
                    // E5b front door: no panes, render the goal launcher.
                    paint_front_door(
                        &mut stderr,
                        &mut prev_frame,
                        tty,
                        launcher.as_ref().map(|l| l.buffer.as_str()).unwrap_or(""),
                        hint.as_deref(),
                    );
                    dirty = false;
                } else if dirty {
                    let rail_arg = rail_state.as_ref().map(|rs| {
                        let (groups, badges) = rail_groups_and_badges(&rail_rows, rs, &panes, &states, &squad_store);
                        (rs, groups, badges)
                    });
                    match rail_arg {
                        Some((rs, groups, badges)) => paint(
                            &mut stderr,
                            &mut prev_frame,
                            tty,
                            &names,
                            &panes,
                            &states,
                            &comp,
                            &host_interactive,
                            hint.as_deref(),
                            cap_note,
                            Some((rs, &groups, &badges, &rail_rows)),
                            help_open,
                            &palette,
                        ),
                        None => paint(
                            &mut stderr,
                            &mut prev_frame,
                            tty,
                            &names,
                            &panes,
                            &states,
                            &comp,
                            &host_interactive,
                            hint.as_deref(),
                            cap_note,
                            None,
                            help_open,
                            &palette,
                        ),
                    };
                    dirty = false;
                }
            }
            _ = ping.tick() => {
                // x-45e6: live-discover externally-spawned children (orchestrator
                // panes via `fno agents spawn --cwd`, fresh `claude --bg` sessions)
                // on the existing tick - tiles within one cadence, no restart.
                // x-87f7: the discovery I/O (registry read + worker.ping probe +
                // roster load) runs on a DETACHED task so this arm never `.await`s
                // it - no discovery latency can starve the key-event arm. The task
                // ALWAYS sends exactly one PaneMsg::Discovered (even empty/error),
                // applied + guard-cleared on the rx.recv() arm below.
                if parsed.all && !discover_in_flight {
                    discover_in_flight = true;
                    // Owned snapshots: tokio::spawn needs 'static + Send. `home`
                    // is a cheap PathBuf wrapper (Locked Decision 3); the probe's
                    // join_all borrow stays inside discover_children.
                    let home_owned = home.clone();
                    let names_snapshot = names.clone();
                    let cache_snapshot = failed_probe_cache.clone();
                    let tx_disc = tx.clone();
                    tokio::spawn(async move {
                        // Single send after the await, no early return between, so
                        // AC3-FR holds on every path (success / error / empty).
                        let result = discover_children(home_owned, names_snapshot, cache_snapshot).await;
                        let _ = tx_disc.send(PaneMsg::Discovered(result)).await;
                    });
                }
                ping_open_sinks(&mut watch_sinks, &mut states).await;
                // E5b: with the front door up (no panes) observe_pane_states
                // would report Quit on its 0-pane guard. Don't tear down the
                // grid while the operator is at the launcher.
                if !panes.is_empty()
                    && matches!(comp.observe_pane_states(&states), CompositorAction::Quit)
                {
                    break;
                }
                dirty = true;
            }
        }
    }

    // Teardown: detach the interactive drive connection (releases its daemon
    // slot) and every pane's watch sink so the daemon drops the grid cleanly.
    // The terminal (raw mode + alternate screen + cursor) is restored by
    // `TerminalGuard`'s Drop when `_guard` falls out of scope.
    if let Some((_, sink)) = drive_sink.take() {
        close_driver_sink(sink, "grid_quit").await;
    }
    for sink in watch_sinks.into_iter().flatten() {
        close_driver_sink(sink, "grid_quit").await;
    }
    0
}

/// RAII terminal guard: enables raw mode + enters the alternate screen +
/// hides the cursor on `enter`, and restores all three on `Drop` (every exit
/// path, including panic unwind). Uses crossterm throughout so its event
/// subsystem is initialized before `EventStream` is polled.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        // EnableMouseCapture turns on SGR mouse reporting so the EventStream
        // yields Event::Mouse (click-to-focus/drive, scroll). Restored on Drop.
        crossterm::execute!(
            io::stderr(),
            terminal::EnterAlternateScreen,
            EnableMouseCapture,
            cursor::Hide
        )?;
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            io::stderr(),
            DisableMouseCapture,
            terminal::LeaveAlternateScreen,
            cursor::Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

/// Apply a compositor `Scroll` action to the owning pane. The mouse wheel and
/// keyboard scrollback both produce `Scroll`; non-scroll actions are a no-op
/// (the wheel only ever yields `Scroll`). Separate from `handle_action`
/// because scrolling needs `&mut panes` while `handle_action` borrows them
/// immutably.
fn apply_scroll_action(action: CompositorAction, panes: &mut [Pane]) {
    debug_assert!(
        matches!(
            action,
            CompositorAction::Scroll { .. } | CompositorAction::NoOp
        ),
        "apply_scroll_action received a non-scroll variant: {action:?}"
    );
    if let CompositorAction::Scroll { pane_idx, cmd } = action {
        if let Some(p) = panes.get_mut(pane_idx) {
            p.apply_scroll(cmd);
        }
    }
}

/// Map a mouse-wheel event to the compositor `InputEvent` that scrolls the
/// focused pane's history, or `None` when the wheel should do nothing: wheel-up
/// on a pane with no history (the caller surfaces the "no scrollback" hint), or
/// wheel-down in WATCH (already at the live tail). The focus side effects stay
/// in the run loop; this mirrors `key_to_input` for the wheel so the
/// mode-transition is unit-testable. (E5a mouse-native.)
fn mouse_to_input(kind: MouseEventKind, mode: Mode, has_history: bool) -> Option<InputEvent> {
    match (kind, mode) {
        // First wheel-up notch (while driving) enters scrollback, which itself
        // scrolls up one line — only when the pane has history to freeze onto.
        (MouseEventKind::ScrollUp, Mode::Drive) if has_history => Some(InputEvent::EnterScrollback),
        (MouseEventKind::ScrollUp, Mode::Scrollback) => Some(InputEvent::ScrollLineUp),
        (MouseEventKind::ScrollDown, Mode::Scrollback) => Some(InputEvent::ScrollLineDown),
        _ => None,
    }
}

/// Apply a compositor action that may require I/O. Returns `true` when the
/// loop should quit. Owned-PTY model (x-1356): the only I/O action is
/// forwarding keystrokes to the focused pane's single owned-PTY sink - there
/// is no promote/release, so no second "interactive" connection to open.
/// The single interactive drive connection, keyed to the pane it controls.
/// At most ONE is ever open: the daemon counts interactive ("controlling")
/// connections against `DEFAULT_MAX_CONCURRENT_DRIVES` and they are per-agent
/// exclusive, so the grid renders every pane over cheap "watch" connections
/// and holds just one interactive slot for the pane being driven.
type DriveSink = Option<(usize, WsSink)>;

/// Point the one interactive drive connection at `pane_idx`, opening it lazily
/// and closing any previous one first (release the old slot before claiming
/// the new). Returns the sink, or `None` if the daemon refused (agent driven
/// elsewhere, cap full) - the caller drops the keystrokes.
async fn ensure_drive_sink<'a>(
    drive_sink: &'a mut DriveSink,
    pane_idx: usize,
    name: &str,
    home: &AgentsHome,
) -> Option<&'a mut WsSink> {
    if drive_sink.as_ref().map(|(i, _)| *i) != Some(pane_idx) {
        if let Some((_, old)) = drive_sink.take() {
            close_driver_sink(old, "refocus").await;
        }
        match open_drive_ws(home, name, "interactive").await {
            Ok(ws) => {
                let (sink, mut source) = ws.split();
                // Drain + discard the interactive stream's echo; the pane
                // renders from its own "watch" connection.
                tokio::spawn(async move { while source.next().await.is_some() {} });
                *drive_sink = Some((pane_idx, sink));
            }
            Err(_) => return None,
        }
    }
    drive_sink.as_mut().map(|(_, s)| s)
}

/// Apply a compositor action that may require I/O. Returns `true` when the loop
/// should quit. Owned-PTY model (x-1356): the only I/O action is forwarding
/// keystrokes, which lazily opens the single interactive drive connection for
/// the focused pane (the dispatch has already refused exec panes).
async fn handle_action(
    action: CompositorAction,
    drive_sink: &mut DriveSink,
    names: &[String],
    home: &AgentsHome,
) -> bool {
    match action {
        CompositorAction::Quit => return true,
        CompositorAction::NoOp => {}
        // Scroll is applied inline by the run loop (it needs `&mut panes`); it
        // never reaches here. Handle it for exhaustiveness.
        CompositorAction::Scroll { .. } => {}
        CompositorAction::ForwardKeystrokes { pane_idx, bytes } => {
            if let Some(name) = names.get(pane_idx) {
                if let Some(sink) = ensure_drive_sink(drive_sink, pane_idx, name, home).await {
                    let _ = sink.send(Message::Binary(bytes.into())).await;
                }
            }
        }
    }
    false
}

// ── ? help overlay (E5c AC-3) ─────────────────────────────────────────────────

/// Decision the run loop takes for a key event while the help overlay may be open.
/// Pure + testable; the run loop dispatches on this rather than inlining the match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpAction {
    /// Close the overlay (e.g. Esc / `q` while open).
    Close,
    /// Swallow this key; the overlay is open and this key navigates nothing.
    Inert,
    /// Let the caller (run loop) handle the key via normal key_to_input logic.
    Passthrough,
}

/// Pure decision fn for the `?`-overlay key events. The run loop calls this
/// BEFORE the normal key dispatch so an OPEN overlay can intercept keys.
///
/// Owned-PTY (x-1356): the overlay OPENS via `leader + ?` (handled in the leader
/// dispatch), never a bare `?` - a bare `?` types to the focused agent like any
/// other key. So this fn only intercepts keys while the overlay is already
/// open: `Esc`/`q` close it, anything else is inert (it does not leak to the
/// agent behind the overlay). Closed -> everything passes through. Ctrl-C always
/// passes through to the outer Quit handler.
fn help_key_action(key: KeyEvent, help_open: bool) -> HelpAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl {
        return HelpAction::Passthrough;
    }
    match (help_open, key.code) {
        (true, KeyCode::Char('q') | KeyCode::Esc) => HelpAction::Close,
        (true, _) => HelpAction::Inert,
        (false, _) => HelpAction::Passthrough,
    }
}

/// Build the help text lines for the `?` overlay.
/// Pure - no I/O, no state, easy to unit-test.
/// `rail_on`: when true, adds the rail-mode bindings (g/a/Tab).
fn help_overlay_lines(rail_on: bool) -> Vec<String> {
    let mut lines = vec![
        " Keybindings ".to_string(),
        String::new(),
        "  leader + key   mux command while driving (tiled grid)".to_string(),
        "  leader twice   send the literal leader key to the agent".to_string(),
        String::new(),
        "  Tab / arrows   focus next/prev pane".to_string(),
        "  Enter          drive focused pane".to_string(),
        "  Esc            release drive -> WATCH".to_string(),
        "  ] / [          next / previous page".to_string(),
        "  Space          enter scrollback".to_string(),
        "  q              quit".to_string(),
        "  ?              close this help".to_string(),
    ];
    if rail_on {
        lines.push(String::new());
        lines.push("  Rail (when active):".to_string());
        lines.push("  g              cycle group-by (cwd .. union)".to_string());
        lines.push("  m              recruit selected agent (Up/Down picks a squad)".to_string());
        lines
            .push("  r              remove selected agent from its squad (squad view)".to_string());
        lines.push("  a              toggle attention filter".to_string());
        lines.push("  Tab            tile / zoom selected group".to_string());
    }
    lines
}

/// Draw the help overlay as a centered bordered box OVER `frame`.
/// Uses the same box-drawing glyphs as `raster_border` (light border, not heavy).
/// Content is truncated to fit the box interior. Called only when help_open.
fn raster_help_overlay(frame: &mut ScreenBuffer, lines: &[String]) {
    // Compute box dimensions: pad by 1 on each side, width = longest line + 2 border cols.
    let content_width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    // Box is content + 2 (left/right border). Clamp to frame width - 2.
    let box_cols = ((content_width + 2) as u16).min(frame.cols.saturating_sub(2));
    // Height = lines count + 2 (top/bottom border). Clamp to frame height - 2.
    let box_rows = ((lines.len() + 2) as u16).min(frame.rows.saturating_sub(2));

    // Center the box.
    let start_row = frame.rows.saturating_sub(box_rows) / 2;
    let start_col = frame.cols.saturating_sub(box_cols) / 2;

    let inner_cols = box_cols.saturating_sub(2) as usize;

    // Top border: ┌───┐
    let top: String = std::iter::once('┌')
        .chain(std::iter::repeat_n(
            '─',
            box_cols.saturating_sub(2) as usize,
        ))
        .chain(std::iter::once('┐'))
        .collect();
    frame.put_str(start_row, start_col, &top, false);

    // Content rows.
    for (i, line) in lines.iter().enumerate() {
        let r = start_row + 1 + i as u16;
        if r >= start_row + box_rows.saturating_sub(1) {
            break;
        }
        let padded = format!("{:<width$}", line, width = inner_cols);
        let truncated = truncate(&padded, inner_cols);
        frame.put_str(r, start_col, "│", false);
        frame.put_str(r, start_col + 1, &truncated, false);
        frame.put_str(r, start_col + box_cols.saturating_sub(1), "│", false);
    }

    // Bottom border: └───┘
    let bottom: String = std::iter::once('└')
        .chain(std::iter::repeat_n(
            '─',
            box_cols.saturating_sub(2) as usize,
        ))
        .chain(std::iter::once('┘'))
        .collect();
    frame.put_str(
        start_row + box_rows.saturating_sub(1),
        start_col,
        &bottom,
        false,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn resolve_explicit_names_passthrough() {
        let parsed = GridArgs {
            names: argv(&["a", "b"]),
            all: false,
            rail: false,
            group_by: None,
        };
        let home = AgentsHome::from_env();
        assert_eq!(resolve_agent_names(&parsed, &home).unwrap(), vec!["a", "b"]);
    }

    // ── claude --bg roster cards (x-57eb) ────────────────────────────────────

    fn roster_worker(session_id: &str, cwd: &str) -> crate::claude_roster::RosterWorker {
        crate::claude_roster::RosterWorker {
            session_id: session_id.to_string(),
            pid: None,
            proc_start: None,
            pty_sock: None,
            pty_auth: None,
            cli_version: None,
            cwd: cwd.to_string(),
            worktree_path: None,
        }
    }

    // AC-HP (Change 1): a roster session absent from the registry becomes a card.
    #[test]
    fn roster_only_worker_becomes_a_card() {
        let registry = vec![json!({"name": "wkA", "provider": "codex", "short_id": "aaaa1111"})];
        let w = roster_worker("bbbb2222-3333-4444-5555-666677778888", "/Users/x/code/proj");
        let roster = vec![&w];
        let cards = roster_bg_cards_from(&registry, &roster);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].name, "bbbb2222");
        assert_eq!(cards[0].session_id, "bbbb2222-3333-4444-5555-666677778888");
        assert_eq!(cards[0].cwd, "/Users/x/code/proj");
    }

    // x-45e6 AC2-EDGE: the registry poll diff never re-adds an already-tiled
    // name; only genuinely new children survive.
    #[test]
    fn new_registry_names_skips_already_tiled() {
        let known = vec!["wkA".to_string(), "wkB".to_string()];
        let fresh = vec![
            "wkA".to_string(), // already tiled -> dropped
            "wkB".to_string(), // already tiled -> dropped
            "wkC".to_string(), // new -> kept
        ];
        assert_eq!(new_registry_names(fresh, &known), vec!["wkC".to_string()]);
        // No new rows when the registry is unchanged.
        assert!(new_registry_names(known.clone(), &known).is_empty());
    }

    // x-c226: the negative-probe cache filter. A failed lane is skipped within
    // TTL (AC1-EDGE / AC1-ERR) and re-probed at/after the boundary (AC3-EDGE).
    #[test]
    fn cache_candidates_skips_within_ttl_reprobes_after() {
        use std::collections::HashMap;
        use std::time::Instant;
        let ttl = Duration::from_secs(60);
        let now = Instant::now();
        let mut cache: HashMap<String, Instant> = HashMap::new();
        cache.insert("fresh_fail".into(), now); // just failed -> within TTL
        cache.insert(
            "boundary".into(),
            now.checked_sub(ttl).expect("instant in range"),
        ); // exactly TTL old -> eligible (>=)
        cache.insert(
            "expired".into(),
            now.checked_sub(ttl + Duration::from_secs(1))
                .expect("instant in range"),
        ); // past TTL -> eligible

        let new = vec![
            "unseen".to_string(),     // not cached -> probe
            "fresh_fail".to_string(), // within TTL -> skip
            "boundary".to_string(),   // == TTL -> re-probe
            "expired".to_string(),    // > TTL -> re-probe
        ];
        let out = cache_candidates(new, &cache, now, ttl);
        assert_eq!(
            out,
            vec![
                "unseen".to_string(),
                "boundary".to_string(),
                "expired".to_string()
            ]
        );

        // AC1-EDGE: every new name is within TTL -> empty candidate set.
        let all_cached = vec!["fresh_fail".to_string()];
        assert!(cache_candidates(all_cached, &cache, now, ttl).is_empty());
    }

    // x-87f7 AC2-FR: the apply arm re-filters the task's survivors against the
    // CURRENT names, so a survivor that tiled between snapshot and apply is not
    // double-added; genuinely-new survivors keep their slot. The returned indices
    // are into the result's `panes`, preserving order.
    #[test]
    fn fresh_survivors_skips_already_tiled() {
        let panes = vec![
            ("wkA".to_string(), false, json!({ "name": "wkA" })), // already tiled -> dropped
            ("wkB".to_string(), true, json!({ "name": "wkB" })),  // new -> kept (idx 1)
            ("wkC".to_string(), false, json!({ "name": "wkC" })), // new -> kept (idx 2)
        ];
        let names = vec!["wkA".to_string()];
        assert_eq!(fresh_survivors(&panes, &names), vec![1, 2]);

        // All survivors already present -> nothing to tile.
        let all_present = vec!["wkA".to_string(), "wkB".to_string(), "wkC".to_string()];
        assert!(fresh_survivors(&panes, &all_present).is_empty());
    }

    // x-87f7 AC1-EDGE: an empty DiscoveryResult (no survivors) tiles nothing; the
    // helper is total over the empty case (the in-flight guard still clears on the
    // apply arm, verified by reading).
    #[test]
    fn fresh_survivors_empty_result() {
        let panes: Vec<(String, bool, Value)> = Vec::new();
        let names = vec!["wkA".to_string()];
        assert!(fresh_survivors(&panes, &names).is_empty());
    }

    // ── recruit picker + cycle (x-0175) ──────────────────────────────────────

    fn squad_store_named(names: &[&str]) -> squads::SquadStore {
        squads::SquadStore {
            squads: names
                .iter()
                .map(|n| squads::Squad {
                    name: n.to_string(),
                    members: vec![],
                    created_at: String::new(),
                })
                .collect(),
        }
    }

    // AC2-HP / Discretion #2: the cursor seeds onto last_squad's index.
    #[test]
    fn recruit_candidates_seeds_cursor_on_last() {
        let store = squad_store_named(&["stack", "crew", "ops"]);
        let (cands, cursor) = recruit_candidates(&store, Some("crew"));
        assert_eq!(cands, vec!["stack", "crew", "ops"]);
        assert_eq!(cursor, 1);
    }

    // AC2-FR: a last_squad that no longer exists (or None) -> cursor 0, no crash.
    #[test]
    fn recruit_candidates_unknown_last_is_zero() {
        let store = squad_store_named(&["stack", "crew"]);
        assert_eq!(recruit_candidates(&store, Some("ghost")).1, 0);
        assert_eq!(recruit_candidates(&store, None).1, 0);
    }

    // AC1-EDGE: no squads -> empty candidates, cursor 0.
    #[test]
    fn recruit_candidates_empty_store() {
        let store = squad_store_named(&[]);
        let (cands, cursor) = recruit_candidates(&store, Some("stack"));
        assert!(cands.is_empty());
        assert_eq!(cursor, 0);
    }

    // AC1-UI: from a shown candidate, Down/Up step and wrap (Discretion #4).
    #[test]
    fn cycle_advances_and_wraps_from_candidate() {
        assert_eq!(cycle_cursor(3, 2, true, true), 0); // Down wraps high->0
        assert_eq!(cycle_cursor(3, 0, true, false), 2); // Up wraps 0->high
        assert_eq!(cycle_cursor(3, 0, true, true), 1);
        assert_eq!(cycle_cursor(3, 2, true, false), 1);
    }

    // AC1-UI: a diverged buffer (empty or typed) snaps onto the cursor candidate
    // without advancing, so the first press always shows feedback.
    #[test]
    fn cycle_snaps_when_buffer_diverged() {
        assert_eq!(cycle_cursor(2, 0, false, true), 0); // fresh: first Down -> [0]
        assert_eq!(cycle_cursor(2, 0, false, false), 0); // first Up -> [0]
        assert_eq!(cycle_cursor(2, 5, false, true), 1); // stale cursor clamps in-range
    }

    // AC1-EDGE: zero candidates -> cursor unchanged (inert cycle).
    #[test]
    fn cycle_inert_when_empty() {
        assert_eq!(cycle_cursor(0, 0, false, true), 0);
        assert_eq!(cycle_cursor(0, 0, true, false), 0);
    }

    // AC1-UI: the legend lists every squad and marks the cursor one with
    // guillemets, so the operator sees the choices and which is current.
    #[test]
    fn candidate_legend_marks_cursor() {
        let cands = vec!["stack".to_string(), "crew".to_string(), "ops".to_string()];
        assert_eq!(
            candidate_legend(&cands, 1),
            "stack \u{2039}crew\u{203a} ops"
        );
        assert_eq!(
            candidate_legend(&cands, 0),
            "\u{2039}stack\u{203a} crew ops"
        );
    }

    // AC1-EDGE: no squads -> empty legend, so the footer is the bare prompt.
    #[test]
    fn candidate_legend_empty_when_no_squads() {
        assert_eq!(candidate_legend(&[], 0), "");
    }

    // x-45e6 AC3-EDGE: the roster poll diff is by session_id, and skips slots
    // that are real fno panes (None) as well as already-carried cards.
    #[test]
    fn new_roster_cards_diffs_by_session_id() {
        let card = |sid: &str| RosterBgCard {
            name: sid[..4].to_string(),
            session_id: sid.to_string(),
            cwd: "/c".to_string(),
            proc_start: None,
        };
        // Grid currently carries one real pane (None) and one bg card.
        let known = vec![None, Some(card("aaaa1111-0000-0000-0000-000000000000"))];
        let fresh = vec![
            card("aaaa1111-0000-0000-0000-000000000000"), // already carried -> dropped
            card("bbbb2222-0000-0000-0000-000000000000"), // new -> kept
        ];
        let got = new_roster_cards(fresh, &known);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].session_id, "bbbb2222-0000-0000-0000-000000000000");
    }

    // AC-EDGE (dedup): a session in BOTH registry and roster appears once
    // (registry wins). Dedups by short_id AND by full session_id.
    #[test]
    fn roster_card_dedups_against_registry_by_short_and_full_id() {
        let w_short = roster_worker("aaaa1111-0000-0000-0000-000000000000", "/a");
        let w_full = roster_worker("ffff9999-1111-2222-3333-444455556666", "/b");
        let w_new = roster_worker("cccc3333-7777-8888-9999-aaaabbbbcccc", "/c");
        // Registry carries one as short_id, one as full session_id.
        let registry = vec![
            json!({"name": "wk1", "short_id": "aaaa1111"}),
            json!({"name": "wk2", "session_id": "ffff9999-1111-2222-3333-444455556666"}),
        ];
        let roster = vec![&w_short, &w_full, &w_new];
        let cards = roster_bg_cards_from(&registry, &roster);
        // Only the truly-new session survives; both overlaps are deduped out.
        assert_eq!(
            cards.len(),
            1,
            "registry rows win on overlap (one row per session)"
        );
        assert_eq!(cards[0].name, "cccc3333");
    }

    // AC-ERR: an empty roster yields zero cards (the absent-roster degrade path
    // resolves to an empty worker list, never an error).
    #[test]
    fn empty_roster_yields_zero_cards() {
        let registry = vec![json!({"name": "wkA", "short_id": "aaaa1111"})];
        let roster: Vec<&crate::claude_roster::RosterWorker> = Vec::new();
        assert!(roster_bg_cards_from(&registry, &roster).is_empty());
    }

    #[test]
    fn bg_age_label_buckets_and_degrades() {
        assert_eq!(bg_age_label(Some(1_000), 1_000 + 120), "2m");
        assert_eq!(bg_age_label(Some(1_000), 1_000 + 7_200), "2h");
        assert_eq!(bg_age_label(Some(1_000), 1_000 + 172_800), "2d");
        // None (the date-string drift) and a future stamp both degrade to "?".
        assert_eq!(bg_age_label(None, 5_000), "?");
        assert_eq!(bg_age_label(Some(9_999), 1_000), "?");
    }

    // AC-HP (Change 2): a --bg card shows cwd basename + age + the attach hint.
    #[test]
    fn bg_card_pane_renders_cwd_age_and_attach_hint() {
        let card = RosterBgCard {
            name: "bbbb2222".to_string(),
            session_id: "bbbb2222-3333-4444-5555-666677778888".to_string(),
            cwd: "/Users/x/code/myproj".to_string(),
            proc_start: Some(1_000),
        };
        let pane = bg_card_pane(&card, 12, 40, 1_000 + 3_600);
        let snap = pane.snapshot();
        let mut text = String::new();
        for r in 0..snap.rows {
            for c in 0..snap.cols {
                if let Some(cell) = snap.cell(r, c) {
                    text.push_str(&cell.text);
                }
            }
            text.push('\n');
        }
        assert!(text.contains("myproj"), "card shows cwd basename: {text:?}");
        assert!(text.contains("1h"), "card shows age: {text:?}");
        assert!(text.contains("attach"), "card shows attach hint: {text:?}");
        // No PTY stream is opened: the pane is fed a one-shot card only.
    }

    #[test]
    fn filter_pty_agents_keeps_interactive_claude_drops_exec_and_dead() {
        let rows = vec![
            json!({"name": "wkA", "provider": "codex", "status": "idle"}),
            // Interactive claude (E2): tileable, sub-billed PTY -> KEPT.
            json!({"name": "wkClaudeInt", "provider": "claude", "status": "live", "host_mode": "interactive"}),
            // Exec/stream claude (absent host_mode == exec): headless lane -> dropped.
            json!({"name": "wkClaudeExec", "provider": "claude", "status": "live"}),
            json!({"name": "wkG", "provider": "gemini", "status": "busy"}),
            json!({"name": "wkDead", "provider": "codex", "status": "exited"}),
            json!({"name": "wkNoStatus", "provider": "gemini"}),
        ];
        let got = filter_pty_agents(&rows);
        // codex+idle, interactive claude, gemini+busy, gemini+(default live) kept;
        // exec claude + exited dropped.
        assert_eq!(got, vec!["wkA", "wkClaudeInt", "wkG", "wkNoStatus"]);
    }

    #[test]
    fn survives_pty_gate_probes_only_claude() {
        // codex/gemini always survive, regardless of the (unused) ping flag.
        assert!(survives_pty_gate(Some("codex"), false));
        assert!(survives_pty_gate(Some("gemini"), false));
        // An unknown/absent provider is never claude -> survives.
        assert!(survives_pty_gate(None, false));
        // claude survives iff its worker answered worker.ping (PTY lane);
        // the stream lane (ping fails) is dropped.
        assert!(survives_pty_gate(Some("claude"), true));
        assert!(!survives_pty_gate(Some("claude"), false));
    }

    #[test]
    fn ctrl_c_forwards_as_byte_and_is_leader_quit() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        // Owned-PTY: a bare Ctrl-C forwards 0x03 to the agent (so you can
        // interrupt it); quitting the grid is a leader command.
        assert_eq!(key_to_input(key), Some(InputEvent::Keystroke(vec![0x03])));
        assert_eq!(leader_command(key), Some(InputEvent::Quit));
    }

    // ── Launcher (E5b) ──────────────────────────────────────────────────

    #[test]
    fn launcher_key_maps_actions() {
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(launcher_key(k(KeyCode::Esc)), LauncherAction::Cancel);
        assert_eq!(launcher_key(k(KeyCode::Enter)), LauncherAction::Submit);
        assert_eq!(
            launcher_key(k(KeyCode::Backspace)),
            LauncherAction::Backspace
        );
        assert_eq!(
            launcher_key(k(KeyCode::Char('a'))),
            LauncherAction::Append('a')
        );
        // Ctrl-C cancels the launcher (escape hatch); arrows are inert.
        assert_eq!(
            launcher_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            LauncherAction::Cancel
        );
        assert_eq!(launcher_key(k(KeyCode::Up)), LauncherAction::Ignore);
    }

    #[test]
    fn launcher_accumulates_and_edits() {
        let mut l = Launcher::new();
        assert_eq!(l.apply(LauncherAction::Append('h')), LauncherOutcome::Stay);
        l.apply(LauncherAction::Append('i'));
        assert_eq!(l.buffer, "hi");
        assert_eq!(l.apply(LauncherAction::Backspace), LauncherOutcome::Stay);
        assert_eq!(l.buffer, "h");
        // Backspace on the way to empty never panics.
        l.apply(LauncherAction::Backspace);
        l.apply(LauncherAction::Backspace);
        assert_eq!(l.buffer, "");
    }

    #[test]
    fn launcher_submit_trims_and_blank_is_noop() {
        // AC1-EDGE: a whitespace-only goal does not spawn.
        let mut l = Launcher::new();
        l.apply(LauncherAction::Append(' '));
        l.apply(LauncherAction::Append(' '));
        assert_eq!(l.apply(LauncherAction::Submit), LauncherOutcome::Stay);
        // A real goal submits, trimmed.
        let mut l = Launcher::new();
        for c in " add auth ".chars() {
            l.apply(LauncherAction::Append(c));
        }
        assert_eq!(
            l.apply(LauncherAction::Submit),
            LauncherOutcome::Submitted("add auth".to_string())
        );
    }

    #[test]
    fn launcher_cancel_closes() {
        let mut l = Launcher::new();
        l.apply(LauncherAction::Append('x'));
        assert_eq!(l.apply(LauncherAction::Cancel), LauncherOutcome::Cancelled);
    }

    #[test]
    fn classify_launch_routes_sigil_to_interactive_pane() {
        // Plain goal -> autonomous /target worker (the E5b default).
        assert_eq!(
            classify_launch("add auth"),
            LaunchKind::TargetWorker("add auth".to_string())
        );
        // `>` opens an interactive pane; trailing text is the first message.
        assert_eq!(
            classify_launch("> fix the bug"),
            LaunchKind::InteractivePane("fix the bug".to_string())
        );
        // Bare `>` opens an empty pane (no first message).
        assert_eq!(
            classify_launch(">"),
            LaunchKind::InteractivePane(String::new())
        );
        // `>` with no space still strips the sigil.
        assert_eq!(
            classify_launch(">hello"),
            LaunchKind::InteractivePane("hello".to_string())
        );
    }

    #[test]
    fn interactive_pane_name_is_distinct_from_target() {
        assert_eq!(interactive_pane_name(3), "claude-3");
        assert!(!interactive_pane_name(3).starts_with("target-"));
    }

    #[test]
    fn target_worker_name_slugs_goal() {
        assert_eq!(
            target_worker_name("Add user auth!", 1),
            "target-add-user-auth-1"
        );
        // All-punctuation / blank goal falls back to a stable stem.
        assert_eq!(target_worker_name("   ", 2), "target-goal-2");
        // Long goals are capped and never leave a trailing dash.
        let n = target_worker_name("a very long goal string that keeps going forever", 3);
        assert!(n.starts_with("target-a-very-long-goal"), "got {n}");
        assert!(n.ends_with("-3"));
        assert!(!n.contains("--"));
    }

    #[test]
    fn front_door_frame_renders_prompt_and_goal() {
        let tty = TtySize::new(24, 80);
        let frame = build_front_door_frame(tty, "add auth", None);
        let all: String = (0..tty.rows)
            .map(|r| row_text(&frame, r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("footnote grid"), "title present: {all:?}");
        assert!(all.contains("Enter a goal"), "prompt present");
        assert!(
            all.contains("claude pane"),
            "interactive-pane gesture advertised"
        );
        assert!(all.contains("goal> add auth"), "live goal buffer present");
    }

    #[test]
    fn front_door_frame_renders_hint_status_line() {
        // A failed submit on the empty front door must show feedback, not a
        // blank prompt (sigma-review finding).
        let tty = TtySize::new(24, 80);
        let frame = build_front_door_frame(tty, "", Some("launch failed: daemon down"));
        let all: String = (0..tty.rows)
            .map(|r| row_text(&frame, r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("launch failed: daemon down"),
            "hint shown: {all:?}"
        );
    }

    #[test]
    fn front_door_paint_clears_and_writes_goal() {
        let tty = TtySize::new(24, 80);
        let mut prev: Option<ScreenBuffer> = None;
        let mut buf = Vec::new();
        paint_front_door(&mut buf, &mut prev, tty, "do it", None);
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains(CLEAR_ALL), "first front-door frame clears");
        assert!(s.contains("goal> do it"), "renders the goal buffer");
        assert!(prev.is_some(), "records the screen buffer for diffing");
    }

    /// The leader unlocks the mux command set (the former WATCH keymap).
    #[test]
    fn leader_command_mux_mapping() {
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(leader_command(k(KeyCode::Tab)), Some(InputEvent::FocusNext));
        assert_eq!(
            leader_command(k(KeyCode::BackTab)),
            Some(InputEvent::FocusPrev)
        );
        assert_eq!(
            leader_command(k(KeyCode::Right)),
            Some(InputEvent::FocusNext)
        );
        assert_eq!(
            leader_command(k(KeyCode::Left)),
            Some(InputEvent::FocusPrev)
        );
        assert_eq!(
            leader_command(k(KeyCode::Char('q'))),
            Some(InputEvent::Quit)
        );
        // Scrollback entry moved to leader+Space (bare Space types now).
        assert_eq!(
            leader_command(k(KeyCode::Char(' '))),
            Some(InputEvent::EnterScrollback)
        );
        // An unbound leader key is reported, not forwarded.
        assert_eq!(leader_command(k(KeyCode::Char('x'))), None);
    }

    #[test]
    fn leader_page_keys_map_to_page_events() {
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(
            leader_command(k(KeyCode::Char(']'))),
            Some(InputEvent::PageNext)
        );
        assert_eq!(
            leader_command(k(KeyCode::Char('['))),
            Some(InputEvent::PagePrev)
        );
        assert_eq!(
            leader_command(k(KeyCode::PageDown)),
            Some(InputEvent::PageNext)
        );
        assert_eq!(
            leader_command(k(KeyCode::PageUp)),
            Some(InputEvent::PagePrev)
        );
    }

    /// Owned-PTY: every bare key forwards to the focused agent as bytes -
    /// page keys, letters, Enter, Esc, arrows, control bytes.
    #[test]
    fn bare_keys_forward_to_agent() {
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(
            key_to_input(k(KeyCode::Char(']'))),
            Some(InputEvent::Keystroke(b"]".to_vec()))
        );
        assert_eq!(
            key_to_input(k(KeyCode::PageDown)),
            Some(InputEvent::Keystroke(b"\x1b[6~".to_vec()))
        );
        assert_eq!(
            key_to_input(k(KeyCode::Esc)),
            Some(InputEvent::Keystroke(b"\x1b".to_vec()))
        );
        assert_eq!(
            key_to_input(k(KeyCode::Char('a'))),
            Some(InputEvent::Keystroke(b"a".to_vec()))
        );
        assert_eq!(
            key_to_input(k(KeyCode::Enter)),
            Some(InputEvent::Keystroke(b"\r".to_vec()))
        );
        assert_eq!(
            key_to_input(k(KeyCode::Up)),
            Some(InputEvent::Keystroke(b"\x1b[A".to_vec()))
        );
    }

    #[test]
    fn ctrl_letter_maps_to_control_byte() {
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        // Ctrl-A = 0x01 forwarded to the agent.
        assert_eq!(key_to_input(key), Some(InputEvent::Keystroke(vec![0x01])));
    }

    /// Scrollback keymap: the frozen-pane nav keys.
    #[test]
    fn scrollback_keymap() {
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(
            scrollback_key(k(KeyCode::Up)),
            Some(InputEvent::ScrollLineUp)
        );
        assert_eq!(
            scrollback_key(k(KeyCode::Char('j'))),
            Some(InputEvent::ScrollLineDown)
        );
        assert_eq!(
            scrollback_key(k(KeyCode::Esc)),
            Some(InputEvent::ExitScrollback)
        );
        assert_eq!(scrollback_key(k(KeyCode::Char('z'))), None);
    }

    #[test]
    fn truncate_adds_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("x", 0), "");
        assert_eq!(truncate("abc", 1), "…");
    }

    #[test]
    fn style_diff_distinguishes_underline_inverse_and_italic() {
        let mut buf = Vec::new();
        let mut last = None;

        let mut cell = RenderCell {
            text: "u".to_string(),
            underline: true,
            ..RenderCell::default()
        };
        apply_cell_style(&mut buf, &cell, &mut last).unwrap();
        let underline_len = buf.len();

        cell.underline = false;
        cell.inverse = true;
        apply_cell_style(&mut buf, &cell, &mut last).unwrap();
        let inverse_len = buf.len();

        cell.inverse = false;
        cell.italic = true;
        apply_cell_style(&mut buf, &cell, &mut last).unwrap();
        let italic_len = buf.len();

        assert!(
            inverse_len > underline_len,
            "inverse must emit a distinct style transition after underline"
        );
        assert!(
            italic_len > inverse_len,
            "italic must emit a distinct style transition after inverse"
        );
    }

    #[test]
    fn render_to_paints_border_title_and_footer() {
        // 1 pane, 24x80 terminal. Render to a buffer and assert structural
        // markers are present (border glyphs, title, footer mode string).
        let paged = layout::compute_page(TtySize::new(24, 80), 1, 0).unwrap();
        let names = vec!["wkA".to_string()];
        let mut pane = Pane::new(paged.tiles[0].rows - 2, paged.tiles[0].cols - 2);
        pane.feed(b"hello from agent");
        let snaps = vec![pane.snapshot()];
        let states = vec![ConnState::Live];
        let comp = Compositor::new(1);
        let mut buf: Vec<u8> = Vec::new();
        render_to(
            &mut buf,
            &paged,
            &names,
            &snaps,
            &states,
            &comp,
            &[true],
            None,
            &[],
            None,
        )
        .unwrap();
        let s = String::from_utf8_lossy(&buf);
        // Focused (only) pane uses heavy border corners.
        assert!(s.contains('┏'), "heavy top-left border for focused pane");
        assert!(s.contains("wkA"), "title shows agent name");
        assert!(
            s.contains("leader"),
            "footer shows the owned-PTY leader hint"
        );
        assert!(s.contains("hello from agent"), "pane content painted");
        // AC1-UI: single-page renders no pagination chrome.
        assert!(
            !s.contains("Page "),
            "single page shows no Page n/P indicator"
        );
    }

    /// AC1-HP + AC2-HP footer chrome: a multi-page grid shows `Page n/P` and,
    /// for an off-screen waiting agent, an attention badge. Page slots map to
    /// global pane indices via page_start.
    #[test]
    fn render_footer_shows_page_indicator_and_badges() {
        let t = TtySize::new(13, 40); // capacity 4
        let n = 9; // → 3 pages
        let paged = layout::compute_page(t, n, 0).unwrap();
        assert_eq!(paged.capacity, 4);
        assert_eq!(paged.page_count, 3);
        let names: Vec<String> = (0..n).map(|i| format!("w{i}")).collect();
        let vis_snaps: Vec<PaneSnapshot> = paged
            .tiles
            .iter()
            .map(|tile| Pane::new(tile.rows - 2, tile.cols - 2).snapshot())
            .collect();
        let states = vec![ConnState::Live; n];
        let mut comp = Compositor::new(n);
        comp.recompute_pagination(4);
        // Pane 5 lives on page 1 (off-screen from page 0) and is waiting.
        let mut waiting = vec![false; n];
        waiting[5] = true;
        let badges = off_screen_waiting_by_page(&waiting, paged.capacity, paged.current_page);
        let mut buf: Vec<u8> = Vec::new();
        render_to(
            &mut buf,
            &paged,
            &names,
            &vis_snaps,
            &states,
            &comp,
            &vec![false; n],
            None,
            &badges,
            None,
        )
        .unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("Page 1/3"),
            "footer shows current page, got: {s:?}"
        );
        assert!(
            s.contains("▸p2●1"),
            "footer shows attention badge for page 2, got: {s:?}"
        );
    }

    #[test]
    fn host_modes_from_rows_aligns_to_names() {
        let rows = vec![
            json!({"name": "wkExec", "provider": "codex", "host_mode": "exec"}),
            json!({"name": "wkInter", "provider": "codex", "host_mode": "interactive"}),
            json!({"name": "wkLegacy", "provider": "gemini"}), // no host_mode => exec
        ];
        // Order follows `names`, not the registry; an unknown name => false.
        let names = argv(&["wkInter", "wkExec", "wkLegacy", "wkMissing"]);
        assert_eq!(
            host_modes_from_rows(&rows, &names),
            vec![true, false, false, false]
        );
    }

    /// Owned-PTY: the tiled footer advertises "type into the focused pane" +
    /// the leader, and a transient hint overrides it for one frame.
    #[test]
    fn footer_shows_leader_hint_and_transient() {
        let paged = layout::compute_page(TtySize::new(24, 80), 1, 0).unwrap();
        let names = vec!["wkA".to_string()];
        let snaps = vec![Pane::new(paged.tiles[0].rows - 2, paged.tiles[0].cols - 2).snapshot()];
        let states = vec![ConnState::Live];
        let comp = Compositor::new(1);

        let mut buf = Vec::new();
        render_to(
            &mut buf,
            &paged,
            &names,
            &snaps,
            &states,
            &comp,
            &[true],
            None,
            &[],
            None,
        )
        .unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("focused pane") && s.contains("leader"),
            "owned-PTY footer, got: {s:?}"
        );

        // A transient hint replaces the mode line entirely.
        let mut buf = Vec::new();
        render_to(
            &mut buf,
            &paged,
            &names,
            &snaps,
            &states,
            &comp,
            &[false],
            Some("wkA is an exec agent - watch only"),
            &[],
            None,
        )
        .unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("wkA is an exec agent"),
            "hint shown in footer, got: {s:?}"
        );
    }

    /// A multi-page grid always shows `Page n/P` and off-screen attention
    /// badges in the footer (paging is a leader command, always available).
    #[test]
    fn render_footer_multipage_shows_page_and_badges() {
        let t = TtySize::new(13, 40); // capacity 4 → multi-page
        let n = 9;
        let paged = layout::compute_page(t, n, 0).unwrap();
        let names: Vec<String> = (0..n).map(|i| format!("w{i}")).collect();
        let vis_snaps: Vec<PaneSnapshot> = paged
            .tiles
            .iter()
            .map(|tile| Pane::new(tile.rows - 2, tile.cols - 2).snapshot())
            .collect();
        let states = vec![ConnState::Live; n];
        let mut comp = Compositor::new(n);
        comp.recompute_pagination(4);
        // Pane 5 lives off-screen (page 1) and is waiting.
        let mut waiting = vec![false; n];
        waiting[5] = true;
        let badges = off_screen_waiting_by_page(&waiting, paged.capacity, paged.current_page);
        let mut buf: Vec<u8> = Vec::new();
        render_to(
            &mut buf,
            &paged,
            &names,
            &vis_snaps,
            &states,
            &comp,
            &[],
            None,
            &badges,
            None,
        )
        .unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("Page "),
            "multi-page footer shows the page indicator"
        );
        assert!(
            s.contains("▸p"),
            "off-screen waiting agent badges in the footer"
        );
    }

    /// The soft-cap note rides the footer so the operator sees the fleet was
    /// truncated even on the alternate screen (Locked Decision 5).
    #[test]
    fn render_footer_shows_soft_cap_note() {
        let paged = layout::compute_page(TtySize::new(24, 80), 1, 0).unwrap();
        let names = vec!["wkA".to_string()];
        let snaps = vec![Pane::new(paged.tiles[0].rows - 2, paged.tiles[0].cols - 2).snapshot()];
        let states = vec![ConnState::Live];
        let comp = Compositor::new(1);
        let mut buf: Vec<u8> = Vec::new();
        render_to(
            &mut buf,
            &paged,
            &names,
            &snaps,
            &states,
            &comp,
            &[],
            None,
            &[],
            Some((32, 40)),
        )
        .unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("32/40 shown"),
            "footer shows cap note, got: {s:?}"
        );
    }

    // ── Frame-diff renderer (flicker fix) ────────────────────────────────

    /// The full-screen clear escape that, emitted every dirty tick, was the
    /// flicker source. The diff path must never emit it.
    const CLEAR_ALL: &str = "\x1b[2J";

    /// One pane fed `bytes`, sized to the 24x80 single-pane interior.
    fn one_pane(bytes: &[u8]) -> Vec<Pane> {
        // 24-row tty, 1-row footer => 23-row tile => 21x78 interior.
        let mut p = Pane::new(21, 78);
        p.feed(bytes);
        vec![p]
    }

    /// E5a wheel-scroll: `apply_scroll_action` scrolls the addressed pane on a
    /// `Scroll` action and is an inert no-op on anything else or an out-of-range
    /// index (the wheel only yields `Scroll`, but the helper must not panic).
    #[test]
    fn apply_scroll_action_scrolls_only_on_scroll() {
        use crate::grid::state::ScrollCmd;
        // Overflow the 21-row interior so the pane has scrollback history.
        let mut panes = one_pane(b"");
        for i in 0..30 {
            panes[0].feed(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(panes[0].scroll_offset(), 0, "starts at the live tail");

        apply_scroll_action(
            CompositorAction::Scroll {
                pane_idx: 0,
                cmd: ScrollCmd::LineUp,
            },
            &mut panes,
        );
        assert_eq!(panes[0].scroll_offset(), 1, "wheel-up scrolled one line");

        apply_scroll_action(CompositorAction::NoOp, &mut panes);
        assert_eq!(panes[0].scroll_offset(), 1, "NoOp left the pane untouched");

        // Out-of-range pane index is ignored, not a panic.
        apply_scroll_action(
            CompositorAction::Scroll {
                pane_idx: 99,
                cmd: ScrollCmd::LineUp,
            },
            &mut panes,
        );
        assert_eq!(panes[0].scroll_offset(), 1);
    }

    /// E5a wheel router: `mouse_to_input` maps wheel + mode + history to the
    /// scroll InputEvent, mirroring `key_to_input`. The focus side effects live
    /// in the run loop and are not exercised here.
    #[test]
    fn mouse_to_input_wheel_routing() {
        use MouseEventKind::{ScrollDown, ScrollUp};
        // WATCH + history: the first wheel-up notch enters scrollback.
        assert_eq!(
            mouse_to_input(ScrollUp, Mode::Drive, true),
            Some(InputEvent::EnterScrollback)
        );
        // WATCH + no history: nothing (the caller surfaces the hint instead).
        assert_eq!(mouse_to_input(ScrollUp, Mode::Drive, false), None);
        // SCROLLBACK: wheel up/down walk the history regardless of has_history.
        assert_eq!(
            mouse_to_input(ScrollUp, Mode::Scrollback, false),
            Some(InputEvent::ScrollLineUp)
        );
        assert_eq!(
            mouse_to_input(ScrollDown, Mode::Scrollback, true),
            Some(InputEvent::ScrollLineDown)
        );
        // Wheel-down in WATCH: already at the live tail → no-op.
        assert_eq!(mouse_to_input(ScrollDown, Mode::Drive, true), None);
        // Non-wheel events never route through here.
        assert_eq!(
            mouse_to_input(MouseEventKind::Moved, Mode::Drive, true),
            None
        );
    }

    /// emit_diff over two identical frames produces zero bytes - an idle grid
    /// writes nothing at all, so there is nothing to flicker.
    #[test]
    fn diff_of_identical_frames_emits_nothing() {
        let paged = layout::compute_page(TtySize::new(24, 80), 1, 0).unwrap();
        let names = vec!["wkA".to_string()];
        let mut snaps = vec![one_pane(b"hello").pop().unwrap().snapshot()];
        let states = vec![ConnState::Live];
        let comp = Compositor::new(1);
        let a = build_frame(
            &paged,
            &names,
            &mut snaps,
            &states,
            &comp,
            &[true],
            None,
            &[],
            None,
            &Palette::fixed(),
        );
        let b = a.clone();
        let mut buf = Vec::new();
        emit_diff(&a, &b, &mut buf).unwrap();
        assert!(
            buf.is_empty(),
            "identical frames emit no bytes, got {buf:?}"
        );
    }

    /// First paint clears + full-paints; a second paint of the identical state
    /// takes the diff path, emits no clear, and is dramatically smaller. This
    /// is the core flicker-elimination guarantee.
    #[test]
    fn paint_clears_first_frame_then_diffs_without_clear() {
        let tty = TtySize::new(24, 80);
        let names = vec!["wkA".to_string()];
        let states = vec![ConnState::Live];
        let comp = Compositor::new(1);
        let hi = vec![true];

        let mut prev: Option<ScreenBuffer> = None;
        let mut buf1 = Vec::new();
        paint(
            &mut buf1,
            &mut prev,
            tty,
            &names,
            &one_pane(b"hello"),
            &states,
            &comp,
            &hi,
            None,
            None,
            None,
            false,
            &Palette::fixed(),
        );
        let s1 = String::from_utf8_lossy(&buf1);
        assert!(s1.contains(CLEAR_ALL), "first frame clears + full paints");
        assert!(s1.contains("hello"), "first frame paints content");
        assert!(prev.is_some(), "first frame records the screen buffer");

        let mut buf2 = Vec::new();
        paint(
            &mut buf2,
            &mut prev,
            tty,
            &names,
            &one_pane(b"hello"),
            &states,
            &comp,
            &hi,
            None,
            None,
            None,
            false,
            &Palette::fixed(),
        );
        let s2 = String::from_utf8_lossy(&buf2);
        assert!(
            !s2.contains(CLEAR_ALL),
            "steady-state repaint must never clear (no flicker), got {s2:?}"
        );
        assert!(
            buf2.len() < buf1.len() / 4,
            "unchanged frame diffs to near-nothing: {} vs {} bytes",
            buf2.len(),
            buf1.len()
        );
    }

    /// When one cell changes, the diff repaints only that cell - the unchanged
    /// prefix is not re-emitted, and no clear is issued.
    #[test]
    fn paint_diff_repaints_only_the_changed_cell() {
        let tty = TtySize::new(24, 80);
        let names = vec!["wkA".to_string()];
        let states = vec![ConnState::Live];
        let comp = Compositor::new(1);
        let hi = vec![true];

        let mut prev: Option<ScreenBuffer> = None;
        let mut sink = Vec::new();
        paint(
            &mut sink,
            &mut prev,
            tty,
            &names,
            &one_pane(b"hello"),
            &states,
            &comp,
            &hi,
            None,
            None,
            None,
            false,
            &Palette::fixed(),
        );

        // "hello" -> "hellX": only the 5th interior cell changes.
        let mut buf = Vec::new();
        paint(
            &mut buf,
            &mut prev,
            tty,
            &names,
            &one_pane(b"hellX"),
            &states,
            &comp,
            &hi,
            None,
            None,
            None,
            false,
            &Palette::fixed(),
        );
        let s = String::from_utf8_lossy(&buf);
        assert!(!s.contains(CLEAR_ALL), "diff path issues no clear");
        assert!(s.contains('X'), "the changed glyph is painted, got {s:?}");
        assert!(
            !s.contains("hell"),
            "the unchanged 'hell' prefix is not repainted, got {s:?}"
        );
    }

    /// A resize changes the frame dimensions, so the next paint cannot diff
    /// against the old-size frame: it takes the full-paint path and re-clears.
    #[test]
    fn paint_resize_takes_full_paint_path() {
        let names = vec!["wkA".to_string()];
        let states = vec![ConnState::Live];
        let comp = Compositor::new(1);
        let hi = vec![true];

        let mut prev: Option<ScreenBuffer> = None;
        let mut b0 = Vec::new();
        paint(
            &mut b0,
            &mut prev,
            TtySize::new(24, 80),
            &names,
            &one_pane(b"hi"),
            &states,
            &comp,
            &hi,
            None,
            None,
            None,
            false,
            &Palette::fixed(),
        );
        // Same size again -> diff, no clear.
        let mut b1 = Vec::new();
        paint(
            &mut b1,
            &mut prev,
            TtySize::new(24, 80),
            &names,
            &one_pane(b"hi"),
            &states,
            &comp,
            &hi,
            None,
            None,
            None,
            false,
            &Palette::fixed(),
        );
        assert!(
            !String::from_utf8_lossy(&b1).contains(CLEAR_ALL),
            "same-size repaint diffs without clearing"
        );
        // Resize -> dimensions differ -> full paint with clear.
        let mut b2 = Vec::new();
        paint(
            &mut b2,
            &mut prev,
            TtySize::new(13, 40),
            &names,
            &one_pane(b"hi"),
            &states,
            &comp,
            &hi,
            None,
            None,
            None,
            false,
            &Palette::fixed(),
        );
        assert!(
            String::from_utf8_lossy(&b2).contains(CLEAR_ALL),
            "a resize re-clears + full-paints the new geometry"
        );
    }

    /// Regression (chatgpt-codex, PR #386): a wide-char spacer must emit
    /// nothing on the diff path. Row `abc` -> `你<spacer>c`: the run covers
    /// cols 0-1; printing `你` advances the terminal cursor two columns, and
    /// the spacer must NOT print a space (which would overwrite the unchanged
    /// `c` in col 2 that the diff never restores).
    #[test]
    fn diff_skips_wide_char_spacer_no_overwrite() {
        let mk = |text: &str, spacer: bool| RenderCell {
            text: text.to_string(),
            wide_spacer: spacer,
            ..RenderCell::default()
        };
        let prev = ScreenBuffer {
            rows: 1,
            cols: 3,
            cells: vec![mk("a", false), mk("b", false), mk("c", false)],
            chrome_fg: CellColor::Default,
            chrome_bg: CellColor::Default,
        };
        let cur = ScreenBuffer {
            rows: 1,
            cols: 3,
            cells: vec![mk("你", false), mk("", true), mk("c", false)],
            chrome_fg: CellColor::Default,
            chrome_bg: CellColor::Default,
        };
        let mut buf = Vec::new();
        emit_diff(&prev, &cur, &mut buf).unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains('你'), "wide glyph is painted, got {s:?}");
        assert!(
            !s.contains("你 "),
            "spacer must not print a space after the glyph, got {s:?}"
        );
        assert!(
            !s.contains('c'),
            "the unchanged trailing cell is neither overwritten nor repainted, got {s:?}"
        );
    }

    /// A blank cell (empty text, NOT a spacer) still renders as a space so the
    /// diff can erase stale content - the wide_spacer skip must not regress this.
    #[test]
    fn diff_blank_cell_still_erases() {
        let mk = |text: &str| RenderCell {
            text: text.to_string(),
            ..RenderCell::default()
        };
        let prev = ScreenBuffer {
            rows: 1,
            cols: 2,
            cells: vec![mk("x"), mk("y")],
            chrome_fg: CellColor::Default,
            chrome_bg: CellColor::Default,
        };
        // col0 cleared to a blank (default) cell; col1 unchanged.
        let cur = ScreenBuffer {
            rows: 1,
            cols: 2,
            cells: vec![RenderCell::default(), mk("y")],
            chrome_fg: CellColor::Default,
            chrome_bg: CellColor::Default,
        };
        let mut buf = Vec::new();
        emit_diff(&prev, &cur, &mut buf).unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains(' '),
            "a blank cell emits a space to erase, got {s:?}"
        );
    }

    // ── Scrollback affordance ────────────────────────────────────────────
    // (The scrollback keymap is unit-tested as `scrollback_keymap` and the
    // leader+Space entry as `leader_command_mux_mapping`.)

    /// AC1-HP / AC1-UI: in scrollback the footer shows the position indicator
    /// and exit affordance, and the focused pane's title carries a SCROLLBACK
    /// badge with the same offset.
    #[test]
    fn scrollback_footer_and_title_show_mode_and_offset() {
        use crate::grid::state::ScrollCmd;
        let paged = layout::compute_page(TtySize::new(24, 80), 1, 0).unwrap();
        let names = vec!["wkA".to_string()];
        let mut pane = Pane::new(paged.tiles[0].rows - 2, paged.tiles[0].cols - 2);
        // Feed more than one screen so there is history to scroll into.
        for i in 0..(paged.tiles[0].rows as usize + 5) {
            pane.feed(format!("line{i}\r\n").as_bytes());
        }
        pane.apply_scroll(ScrollCmd::LineUp);
        pane.apply_scroll(ScrollCmd::LineUp);
        let off = pane.scroll_offset();
        assert!(off >= 1, "pane scrolled up into history");
        let snaps = vec![pane.snapshot()];
        let states = vec![ConnState::Live];
        let mut comp = Compositor::new(1);
        comp.step(InputEvent::EnterScrollback, &states);
        assert_eq!(comp.mode(), Mode::Scrollback);

        let mut buf = Vec::new();
        render_to(
            &mut buf,
            &paged,
            &names,
            &snaps,
            &states,
            &comp,
            &[true],
            None,
            &[],
            None,
        )
        .unwrap();
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("SCROLLBACK"),
            "footer/title show scrollback, got: {s:?}"
        );
        assert!(
            s.contains(&format!("-{off}")),
            "shows the scroll offset -{off}, got: {s:?}"
        );
        assert!(s.contains("Esc live"), "footer shows the exit affordance");
    }

    // ── Rail renderer (raster_rail / raster_footer_rail) ──────────────────
    //
    // The rail key-handling state machine is exercised live (needs a daemon),
    // but the render half is pure over a ScreenBuffer. These lock the AC1-UI /
    // AC4-UI / AC5-HP / AC5-UI render contract the sigma-review panel flagged
    // as untested.

    /// Reconstruct a buffer row as a String (blank cells render as a space),
    /// so a test can assert on the rendered text.
    #[cfg(test)]
    fn row_text(frame: &ScreenBuffer, r: u16) -> String {
        (0..frame.cols)
            .filter_map(|c| frame.get(r, c))
            .map(|cell| {
                if cell.text.is_empty() {
                    " ".to_string()
                } else {
                    cell.text.clone()
                }
            })
            .collect()
    }

    // ── x-fef5: base_groups Union assembly (sidelines ++ squads) ──────────

    #[test]
    fn base_groups_union_assembles_sidelines_and_squads() {
        // HP: agent x is in repo sideline `footnote` AND squad `stack`. The
        // union contains both a `cwd:`-keyed sideline group and a `squad:`-keyed
        // squad group, each containing x - so the occurrence cursor can select
        // each independently.
        let rows = vec![
            json!({"name": "x", "cwd": "/code/footnote"}), // idx 0
            json!({"name": "y", "cwd": "/code/footnote"}), // idx 1
            json!({"name": "z", "cwd": "/code/other"}),    // idx 2
        ];
        let mut store = squads::SquadStore::default();
        store.create("stack", "t");
        store.recruit("stack", "x");
        store.recruit("stack", "z");

        let groups = base_groups(&rows, group::GroupKey::Union, &store);

        let sideline = groups
            .iter()
            .find(|g| g.key_value == "cwd:/code/footnote")
            .expect("namespaced footnote sideline present");
        assert_eq!(
            sideline.header, "footnote",
            "header is the basename, unprefixed"
        );
        assert!(sideline.members.contains(&0), "x under its repo sideline");

        let squad = groups
            .iter()
            .find(|g| g.key_value == "squad:stack")
            .expect("namespaced squad present");
        assert!(squad.members.contains(&0), "x under its squad too");
    }

    #[test]
    fn base_groups_union_namespaces_keys_against_collision() {
        // AC4-EDGE / Boundaries: a squad named the SAME string as a sideline's
        // key_value (the repo path) still yields a distinct key_value, so the
        // occurrence cursor can sit on exactly one. `cwd:` / `squad:` prefixes
        // keep the namespaces disjoint.
        let rows = vec![json!({"name": "x", "cwd": "/code/footnote"})];
        let mut store = squads::SquadStore::default();
        store.create("/code/footnote", "t"); // squad literally named the repo path
        store.recruit("/code/footnote", "x");

        let groups = base_groups(&rows, group::GroupKey::Union, &store);
        let keys: Vec<&str> = groups.iter().map(|g| g.key_value.as_str()).collect();
        let mut uniq = keys.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(
            keys.len(),
            uniq.len(),
            "union key_values must be unique: {keys:?}"
        );
        assert!(
            keys.contains(&"cwd:/code/footnote"),
            "sideline key namespaced"
        );
        assert!(
            keys.contains(&"squad:/code/footnote"),
            "squad key namespaced"
        );
    }

    #[test]
    fn base_groups_union_no_squads_degrades_to_sidelines() {
        // ERR: no squads configured -> the union is just the sidelines, no panic.
        let rows = vec![
            json!({"name": "x", "cwd": "/a"}),
            json!({"name": "y", "cwd": "/b"}),
        ];
        let store = squads::SquadStore::default();
        let groups = base_groups(&rows, group::GroupKey::Union, &store);
        assert_eq!(groups.len(), 2, "two sidelines, no squad half");
        assert!(
            groups.iter().all(|g| g.key_value.starts_with("cwd:")),
            "all groups are namespaced sidelines: {groups:?}"
        );
    }

    #[test]
    fn raster_rail_renders_count_badge_and_selection() {
        // /alpha = wkA + wkB (wkA exited -> x1 badge); /beta = wkC.
        let rows = vec![
            json!({"name": "wkA", "cwd": "/alpha", "provider": "codex", "status": "live"}),
            json!({"name": "wkB", "cwd": "/alpha", "provider": "codex", "status": "live"}),
            json!({"name": "wkC", "cwd": "/beta",  "provider": "gemini", "status": "live"}),
        ];
        let groups = group::group_by(&rows, group::GroupKey::Cwd);
        let waiting = vec![false, false, false];
        let exited = vec![true, false, false]; // wkA (idx 0) exited
        let badges = group::compute_badges_from_live(&groups, &waiting, &exited, &[], 0);
        let mut rs = group::RailState::new(group::GroupKey::Cwd);
        rs.selected_agent_idx = Some(1); // wkB selected

        let rail_rect = layout::TileRect {
            row: 0,
            col: 0,
            rows: 10,
            cols: 18,
        };
        let mut frame = ScreenBuffer::blank(10, 18);
        raster_rail(&mut frame, &rail_rect, &groups, &badges, &rs, &rows);

        let all: String = (0..10)
            .map(|r| row_text(&frame, r))
            .collect::<Vec<_>>()
            .join("\n");

        // AC5-HP: /alpha header shows the member count (2).
        assert!(all.contains("(2)"), "member count missing:\n{all}");
        // AC5-UI: the exited attention badge is distinct from the count.
        assert!(all.contains("x1"), "exited badge missing:\n{all}");
        // AC1-UI: the selected member carries the selection marker.
        assert!(
            all.contains("> wkB"),
            "selection marker on wkB missing:\n{all}"
        );
    }

    #[test]
    fn raster_rail_repo_rollup_header_basename_and_worktree_labels() {
        // US1/US2/AC1-UI: a repo's main checkout + a worktree (stamped with the
        // same `_repo_root`) render under ONE header (the repo basename), with
        // members labeled by their worktree (`main`, `e5c-layout`), never the
        // full path or the agent name.
        let rows = vec![
            json!({
                "name": "wkMain", "provider": "claude", "status": "live",
                "cwd": "/code/footnote/footnote",
                repo::REPO_ROOT_FIELD: "/code/footnote/footnote",
                repo::WORKTREE_FIELD: "main",
            }),
            json!({
                "name": "wkLeaf", "provider": "claude", "status": "live",
                "cwd": "/conductor/workspaces/footnote/e5c-layout",
                repo::REPO_ROOT_FIELD: "/code/footnote/footnote",
                repo::WORKTREE_FIELD: "e5c-layout",
            }),
        ];
        let groups = group::group_by(&rows, group::GroupKey::Cwd);
        let badges =
            group::compute_badges_from_live(&groups, &[false, false], &[false, false], &[], 0);
        let rs = group::RailState::new(group::GroupKey::Cwd);

        let rail_rect = layout::TileRect {
            row: 0,
            col: 0,
            rows: 6,
            cols: 24,
        };
        let mut frame = ScreenBuffer::blank(6, 24);
        raster_rail(&mut frame, &rail_rect, &groups, &badges, &rs, &rows);
        let all: String = (0..6)
            .map(|r| row_text(&frame, r))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(all.contains("footnote (2)"), "repo basename header:\n{all}");
        assert!(!all.contains("/code/footnote"), "full path leaked:\n{all}");
        assert!(all.contains("main"), "worktree label `main`:\n{all}");
        assert!(all.contains("e5c-layout"), "worktree label:\n{all}");
        assert!(
            !all.contains("wkMain") && !all.contains("wkLeaf"),
            "agent names must not show under repo rollup:\n{all}"
        );
    }

    #[test]
    fn raster_rail_disambiguates_shared_worktree_with_agent_name() {
        // codex P2: two agents in the SAME checkout share `_worktree = main`.
        // The rail must disambiguate (append the agent name) rather than render
        // two identical `main` bullets with an ambiguous selection marker.
        let rows = vec![
            json!({
                "name": "goalA", "provider": "claude", "status": "live",
                "cwd": "/r/footnote",
                repo::REPO_ROOT_FIELD: "/r/footnote", repo::WORKTREE_FIELD: "main",
            }),
            json!({
                "name": "goalB", "provider": "claude", "status": "live",
                "cwd": "/r/footnote",
                repo::REPO_ROOT_FIELD: "/r/footnote", repo::WORKTREE_FIELD: "main",
            }),
        ];
        let groups = group::group_by(&rows, group::GroupKey::Cwd);
        let badges =
            group::compute_badges_from_live(&groups, &[false, false], &[false, false], &[], 0);
        let rs = group::RailState::new(group::GroupKey::Cwd);
        let rail_rect = layout::TileRect {
            row: 0,
            col: 0,
            rows: 6,
            cols: 28,
        };
        let mut frame = ScreenBuffer::blank(6, 28);
        raster_rail(&mut frame, &rail_rect, &groups, &badges, &rs, &rows);
        let all: String = (0..6)
            .map(|r| row_text(&frame, r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("main (goalA)"), "A disambiguated:\n{all}");
        assert!(all.contains("main (goalB)"), "B disambiguated:\n{all}");
    }

    // codex P2: a fleet taller than the rail must scroll so the selected row
    // stays visible (it was previously clipped once row >= last_row).
    #[test]
    fn raster_rail_scrolls_to_keep_selection_visible() {
        let specs: Vec<(String, String, String, String)> = (0..8)
            .map(|i| (format!("wk{i}"), "/g".into(), "codex".into(), "live".into()))
            .collect();
        let rows: Vec<serde_json::Value> = specs
            .iter()
            .map(|(n, c, p, s)| json!({"name": n, "cwd": c, "provider": p, "status": s}))
            .collect();
        let groups = group::group_by(&rows, group::GroupKey::Cwd);
        let badges =
            group::compute_badges_from_live(&groups, &vec![false; 8], &vec![false; 8], &[], 0);
        let mut rs = group::RailState::new(group::GroupKey::Cwd);
        rs.selected_agent_idx = Some(7); // wk7, the last member

        // Rail rect only 4 rows tall - cannot show the header + all 8 members.
        let rail_rect = layout::TileRect {
            row: 0,
            col: 0,
            rows: 4,
            cols: 18,
        };
        let mut frame = ScreenBuffer::blank(4, 18);
        raster_rail(&mut frame, &rail_rect, &groups, &badges, &rs, &rows);
        let all: String = (0..4)
            .map(|r| row_text(&frame, r))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            all.contains("> wk7"),
            "selected row scrolled off-screen:\n{all}"
        );
        assert!(
            !all.contains("(8)"),
            "header should have scrolled out of view:\n{all}"
        );
        assert!(
            !all.contains("wk0"),
            "first member should have scrolled out:\n{all}"
        );
    }

    #[test]
    fn raster_footer_rail_surfaces_hint() {
        // codex P2: a transient hint must reach the rail footer; mode + group-by
        // stay visible alongside it.
        let footer = layout::TileRect {
            row: 0,
            col: 0,
            rows: 1,
            cols: 80,
        };
        let mut frame = ScreenBuffer::blank(1, 80);
        let rs = group::RailState::new(group::GroupKey::Cwd);
        raster_footer_rail(&mut frame, &footer, &rs, None, Some("wkA: drive denied"));
        let line = row_text(&frame, 0);
        assert!(
            line.contains("drive denied"),
            "hint missing from rail footer: {line}"
        );
        assert!(
            line.contains("group-by: cwd"),
            "group-by still shown with hint: {line}"
        );
    }

    #[test]
    fn raster_footer_rail_always_shows_axis_and_group_key() {
        let footer = layout::TileRect {
            row: 0,
            col: 0,
            rows: 1,
            cols: 80,
        };
        let mut frame = ScreenBuffer::blank(1, 80);
        let rs = group::RailState::new(group::GroupKey::Provider);
        raster_footer_rail(&mut frame, &footer, &rs, None, None);
        let line = row_text(&frame, 0);
        // AC4-UI: footer always names the active group-by key + axis.
        assert!(line.contains("DRIVE"), "axis label missing: {line}");
        assert!(
            line.contains("group-by: provider"),
            "active group-by key missing: {line}"
        );
    }

    // ── US3: GroupTile render + footer (ab-6aed6905) ──────────────────────

    #[test]
    fn rail_group_footer_shows_mode_axis_groupby_and_page() {
        let footer = layout::TileRect {
            row: 0,
            col: 0,
            rows: 1,
            cols: 120,
        };
        let mut frame = ScreenBuffer::blank(1, 120);
        let rs = group::RailState::new(group::GroupKey::Provider);
        // current_page 1 (0-indexed) of 3 -> "page 2/3" (AC3-ERR).
        raster_footer_rail_group(&mut frame, &footer, &rs, "codex", 1, 3, None, None);
        let line = row_text(&frame, 0);
        // Footer invariant: mode + focus axis + group-by key always present.
        assert!(line.contains("DRIVE"), "axis missing: {line}");
        assert!(line.contains("tile"), "mode token missing: {line}");
        assert!(
            line.contains("group-by: provider"),
            "group-by missing: {line}"
        );
        assert!(
            line.contains("group codex"),
            "tiled group name missing: {line}"
        );
        assert!(line.contains("page 2/3"), "page chrome missing: {line}");
    }

    #[test]
    fn rail_group_footer_single_page_omits_page_chrome() {
        let footer = layout::TileRect {
            row: 0,
            col: 0,
            rows: 1,
            cols: 120,
        };
        let mut frame = ScreenBuffer::blank(1, 120);
        let rs = group::RailState::new(group::GroupKey::Cwd);
        raster_footer_rail_group(&mut frame, &footer, &rs, "/repo/x", 0, 1, None, None);
        let line = row_text(&frame, 0);
        assert!(line.contains("group /repo/x"), "group name missing: {line}");
        // The keymap hint legitimately says "]/[ page"; assert the page CHROME
        // ("group <name> page p/k") is what's absent on a single page.
        assert!(
            !line.contains("/repo/x page"),
            "single-page must omit page chrome: {line}"
        );
    }

    #[test]
    fn rail_group_frame_tiles_members_and_accents_selection() {
        // Two members of one group tile side-by-side on a wide terminal (AC3-HP).
        let rows = vec![
            json!({"name": "wkA", "cwd": "/repo/x", "provider": "codex", "status": "live"}),
            json!({"name": "wkB", "cwd": "/repo/x", "provider": "codex", "status": "live"}),
        ];
        let groups = group::group_by(&rows, group::GroupKey::Cwd);
        let badges =
            group::compute_badges_from_live(&groups, &[false, false], &[false, false], &[], 0);
        let names = vec!["wkA".to_string(), "wkB".to_string()];
        let states = vec![ConnState::Live, ConnState::Live];

        let mut rs = group::RailState::new(group::GroupKey::Cwd);
        rs.main_mode = group::MainMode::GroupTile;
        rs.selected_agent_idx = Some(0); // wkA selected

        let tty = layout::TtySize::new(30, 120);
        let rail_page = layout::compute_with_rail_page(tty, layout::RAIL_COLS, 2, 0).unwrap();
        assert_eq!(
            rail_page.main.tiles.len(),
            2,
            "both members tile on one page"
        );

        let page_members = vec![0usize, 1usize];
        let mut snaps: Vec<PaneSnapshot> = page_members
            .iter()
            .map(|_| PaneSnapshot {
                rows: 1,
                cols: 1,
                cursor_row: 0,
                cursor_col: 0,
                scroll_offset: 0,
                cells: vec![],
            })
            .collect();

        let frame = build_frame_rail_group(
            &rail_page,
            &names,
            &mut snaps,
            &states,
            &groups,
            &badges,
            &rs,
            &rows,
            &groups[0],
            &page_members,
            None,
            &Palette::fixed(),
        );

        let all: String = (0..frame.rows)
            .map(|r| row_text(&frame, r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("wkA"), "wkA tile missing:\n{all}");
        assert!(all.contains("wkB"), "wkB tile missing:\n{all}");

        let footer_line = row_text(&frame, rail_page.footer.row);
        // Footer shows the selected group's header, which under repo-rollup is the
        // repo basename (`x` for cwd `/repo/x`), not the full path (US1).
        assert!(
            footer_line.contains("group x"),
            "footer group missing: {footer_line}"
        );

        // The selected member (wkA) title is accented; the unselected (wkB) is
        // not - so the operator can tell which pane Enter/d will drive. The two
        // tiles share a row, so count inverse cells within each tile's COLUMN
        // range rather than across the whole row.
        let t0 = &rail_page.main.tiles[0];
        let t1 = &rail_page.main.tiles[1];
        let inv_in_tile = |tile: &layout::TileRect| -> usize {
            (tile.col..tile.col + tile.cols)
                .filter(|&c| frame.get(tile.row, c).map_or(false, |x| x.inverse))
                .count()
        };
        assert!(
            inv_in_tile(t0) > inv_in_tile(t1),
            "selected tile title should be more accented than unselected (sel={}, unsel={})",
            inv_in_tile(t0),
            inv_in_tile(t1),
        );
    }

    #[test]
    fn rail_group_frame_placeholder_snapshot_keeps_tile_alignment() {
        // Even if a member's snapshot is the empty placeholder (a pane lookup
        // missed), all tiles still render their titles in their own slots - no
        // left-shift that would leave the last tile unrendered (gemini HIGH, #399).
        let rows = vec![
            json!({"name": "wkA", "cwd": "/x", "provider": "codex", "status": "live"}),
            json!({"name": "wkB", "cwd": "/x", "provider": "codex", "status": "live"}),
        ];
        let groups = group::group_by(&rows, group::GroupKey::Cwd);
        let badges =
            group::compute_badges_from_live(&groups, &[false, false], &[false, false], &[], 0);
        let names = vec!["wkA".to_string(), "wkB".to_string()];
        let states = vec![ConnState::Live, ConnState::Live];
        let mut rs = group::RailState::new(group::GroupKey::Cwd);
        rs.main_mode = group::MainMode::GroupTile;
        rs.selected_agent_idx = Some(0);

        let tty = layout::TtySize::new(30, 120);
        let rail_page = layout::compute_with_rail_page(tty, layout::RAIL_COLS, 2, 0).unwrap();
        let page_members = vec![0usize, 1usize];
        // Second member's snapshot is the empty placeholder.
        let mut snaps = vec![
            PaneSnapshot {
                rows: 1,
                cols: 1,
                cursor_row: 0,
                cursor_col: 0,
                scroll_offset: 0,
                cells: vec![],
            },
            empty_pane_snapshot(),
        ];

        let frame = build_frame_rail_group(
            &rail_page,
            &names,
            &mut snaps,
            &states,
            &groups,
            &badges,
            &rs,
            &rows,
            &groups[0],
            &page_members,
            None,
            &Palette::fixed(),
        );
        let all: String = (0..frame.rows)
            .map(|r| row_text(&frame, r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("wkA"), "wkA tile missing:\n{all}");
        assert!(
            all.contains("wkB"),
            "wkB tile missing - alignment broke:\n{all}"
        );
    }

    // ── AC3-FR / Open Q1 / Q2 follow-ups: reflow, attention summary, drill-down ──

    /// Text inside one tile's title row (its column range only), so a test can
    /// assert which agent a tile renders without catching the rail column.
    fn tile_title_text(frame: &ScreenBuffer, tile: &layout::TileRect) -> String {
        (tile.col..tile.col + tile.cols)
            .filter_map(|c| frame.get(tile.row, c))
            .map(|cell| {
                if cell.text.is_empty() {
                    " ".to_string()
                } else {
                    cell.text.clone()
                }
            })
            .collect()
    }

    fn rail_column_text(frame: &ScreenBuffer) -> String {
        (0..frame.rows)
            .map(|r| {
                (0..layout::RAIL_COLS)
                    .filter_map(|c| frame.get(r, c))
                    .map(|cell| {
                        if cell.text.is_empty() {
                            " ".to_string()
                        } else {
                            cell.text.clone()
                        }
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn live_group_of_filters_exited_members() {
        let rows = vec![
            json!({"name": "wkA", "cwd": "/repo/x", "provider": "codex", "status": "live"}),
            json!({"name": "wkB", "cwd": "/repo/x", "provider": "codex", "status": "live"}),
            json!({"name": "wkC", "cwd": "/repo/x", "provider": "codex", "status": "live"}),
        ];
        let groups = group::group_by(&rows, group::GroupKey::Cwd);
        let states = vec![
            ConnState::Live,
            ConnState::Exited { code: 0 },
            ConnState::Live,
        ];
        let live = live_group_of(&groups[0], &states);
        // Header is the repo basename (US1); key_value keeps the full path.
        assert_eq!(live.header, "x", "header preserved for rail + footer");
        assert_eq!(live.key_value, "/repo/x");
        assert_eq!(
            live.members,
            vec![0, 2],
            "exited wkB (idx 1) dropped, order kept"
        );
    }

    #[test]
    fn rail_group_tile_reflows_to_survivors_when_member_exits() {
        // AC3-FR: wkB exits mid-tile; the main area re-tiles to wkA + wkC only,
        // and the exited agent holds no tile (so a drive target never points at
        // an exited slot). wkB stays in the rail with the header exit badge.
        let rows = vec![
            json!({"name": "wkA", "cwd": "/repo/x", "provider": "codex", "status": "live"}),
            json!({"name": "wkB", "cwd": "/repo/x", "provider": "codex", "status": "live"}),
            json!({"name": "wkC", "cwd": "/repo/x", "provider": "codex", "status": "live"}),
        ];
        let groups = group::group_by(&rows, group::GroupKey::Cwd);
        let names = vec!["wkA".to_string(), "wkB".to_string(), "wkC".to_string()];
        let states = vec![
            ConnState::Live,
            ConnState::Exited { code: 0 },
            ConnState::Live,
        ];
        let badges = group::compute_badges_from_live(
            &groups,
            &[false, false, false],
            &[false, true, false],
            &[],
            0,
        );

        let mut rs = group::RailState::new(group::GroupKey::Cwd);
        rs.main_mode = group::MainMode::GroupTile;
        rs.selected_agent_idx = Some(0);

        // Mirror paint's GroupTile branch: tile only the live members.
        let live = live_group_of(&groups[0], &states);
        assert_eq!(live.members, vec![0, 2]);
        let tty = layout::TtySize::new(30, 120);
        let rail_page =
            layout::compute_with_rail_page(tty, layout::RAIL_COLS, live.members.len(), 0).unwrap();
        assert_eq!(
            rail_page.main.tiles.len(),
            2,
            "two survivors tile, not three"
        );

        let page_members: Vec<usize> = (0..rail_page.main.tiles.len())
            .filter_map(|slot| live.members.get(rail_page.main.page_start + slot).copied())
            .collect();
        assert_eq!(page_members, vec![0, 2]);
        let mut snaps: Vec<PaneSnapshot> =
            page_members.iter().map(|_| empty_pane_snapshot()).collect();

        let frame = build_frame_rail_group(
            &rail_page,
            &names,
            &mut snaps,
            &states,
            &groups,
            &badges,
            &rs,
            &rows,
            &live,
            &page_members,
            None,
            &Palette::fixed(),
        );

        let t0 = tile_title_text(&frame, &rail_page.main.tiles[0]);
        let t1 = tile_title_text(&frame, &rail_page.main.tiles[1]);
        assert!(t0.contains("wkA"), "tile 0 should be wkA: {t0}");
        assert!(t1.contains("wkC"), "tile 1 should be wkC: {t1}");
        assert!(
            !t0.contains("wkB") && !t1.contains("wkB"),
            "exited wkB must hold no tile: [{t0}] [{t1}]"
        );

        let rail_text = rail_column_text(&frame);
        assert!(
            rail_text.contains("wkB"),
            "exited wkB stays in the rail:\n{rail_text}"
        );
        assert!(
            rail_text.contains("x1"),
            "header carries the exit badge:\n{rail_text}"
        );
    }

    // ── Open Q1: global attention summary in the footer ──────────────────────

    #[test]
    fn attention_summary_formats_needs_and_exited() {
        let badge = |n: usize, x: usize| group::GroupBadge {
            needs_input: n,
            exited: x,
        };
        assert_eq!(
            attention_summary(&[badge(0, 0)]),
            None,
            "quiet when nothing needs attention"
        );
        assert_eq!(attention_summary(&[badge(3, 0)]).as_deref(), Some("!3"));
        assert_eq!(attention_summary(&[badge(0, 2)]).as_deref(), Some("x2"));
        assert_eq!(
            attention_summary(&[badge(2, 1), badge(1, 0)]).as_deref(),
            Some("!3 x1"),
            "totals roll up across all groups"
        );
    }

    #[test]
    fn rail_footer_renders_attention_summary() {
        let footer = layout::TileRect {
            row: 0,
            col: 0,
            rows: 1,
            cols: 80,
        };
        let mut frame = ScreenBuffer::blank(1, 80);
        let rs = group::RailState::new(group::GroupKey::Cwd);
        raster_footer_rail(&mut frame, &footer, &rs, Some("!2 x1"), None);
        let line = row_text(&frame, 0);
        assert!(
            line.contains("!2 x1"),
            "global attention summary missing: {line}"
        );
        assert!(
            line.contains("group-by: cwd"),
            "group-by still shown: {line}"
        );
    }

    #[test]
    fn build_frame_rail_footer_rolls_up_waiting_into_summary() {
        // End-to-end: a waiting agent rolls up into the footer attention summary.
        let rows = vec![json!({"name": "wkA", "cwd": "/x", "provider": "codex", "status": "live"})];
        let groups = group::group_by(&rows, group::GroupKey::Cwd);
        let badges = group::compute_badges_from_live(&groups, &[true], &[false], &[], 0); // wkA waiting
        let names = vec!["wkA".to_string()];
        let states = vec![ConnState::Live];
        let mut rs = group::RailState::new(group::GroupKey::Cwd);
        rs.selected_agent_idx = Some(0);
        let tty = layout::TtySize::new(24, 80);
        let rail_layout = layout::compute_with_rail(tty, layout::RAIL_COLS, 1).unwrap();
        let mut snaps = vec![empty_pane_snapshot()];
        let frame = build_frame_rail(
            &rail_layout,
            &names,
            &mut snaps,
            &states,
            &groups,
            &badges,
            &rs,
            &rows,
            None,
            &Palette::fixed(),
        );
        let footer_line = row_text(&frame, rail_layout.footer.row);
        assert!(
            footer_line.contains("!1"),
            "waiting agent rolls into footer summary: {footer_line}"
        );
    }

    // ── ab-2b55fc77: footer mode token tracks main_mode (no stale `single`) ──

    #[test]
    fn rail_footer_mode_token_follows_main_mode() {
        let footer = layout::TileRect {
            row: 0,
            col: 0,
            rows: 1,
            cols: 80,
        };
        let mut rs = group::RailState::new(group::GroupKey::Cwd);
        // E5c flips the default to GroupTile; force Single for this leg.
        rs.main_mode = group::MainMode::Single;

        // Single mode -> the mode token reads `single |` (and never `tile |`).
        let mut frame = ScreenBuffer::blank(1, 80);
        raster_footer_rail(&mut frame, &footer, &rs, None, None);
        let single = row_text(&frame, 0);
        assert!(
            single.contains("single |"),
            "Single footer mode token: {single}"
        );
        assert!(
            !single.contains("tile |"),
            "Single footer must not read tile: {single}"
        );

        // GroupTile falling through to this Single raster (empty / all-exited /
        // too-narrow group) reads `tile |`, not a stale `single |` (ab-2b55fc77).
        rs.main_mode = group::MainMode::GroupTile;
        let mut frame2 = ScreenBuffer::blank(1, 80);
        raster_footer_rail(&mut frame2, &footer, &rs, None, None);
        let tile = row_text(&frame2, 0);
        assert!(
            tile.contains("tile |"),
            "GroupTile fall-through reads tile: {tile}"
        );
        assert!(
            !tile.contains("single |"),
            "stale single token must be gone: {tile}"
        );
    }

    // ── Open Q2: GroupTile footer advertises the Enter-to-focus drill ─────────

    #[test]
    fn rail_group_footer_advertises_enter_focus() {
        let footer = layout::TileRect {
            row: 0,
            col: 0,
            rows: 1,
            cols: 120,
        };
        let mut frame = ScreenBuffer::blank(1, 120);
        let mut rs = group::RailState::new(group::GroupKey::Cwd);
        rs.enter_nav(); // the drill-down keymap shows while browsing
        raster_footer_rail_group(&mut frame, &footer, &rs, "/repo/x", 0, 1, None, None);
        let line = row_text(&frame, 0);
        assert!(
            line.contains("Enter focus"),
            "drill-down affordance missing from GroupTile footer: {line}"
        );
    }

    // ── Attention filter (`a`): footer token reflects RailState.attention_filter ──

    #[test]
    fn rail_footer_shows_filter_token_only_when_active() {
        let footer = layout::TileRect {
            row: 0,
            col: 0,
            rows: 1,
            cols: 100,
        };
        let mut rs = group::RailState::new(group::GroupKey::Cwd);
        rs.enter_nav(); // the `a` toggle is advertised in the browse keymap

        // Off: no filter token, and the keymap advertises the toggle.
        let mut frame = ScreenBuffer::blank(1, 100);
        raster_footer_rail(&mut frame, &footer, &rs, None, None);
        let off = row_text(&frame, 0);
        assert!(
            !off.contains("filter: attention"),
            "no filter token when off: {off}"
        );
        assert!(
            off.contains("a attn"),
            "keymap advertises the `a` toggle: {off}"
        );

        // On: the footer names the active filter so the operator never wonders
        // why agents are missing from the rail.
        rs.attention_filter = true;
        let mut frame2 = ScreenBuffer::blank(1, 100);
        raster_footer_rail(&mut frame2, &footer, &rs, None, None);
        let on = row_text(&frame2, 0);
        assert!(
            on.contains("filter: attention"),
            "filter token shown when on: {on}"
        );
    }

    #[test]
    fn rail_group_footer_shows_filter_token_when_active() {
        let footer = layout::TileRect {
            row: 0,
            col: 0,
            rows: 1,
            cols: 140,
        };
        let mut frame = ScreenBuffer::blank(1, 140);
        let mut rs = group::RailState::new(group::GroupKey::Cwd);
        rs.attention_filter = true;
        raster_footer_rail_group(&mut frame, &footer, &rs, "/repo/x", 0, 1, None, None);
        let line = row_text(&frame, 0);
        assert!(
            line.contains("filter: attention"),
            "GroupTile footer names the filter: {line}"
        );
    }

    #[test]
    fn attention_filter_label_is_empty_when_off() {
        assert_eq!(attention_filter_label(false), "");
        assert!(attention_filter_label(true).contains("filter: attention"));
    }

    // ── codex P2: Enter drill re-anchors off an exited (hidden) selection ─────

    #[test]
    fn drill_down_reanchors_off_exited_selection_to_live_survivor() {
        // In GroupTile, if the selected member has exited it is filtered out of the
        // visible tiles, but `selected_agent_idx` still points at it. The Enter
        // drill must re-anchor to a live survivor before zooming to Single, so it
        // never focuses a dead pane that was never on screen. This exercises the
        // exact building blocks the drill arm runs (`selected_group` + `live_group_of`
        // + the contains-check), mirroring the inline re-anchor step.
        let rows = vec![
            json!({"name": "wkA", "cwd": "/repo/x", "provider": "codex", "status": "live"}),
            json!({"name": "wkB", "cwd": "/repo/x", "provider": "codex", "status": "live"}),
        ];
        let groups = group::group_by(&rows, group::GroupKey::Cwd);
        let states = vec![ConnState::Exited { code: 0 }, ConnState::Live]; // wkA exited
        let mut rs = group::RailState::new(group::GroupKey::Cwd);
        rs.main_mode = group::MainMode::GroupTile;
        rs.selected_agent_idx = Some(0); // selection on the exited wkA (hidden)

        // The drill arm's re-anchor step, verbatim:
        let reanchored = {
            let sel_group = rs.selected_group(&groups).unwrap();
            let live = live_group_of(sel_group, &states);
            let on_live = rs
                .selected_agent_idx
                .is_some_and(|s| live.members.contains(&s));
            assert!(!on_live, "selection wkA exited -> not a live member");
            live.members.first().copied()
        };
        if let Some(first) = reanchored {
            rs.selected_agent_idx = Some(first);
        }
        assert_eq!(
            rs.selected_agent_idx,
            Some(1),
            "Enter drill re-anchors to the live survivor wkB, never the dead wkA"
        );
    }

    #[test]
    fn drill_down_keeps_selection_when_whole_group_dead() {
        // Degenerate: every member exited. There is no live survivor to re-anchor
        // to, so the selection stays put and Single shows the (exited) pane.
        let rows = vec![
            json!({"name": "wkA", "cwd": "/repo/x", "provider": "codex", "status": "live"}),
            json!({"name": "wkB", "cwd": "/repo/x", "provider": "codex", "status": "live"}),
        ];
        let groups = group::group_by(&rows, group::GroupKey::Cwd);
        let states = vec![ConnState::Exited { code: 0 }, ConnState::Exited { code: 1 }];
        let mut rs = group::RailState::new(group::GroupKey::Cwd);
        rs.main_mode = group::MainMode::GroupTile;
        rs.selected_agent_idx = Some(0);

        let sel_group = rs.selected_group(&groups).unwrap();
        let live = live_group_of(sel_group, &states);
        assert!(live.members.is_empty(), "no survivors");
        if let Some(&first) = live.members.first() {
            rs.selected_agent_idx = Some(first);
        }
        assert_eq!(
            rs.selected_agent_idx,
            Some(0),
            "no survivor -> selection unchanged"
        );
    }

    // ── E5c AC-3: ? help overlay ──────────────────────────────────────────────

    /// AC-E5c-3: help_overlay_lines returns non-empty content and mentions `?`.
    #[test]
    fn ac_e5c_3_help_overlay_lines_non_empty_and_mentions_question_mark() {
        // AC-E5c-3: every variant must list bindings and include `?` (the close key).
        let rail_off = help_overlay_lines(false);
        assert!(!rail_off.is_empty(), "no-rail lines must not be empty");
        let joined_off = rail_off.join("\n");
        assert!(joined_off.contains('?'), "no-rail lines must mention ?");

        let rail_on = help_overlay_lines(true);
        assert!(!rail_on.is_empty(), "rail lines must not be empty");
        let joined_on = rail_on.join("\n");
        assert!(joined_on.contains('?'), "rail lines must mention ?");
    }

    /// AC-E5c-3: help_overlay_lines includes the core WATCH bindings.
    #[test]
    fn ac_e5c_3_help_overlay_lines_contains_core_bindings() {
        // AC-E5c-3: core keys (q, Enter, Esc, ]/[, Space) present in no-rail variant.
        let lines = help_overlay_lines(false);
        let text = lines.join("\n");
        assert!(text.contains('q'), "missing q");
        assert!(text.contains("Enter"), "missing Enter");
        assert!(text.contains("Esc"), "missing Esc");
        assert!(text.contains(']'), "missing ]");
        assert!(text.contains('['), "missing [");
        assert!(text.contains("Space"), "missing Space");
    }

    /// AC-E5c-3: rail variant adds g/a/Tab bindings.
    #[test]
    fn ac_e5c_3_help_overlay_lines_rail_includes_extra_bindings() {
        // AC-E5c-3: rail keys (g, a, Tab) appear only in the rail=true variant.
        let text_rail = help_overlay_lines(true).join("\n");
        assert!(text_rail.contains('g'), "rail lines must mention g");
        assert!(text_rail.contains('a'), "rail lines must mention a");
        assert!(text_rail.contains("Tab"), "rail lines must mention Tab");
        // x-0175 Discretion #1: the new remove key is documented in the overlay.
        assert!(
            text_rail.contains("r              remove"),
            "rail lines must document the r remove key"
        );
    }

    /// Owned-PTY: bare `?` always passes through (it types to the agent; the
    /// overlay opens via leader+?). While open it is inert like any other key.
    #[test]
    fn help_key_action_question_mark_passes_through_or_inert() {
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        // Overlay closed: `?` forwards to the agent (run loop hits key_to_input).
        assert_eq!(
            help_key_action(k(KeyCode::Char('?')), false),
            HelpAction::Passthrough,
        );
        // Overlay open: `?` is inert (does not leak behind the overlay).
        assert_eq!(
            help_key_action(k(KeyCode::Char('?')), true),
            HelpAction::Inert,
        );
    }

    /// While the overlay is open, navigation keys are inert; q closes.
    #[test]
    fn help_key_action_inert_while_open() {
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(help_key_action(k(KeyCode::Tab), true), HelpAction::Inert);
        assert_eq!(help_key_action(k(KeyCode::Right), true), HelpAction::Inert);
        assert_eq!(
            help_key_action(k(KeyCode::Char('q')), true),
            HelpAction::Close,
        );
    }

    /// Esc closes the overlay when open; passes through when closed.
    #[test]
    fn help_key_action_esc_closes() {
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(help_key_action(k(KeyCode::Esc), true), HelpAction::Close);
        assert_eq!(
            help_key_action(k(KeyCode::Esc), false),
            HelpAction::Passthrough,
        );
    }

    /// Ctrl-C always passes through to the outer Quit handler, overlay or not.
    #[test]
    fn help_key_action_ctrl_c_always_passes_through() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(help_key_action(ctrl_c, true), HelpAction::Passthrough);
    }

    /// AC-E5c-3: overlay render does not corrupt the frame and is visible.
    #[test]
    fn ac_e5c_3_raster_help_overlay_writes_visible_content() {
        // AC-E5c-3: a 40x80 buffer gets overlay lines; content appears, borders present.
        let mut frame = ScreenBuffer::blank(40, 80);
        let lines = help_overlay_lines(false);
        raster_help_overlay(&mut frame, &lines);
        let mut rendered = String::new();
        for r in 0..frame.rows {
            for c in 0..frame.cols {
                let ch = frame
                    .get(r, c)
                    .map(|cell| {
                        if cell.text.is_empty() {
                            ' '
                        } else {
                            cell.text.chars().next().unwrap_or(' ')
                        }
                    })
                    .unwrap_or(' ');
                rendered.push(ch);
            }
            rendered.push('\n');
        }
        // Box-drawing border and at least one binding line must appear.
        assert!(
            rendered.contains('┌') || rendered.contains('┏'),
            "overlay border missing"
        );
        assert!(rendered.contains('?'), "overlay content must mention ?");
    }

    // ── E5c AC-4: OSC 10/11 palette integration ────────────────────────────

    /// AC4-HP: A ScreenBuffer created with `with_chrome` applies the palette
    /// fg/bg to cells written by `put_str`.
    #[test]
    fn screen_buffer_with_chrome_colors_put_str_cells() {
        let palette = crate::grid::palette::Palette::from_terminal(
            (200, 200, 200), // fg
            (10, 10, 10),    // bg
        );
        let mut buf = ScreenBuffer::with_chrome(1, 10, palette.border, palette.bg);
        buf.put_str(0, 0, "hi", false);
        let cell = buf.get(0, 0).unwrap();
        assert_ne!(
            cell.fg,
            CellColor::Default,
            "with_chrome put_str cell fg must be palette.border, not Default"
        );
        assert_eq!(cell.fg, palette.border, "cell fg must equal palette.border");
    }

    /// AC4-HP: `build_frame` with a non-default palette produces border cells
    /// with the palette's border color, not CellColor::Default.
    #[test]
    fn build_frame_palette_applied_to_border_cells() {
        let tty = TtySize::new(24, 80);
        let names = vec!["wkA".to_string()];
        let states = vec![ConnState::Live];
        let comp = Compositor::new(1);
        let paged = layout::compute_page(tty, 1, 0).unwrap();
        let mut snaps = vec![one_pane(b"hello").pop().unwrap().snapshot()];
        let palette = crate::grid::palette::Palette::from_terminal(
            (220, 220, 220), // fg
            (10, 10, 10),    // bg
        );
        let frame = build_frame(
            &paged,
            &names,
            &mut snaps,
            &states,
            &comp,
            &[true],
            None,
            &[],
            None,
            &palette,
        );
        // The top-left cell is the border corner (┌). Its fg should be palette.border.
        let tile = &paged.tiles[0];
        let corner = frame.get(tile.row, tile.col).unwrap();
        assert_eq!(
            corner.fg, palette.border,
            "border corner fg must be palette.border"
        );
    }

    /// AC4-VERIFY: `Palette::fixed()` yields the same behavior as the pre-palette
    /// chrome: all chrome cells keep CellColor::Default fg/bg.
    #[test]
    fn build_frame_fixed_palette_preserves_default_colors() {
        let tty = TtySize::new(24, 80);
        let names = vec!["wkA".to_string()];
        let states = vec![ConnState::Live];
        let comp = Compositor::new(1);
        let paged = layout::compute_page(tty, 1, 0).unwrap();
        let mut snaps = vec![one_pane(b"hello").pop().unwrap().snapshot()];
        let frame = build_frame(
            &paged,
            &names,
            &mut snaps,
            &states,
            &comp,
            &[true],
            None,
            &[],
            None,
            &crate::grid::palette::Palette::fixed(),
        );
        let tile = &paged.tiles[0];
        let corner = frame.get(tile.row, tile.col).unwrap();
        assert_eq!(
            corner.fg,
            CellColor::Default,
            "fixed palette: border corner fg must remain Default"
        );
    }
}
