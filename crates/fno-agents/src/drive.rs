//! Drive surface (Wave 4, ab-8d258ddb): WebSocket-backed interactive / watch /
//! step takeover of a PTY-managed agent.
//!
//! The drive verb is the user-facing payoff of the Phase 6 substrate. After a
//! client sends the `agent.drive` JSON-RPC request the daemon validates it,
//! emits `drive_attached` BEFORE any frame (the ordering invariant the stop
//! hook depends on), acks with a `session_id`, then upgrades the SAME Unix
//! stream to a WebSocket (LD21: tokio-tungstenite standard handshake). From
//! there:
//!
//! - **binary frames** carry raw PTY bytes — client->daemon are keystrokes
//!   (forwarded to the worker's stdin unless this is a watch session),
//!   daemon->client is live PTY output (streamed via the worker's incremental
//!   `read_since` cursor, Wave 4 task 4.0).
//! - **text frames** carry control JSON: `resize` (initial handshake + on
//!   SIGWINCH), `ping` (client heartbeat), `detach` (clean sentinel exit with
//!   stats). The daemon replies `pong` and emits `dropped` notices.
//!
//! Every drive session ends with EXACTLY ONE of `drive_detached` (here) or
//! `drive_crashed` (daemon recovery) — the no-silent-leak invariant.
//!
//! Concurrency model: one controlling driver (interactive / step / paranoid)
//! per agent, recorded in `state.json`'s drive window so the stop hook's
//! authority check can see it; many read-only watchers per agent (up to a cap),
//! tracked in-memory only because they neither write input nor open a
//! gate-hardening window (LD24). The [`DriveTable`] is the daemon's in-memory
//! source of truth for caps, single-driver enforcement, and (Wave 4 task 4.2)
//! stale-driver takeover + heartbeat eviction.

use crate::events::EventEmitter;
use crate::paths::AgentsHome;
use crate::protocol::{read_response, write_request, write_response, ErrorCode, Request, Response};
use crate::state::{self, DriveWindow};
use crate::{AgentStatus, MonotonicTimestamp};
use base64::Engine as _;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// LD: `config.drive.max_watchers_per_agent` default.
pub const DEFAULT_MAX_WATCHERS_PER_AGENT: usize = 5;
/// LD: `config.drive.max_concurrent` default (controlling drivers across all
/// agents).
pub const DEFAULT_MAX_CONCURRENT_DRIVES: usize = 10;
/// Heartbeat staleness threshold (LD17: monotonic). A controlling driver whose
/// last client ping is older than this is force-closed by the per-session
/// watchdog with reason `heartbeat_lost`. This is the PRIMARY eviction path for
/// a live, connected session, and it removes the driver's table handle promptly.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);
/// Stale-driver takeover threshold: at admission, a NEW controlling driver may
/// evict an existing one whose heartbeat is older than this (event
/// `drive_takeover_after_stale`). Because the watchdog above already force-closes
/// a healthy session at `HEARTBEAT_TIMEOUT` and frees its handle, this longer
/// threshold is reached only by an ORPHANED handle whose session task (and its
/// watchdog) died without cleanup. So takeover-after-stale is the orphan-recovery
/// FALLBACK layered above the watchdog, never the primary eviction for a
/// well-behaved session. The invariant `STALE_DRIVER_IDLE > HEARTBEAT_TIMEOUT`
/// (asserted below) keeps that layering coherent: a normally-functioning driver
/// is always reaped by its own watchdog (clean `heartbeat_lost`) before a peer
/// could claim it as stale.
pub const STALE_DRIVER_IDLE: Duration = Duration::from_secs(30);

/// Compile-time guard for the watchdog/takeover layering: the stale-takeover
/// threshold MUST exceed the heartbeat watchdog timeout so takeover stays an
/// orphan-recovery fallback rather than racing the watchdog as a primary
/// eviction path. Collapsing `STALE_DRIVER_IDLE` to/below `HEARTBEAT_TIMEOUT`
/// (or raising the watchdog past it) would silently re-attribute clean heartbeat
/// loss as `takeover_after_stale`; this stops that drift at build time.
const _: () = assert!(
    STALE_DRIVER_IDLE.as_millis() > HEARTBEAT_TIMEOUT.as_millis(),
    "STALE_DRIVER_IDLE must exceed HEARTBEAT_TIMEOUT (watchdog is the primary eviction; takeover is the orphan-recovery fallback)"
);
/// How long the daemon waits for the client's initial `resize` before falling
/// back to a 24x80 default with a warning (LD18).
pub const INITIAL_RESIZE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the daemon waits for the WebSocket handshake to complete after the
/// `agent.drive` ack. A client that never finishes the upgrade must not pin the
/// controlling slot + authority window; on timeout the attach is abandoned.
pub const UPGRADE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Ceiling on the cross-process claim probe in [`external_claude_writer`], which
/// shells the `fno` (Python) CLI on the interactive-claude drive-OPEN path. On a
/// loaded box (a grid of many agents) that spawn can outrun the client's 3s
/// drive-open budget, so every interactive-claude refocus fails to open and the
/// grid appears frozen. The in-process single-controller `table` slot is the
/// primary writer guard; this probe only backstops an external relay, so it fails
/// OPEN when it outruns this budget. Well under the client's `DRIVE_OPEN_TIMEOUT`.
const CLAIM_PROBE_TIMEOUT: Duration = Duration::from_millis(750);
/// PTY output poll cadence for the drive output pump.
const OUTPUT_POLL_INTERVAL: Duration = Duration::from_millis(30);

type Ws = WebSocketStream<UnixStream>;
type WsSink = SplitSink<Ws, Message>;

/// The four drive modes a client can request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveMode {
    /// Full bidirectional takeover; opens a gate-hardening window.
    Interactive,
    /// Read-only observation; no input, no authority window (LD24).
    Watch,
    /// Per-line confirmation (client-gated); authority window.
    Step,
    /// Per-byte confirmation (client-gated); authority window.
    Paranoid,
}

impl DriveMode {
    pub fn parse(s: &str) -> Option<DriveMode> {
        match s {
            "interactive" => Some(DriveMode::Interactive),
            "watch" => Some(DriveMode::Watch),
            "step" => Some(DriveMode::Step),
            "paranoid" => Some(DriveMode::Paranoid),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            DriveMode::Interactive => "interactive",
            DriveMode::Watch => "watch",
            DriveMode::Step => "step",
            DriveMode::Paranoid => "paranoid",
        }
    }

    /// Watch is the sole read-only mode.
    pub fn is_watch(&self) -> bool {
        matches!(self, DriveMode::Watch)
    }

    /// Whether this mode opens the operator-authority / gate-hardening window.
    ///
    /// LD24/29 name `interactive` and `step` explicitly; `paranoid` is a
    /// stricter `step`, so it hardens too — treating it as read-only would
    /// leave an authority hole. The carve-out is `watch` alone.
    pub fn opens_authority_window(&self) -> bool {
        !self.is_watch()
    }
}

/// One attached driver/watcher. The handle lives in the [`DriveTable`] for the
/// session's lifetime so the daemon can enforce caps, surface the active driver
/// to `stop`/`rm`, evict stale drivers, and force-close on takeover.
pub struct DriverHandle {
    pub session_id: String,
    pub mode: DriveMode,
    /// Last client heartbeat (monotonic). Bumped on attach and every `ping`.
    pub last_heartbeat: Arc<std::sync::Mutex<MonotonicTimestamp>>,
    /// Set to `true` to force this session closed (takeover, `stop --force`,
    /// heartbeat eviction). A `watch` channel (not `Notify`) so the signal
    /// LATCHES — a forcer that fires between the session's select iterations is
    /// still observed, and it fans out to every subscriber (input loop + pump).
    close_tx: Arc<watch::Sender<bool>>,
    /// Reason recorded by the forcer, read by the session loop when close fires
    /// so the `drive_detached` event attributes the cause.
    pub close_reason: Arc<std::sync::Mutex<Option<String>>>,
}

impl DriverHandle {
    fn new(session_id: String, mode: DriveMode) -> Self {
        let (close_tx, _rx) = watch::channel(false);
        DriverHandle {
            session_id,
            mode,
            last_heartbeat: Arc::new(std::sync::Mutex::new(MonotonicTimestamp::now())),
            close_tx: Arc::new(close_tx),
            close_reason: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn heartbeat_age(&self) -> Duration {
        self.last_heartbeat
            .lock()
            .map(|h| h.elapsed())
            .unwrap_or_default()
    }

    /// Force this session closed with an attributed reason.
    pub fn force_close(&self, reason: &str) {
        if let Ok(mut r) = self.close_reason.lock() {
            if r.is_none() {
                *r = Some(reason.to_string());
            }
        }
        let _ = self.close_tx.send(true);
    }
}

/// Per-agent drive state: at most one controlling driver, plus watchers.
#[derive(Default)]
pub struct AgentDrives {
    pub controlling: Option<DriverHandle>,
    pub watchers: Vec<DriverHandle>,
}

/// The daemon's in-memory drive registry, keyed by agent `short_id`.
pub type DriveTable = Arc<Mutex<HashMap<String, AgentDrives>>>;

/// Construct an empty drive table for the daemon `Ctx`.
pub fn new_table() -> DriveTable {
    Arc::new(Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Lifecycle-verb query surface (Wave 5). `stop`/`rm`/`status` consult the
// in-memory drive table — the daemon-local source of truth for who is driving
// — rather than re-reading `state.json` (whose drive window exists for the
// out-of-process stop hook, not for the daemon's own decisions).
// ---------------------------------------------------------------------------

/// The active controlling driver for `short_id`, if any: its `(session_id,
/// mode)`. `stop`/`rm` consult this to refuse (or, for `stop --force`, evict)
/// while a driver holds the agent. Watchers do NOT count — only the single
/// controlling driver opens an authority window and blocks lifecycle ops
/// (LD24).
pub async fn controlling_driver(table: &DriveTable, short_id: &str) -> Option<(String, DriveMode)> {
    let t = table.lock().await;
    t.get(short_id)
        .and_then(|a| a.controlling.as_ref())
        .map(|h| (h.session_id.clone(), h.mode))
}

/// Force-close `short_id`'s controlling driver with an attributed `reason`
/// (e.g. `stop_force`). Returns `true` if a driver was signalled. The drive
/// session loop observes the latched close, emits `drive_detached{reason}`, and
/// clears the table slot + `state.json` window in [`cleanup`] — so callers
/// should briefly await the slot clearing (see [`await_driver_cleared`]) before
/// mutating the agent.
pub async fn force_close_controlling(table: &DriveTable, short_id: &str, reason: &str) -> bool {
    let t = table.lock().await;
    match t.get(short_id).and_then(|a| a.controlling.as_ref()) {
        Some(h) => {
            h.force_close(reason);
            true
        }
        None => false,
    }
}

/// Poll until `short_id` has no controlling driver (the session loop's
/// [`cleanup`] ran) or `budget` elapses. Returns `true` if the slot cleared in
/// time. Used after [`force_close_controlling`] so `stop --force` does not race
/// the detaching session.
pub async fn await_driver_cleared(table: &DriveTable, short_id: &str, budget: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if controlling_driver(table, short_id).await.is_none() {
            return true;
        }
        if start.elapsed() >= budget {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Count controlling drivers across all agents (the `drives.active` field of
/// `status-v1.json`). Watchers are excluded for the same reason as above.
pub async fn active_drive_count(table: &DriveTable) -> usize {
    table
        .lock()
        .await
        .values()
        .filter(|a| a.controlling.is_some())
        .count()
}

/// True iff `status` permits a drive (LD28: ready | idle | busy | live).
fn drive_eligible(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Ready | AgentStatus::Idle | AgentStatus::Busy | AgentStatus::Live
    )
}

/// Generate a session id unique within this daemon (monotonic ns + counter).
fn next_session_id(short_id: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "drv-{short_id}-{}-{n}",
        MonotonicTimestamp::now().as_nanos()
    )
}

/// Validation outcome: either an accepted attach (with the resolved facts) or a
/// structured rejection to send back to the client.
enum Admit {
    Ok {
        short_id: String,
        mode: DriveMode,
        session_id: String,
    },
    Reject(Response),
}

/// Cross-process single-writer state for a claude `session:<uuid>` claim, read
/// from `fno claim status session:<uuid> -J` (X1 / AC3-EDGE). Pure data so the
/// drive verdict is unit-testable without shelling the CLI.
#[derive(Debug, PartialEq, Eq)]
enum SessionClaimState {
    /// No claim, a stale (dead-holder) claim, or an unreadable record: the drive
    /// is allowed (fail-open, matching the daemon's best-effort acquire posture -
    /// the in-daemon `table` lock stays the authoritative same-process gate).
    FreeOrUnknown,
    /// A claim held LIVE by `holder`.
    Live { holder: String },
}

/// Parse `fno claim status ... -J` output into a [`SessionClaimState`]. Only a
/// `state == "live"` record carrying a `holder` pins a writer; `free` / `stale`
/// / missing fields all read as `FreeOrUnknown`.
fn parse_session_claim_state(v: &serde_json::Value) -> SessionClaimState {
    let live = v.get("state").and_then(|s| s.as_str()) == Some("live");
    match (live, v.get("holder").and_then(|h| h.as_str())) {
        (true, Some(h)) => SessionClaimState::Live {
            holder: h.to_string(),
        },
        _ => SessionClaimState::FreeOrUnknown,
    }
}

/// The X1 grid-drive interlock decision (AC3-EDGE), pure for testability: a
/// claude pane may be driven UNLESS its `session:<uuid>` claim is held live by a
/// holder OTHER than this daemon's own interactive-spawn holder (`pty:<short_id>`).
/// Returns `Some(holder)` to refuse with `BusyElsewhere{holder}`, `None` to allow.
///
/// Forward-compatible with both unresolved X1 resolutions: if the relay routes
/// through the daemon's held claim (AC3-FR) the holder stays `pty:<short_id>` and
/// this always allows; if a future writer acquires its own holder (AC3-EDGE) this
/// refuses. Either way grid-drive never interleaves with an external writer.
fn external_session_writer(state: &SessionClaimState, self_holder: &str) -> Option<String> {
    match state {
        SessionClaimState::Live { holder } if holder != self_holder => Some(holder.clone()),
        _ => None,
    }
}

/// The GLOBAL claims dir for `session:` keys, mirroring `fno.claims.io`: a session
/// claim is durable + cross-checkout, so it is rooted at `$FNO_CLAIMS_ROOT` (else
/// `$HOME`) under `.fno/claims`, NOT the cwd-local repo dir. `None` when no root
/// resolves (then the gate can't derive a path and falls through to the CLI).
fn global_claims_dir() -> Option<PathBuf> {
    // Match the Python source of truth (`fno.claims.io.global_claims_root`): an
    // EMPTY `FNO_CLAIMS_ROOT` is treated as UNSET (falls back to `$HOME`). A
    // set-but-empty value must NOT resolve the claims dir to a cwd-relative
    // `.fno/claims` - that would let the gate miss a real `~/.fno/claims/...`
    // lock and fail the interlock open (gemini HIGH / codex P2). Not filtered:
    // the literal `"null"`, because Python treats it as a real path - filtering
    // it would re-introduce the very divergence this closes.
    global_claims_dir_from(
        std::env::var_os("FNO_CLAIMS_ROOT"),
        std::env::var_os("HOME"),
    )
}

/// Testable core of [`global_claims_dir`]: the two env values are explicit so the
/// empty-is-unset contract is exercised without mutating process-global env.
fn global_claims_dir_from(
    claims_root: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let non_empty = |v: std::ffi::OsString| (!v.is_empty()).then_some(v);
    claims_root
        .and_then(non_empty)
        .or_else(|| home.and_then(non_empty))
        .map(PathBuf::from)
        .map(|r| r.join(".fno/claims"))
}

/// The claim lockfile path for `session:<uuid>` under `dir`, mirroring the Python
/// filename layout `<url-encoded-key>.lock`. `fno`'s `quote(key, safe="")` escapes
/// every byte that is NOT url-unreserved, so for a `session:<uuid>` key the only
/// escaped byte is the `:` (`%3A`) as long as the uuid is unreserved-only. If the
/// uuid carries a byte that WOULD escape, return `None` so a derived path can never
/// silently disagree with the CLI's encoding - the caller then falls through to the
/// authoritative CLI probe (which does its own encoding).
fn session_claim_lock_path_in(dir: &Path, uuid: &str) -> Option<PathBuf> {
    let unreserved = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
    if uuid.is_empty() || !uuid.bytes().all(unreserved) {
        return None;
    }
    Some(dir.join(format!("session%3A{uuid}.lock")))
}

/// Read the cross-process `session:<uuid>` claim and return an EXTERNAL writer's
/// holder if one is live (X1 / AC3-EDGE), WITHOUT paying for the `fno` Python CLI
/// in the common case. The claim is only ever a lockfile, so if none exists there
/// is no writer - see [`external_claude_writer_gated`].
async fn external_claude_writer(uuid: &str, self_holder: &str) -> Option<String> {
    // Honor FNO_BIN (test/custom environments); fall back to PATH `fno`. var_os
    // avoids silently dropping a non-UTF-8 path; a set-but-EMPTY value is treated
    // as unset (else `Command::new("")` just fails to spawn) (gemini HIGH).
    let fno_bin = std::env::var_os("FNO_BIN")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "fno".into());
    external_claude_writer_gated(
        uuid,
        self_holder,
        global_claims_dir(),
        fno_bin,
        CLAIM_PROBE_TIMEOUT,
    )
    .await
}

/// Gate the CLI probe on the claim lockfile's EXISTENCE (herdr-informed): the
/// cross-process claim is only ever a lockfile, so if none exists there is no
/// external writer and we short-circuit WITHOUT shelling the `fno` Python CLI -
/// whose cold start on a loaded box is what froze the grid on every interactive-
/// claude refocus. Only a PRESENT lockfile pays for the authoritative (bounded)
/// [`external_claude_writer_bin`] probe. A path we cannot confidently derive
/// (`claims_dir` None, or a uuid that would url-encode) falls through to the CLI.
///
/// Semantics are unchanged: an absent lockfile and a derive-miss both match the
/// CLI's `free -> no external writer` verdict; the only difference is skipping a
/// subprocess when the answer (no claim -> no writer) is already knowable from the
/// filesystem. A wrong `claims_dir` can only degrade to today's timeout fail-open,
/// never to a silently-broken interlock.
async fn external_claude_writer_gated(
    uuid: &str,
    self_holder: &str,
    claims_dir: Option<PathBuf>,
    fno_bin: std::ffi::OsString,
    budget: Duration,
) -> Option<String> {
    if let Some(dir) = claims_dir {
        if let Some(path) = session_claim_lock_path_in(&dir, uuid) {
            if !path.exists() {
                return None;
            }
        }
    }
    external_claude_writer_bin(uuid, self_holder, fno_bin, budget).await
}

/// Testable core of [`external_claude_writer`]: the `fno` binary and the probe
/// budget are explicit so a slow-probe (fail-open) regression can be exercised
/// without mutating the process-global `FNO_BIN` env.
async fn external_claude_writer_bin(
    uuid: &str,
    self_holder: &str,
    fno_bin: std::ffi::OsString,
    budget: Duration,
) -> Option<String> {
    if uuid.is_empty() {
        return None;
    }
    let key = format!("session:{uuid}");
    let probe = tokio::task::spawn_blocking(move || {
        std::process::Command::new(fno_bin)
            .args(["claim", "status", &key, "-J"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
    });
    // Bound the probe: `fno` is a Python CLI, and on a busy machine its cold
    // start can exceed the client's drive-open budget, stalling the grid on every
    // interactive-claude refocus. On timeout, fail OPEN (drop the handle; the
    // orphaned `output()` finishes and reaps itself on the blocking pool).
    let out = match tokio::time::timeout(budget, probe).await {
        Ok(Ok(Some(o))) => o,
        _ => return None,
    };
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    external_session_writer(&parse_session_claim_state(&v), self_holder)
}

/// Validate the request and, if admitted, register the session in the table and
/// (for controlling modes) write the `state.json` drive window. Emits
/// `drive_attached` BEFORE the caller upgrades to a WebSocket.
async fn admit(
    home: &AgentsHome,
    emitter: &EventEmitter,
    table: &DriveTable,
    req: &Request,
) -> Admit {
    let name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => {
            return Admit::Reject(Response::err(
                req.id,
                ErrorCode::InvalidParams,
                "missing `name`",
            ))
        }
    };
    let mode_str = req
        .params
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("interactive");
    let mode = match DriveMode::parse(mode_str) {
        Some(m) => m,
        None => {
            return Admit::Reject(Response::err(
                req.id,
                ErrorCode::InvalidParams,
                format!("unknown drive mode `{mode_str}`"),
            ))
        }
    };

    let registry = state::load_registry(&home.registry_json()).unwrap_or_default();
    let entry = match registry.find(&name) {
        Some(e) => e.clone(),
        None => {
            return Admit::Reject(Response::err(
                req.id,
                ErrorCode::AgentNotFound,
                format!("agent {name} not found"),
            ))
        }
    };

    if !drive_eligible(entry.status) {
        return Admit::Reject(Response::err(
            req.id,
            ErrorCode::InvalidStatus,
            format!(
                "agent {name} is {}; cannot drive (need ready|idle|busy)",
                format!("{:?}", entry.status).to_lowercase()
            ),
        ));
    }

    // PTY-managed check: only agents with a live worker socket can be driven.
    // Claude agents (shellout, no PTY) and dead workers are refused here.
    let sock = home.worker_sock(&entry.short_id);
    if UnixStream::connect(&sock).await.is_err() {
        let why = if entry.provider == "claude" {
            format!("agent {name} is Claude; drive is PTY-only")
        } else {
            format!("agent {name} worker is not reachable; cannot drive")
        };
        return Admit::Reject(Response::err(req.id, ErrorCode::InvalidStatus, why));
    }

    // X1 cross-process single-writer interlock (AC3-EDGE), claude-only: if another
    // process holds this session's `fno claim session:<uuid>`, refuse the drive so
    // grid-drive and an external writer (the relay, post-E4) never interleave
    // keystrokes into one TUI. Same-daemon driver serialization stays the `table`
    // lock below; codex/gemini are unchanged (their drive conversion is deferred,
    // X1 decision #1). Only checked when actually opening an authority window.
    if entry.provider == "claude" && mode.opens_authority_window() {
        if let Some(uuid) = entry.claude_session_uuid.as_deref() {
            let self_holder = crate::daemon::interactive_claim_holder(&entry.short_id);
            if let Some(holder) = external_claude_writer(uuid, &self_holder).await {
                let _ = emitter.emit(
                    "drive_refused_busy_elsewhere",
                    &json!({"agent": name, "holder": holder, "session_uuid": uuid}),
                );
                return Admit::Reject(Response::err(
                    req.id,
                    ErrorCode::Busy,
                    format!(
                        "agent {name} session is held by {holder} (BusyElsewhere); detach the other writer first"
                    ),
                ));
            }
        }
    }

    let short_id = entry.short_id.clone();
    let session_id = next_session_id(&short_id);

    // Cap + single-driver enforcement under the table lock.
    {
        let mut t = table.lock().await;
        if mode.opens_authority_window() {
            // Eviction phase: a stale controller on THIS agent is taken over.
            {
                let slot = t.entry(short_id.clone()).or_default();
                if let Some(existing) = &slot.controlling {
                    if existing.heartbeat_age() < stale_driver_idle() {
                        return Admit::Reject(Response::err(
                            req.id,
                            ErrorCode::Busy,
                            format!(
                                "agent {name} is driven by {} (active); detach first",
                                existing.session_id
                            ),
                        ));
                    }
                    // Stale driver: take over (Wave 4 task 4.2). Evict the old one.
                    existing.force_close("takeover_after_stale");
                    let _ = emitter.emit(
                        "drive_takeover_after_stale",
                        &json!({
                            "stale_session_id": existing.session_id,
                            "new_session_id": session_id,
                            "agent": name,
                        }),
                    );
                    slot.controlling = None;
                }
            }
            // Count AFTER eviction (Codex P2): a stale takeover is a net-zero
            // replacement, so the just-evicted slot must not count against the
            // concurrency cap.
            let total_controlling = t.values().filter(|a| a.controlling.is_some()).count();
            if total_controlling >= DEFAULT_MAX_CONCURRENT_DRIVES {
                return Admit::Reject(Response::err(
                    req.id,
                    ErrorCode::Busy,
                    format!("max concurrent drives reached ({DEFAULT_MAX_CONCURRENT_DRIVES})"),
                ));
            }
            t.entry(short_id.clone()).or_default().controlling =
                Some(DriverHandle::new(session_id.clone(), mode));
        } else {
            let slot = t.entry(short_id.clone()).or_default();
            if slot.watchers.len() >= DEFAULT_MAX_WATCHERS_PER_AGENT {
                return Admit::Reject(Response::err(
                    req.id,
                    ErrorCode::Busy,
                    format!("max watchers per agent reached ({DEFAULT_MAX_WATCHERS_PER_AGENT})"),
                ));
            }
            slot.watchers
                .push(DriverHandle::new(session_id.clone(), mode));
        }
    }

    // Controlling modes write the state.json drive window (the authority signal
    // the stop hook reads). Watch sessions never do (LD24): many watchers can
    // coexist and none open authority.
    if mode.opens_authority_window() {
        let state_path = home.state_json(&short_id);
        let sid = session_id.clone();
        let mode_str = mode.as_str().to_string();
        // Locked read-modify-write so a concurrent takeover/cleanup cannot
        // interleave and drop this window (review MEDIUM #1).
        let wrote = state::update_state_atomic(&state_path, |st| {
            if let Some(pty) = st.pty.as_mut() {
                pty.drive = Some(DriveWindow {
                    session_id: Some(sid),
                    mode: Some(mode_str),
                    last_heartbeat_at_monotonic_ns: Some(MonotonicTimestamp::now().as_nanos()),
                });
            }
        });
        // If the authority window could NOT be persisted (missing/partial
        // state.json, or an I/O/lock failure), `fno agents drive-authority`
        // would report no operator driving while one actually is — breaking the
        // gate-hardening contract (LD3). Abort the attach and roll back the
        // table slot rather than admit a session with no authority signal
        // (Codex P1).
        let persisted = matches!(wrote, Ok(true));
        if !persisted {
            let mut t = table.lock().await;
            if let Some(slot) = t.get_mut(&short_id) {
                if slot
                    .controlling
                    .as_ref()
                    .map(|h| h.session_id == session_id)
                    .unwrap_or(false)
                {
                    slot.controlling = None;
                }
                if slot.controlling.is_none() && slot.watchers.is_empty() {
                    t.remove(&short_id);
                }
            }
            return Admit::Reject(Response::err(
                req.id,
                ErrorCode::Internal,
                format!("agent {name}: could not persist drive authority window; not attaching"),
            ));
        }
    }

    // Ordering invariant: drive_attached BEFORE any WS frame (the caller upgrades
    // only after admit() returns Ok).
    let _ = emitter.emit(
        "drive_attached",
        &json!({
            "session_id": session_id,
            "agent": name,
            "mode": mode.as_str(),
        }),
    );

    Admit::Ok {
        short_id,
        mode,
        session_id,
    }
}

/// Entry point from the daemon's `serve_connection`: own the accepted Unix
/// stream, validate, ack, upgrade, run the session, and clean up.
pub async fn handle_drive(
    home: &AgentsHome,
    emitter: &EventEmitter,
    table: &DriveTable,
    req: &Request,
    mut stream: UnixStream,
) {
    let (short_id, mode, session_id) = match admit(home, emitter, table, req).await {
        Admit::Ok {
            short_id,
            mode,
            session_id,
        } => (short_id, mode, session_id),
        Admit::Reject(resp) => {
            // Send the structured rejection on the raw stream; no upgrade.
            let _ = write_response(&mut stream, &resp).await;
            return;
        }
    };

    // Ack the upgrade so the client switches to the WS handshake.
    let ack = Response::ok(
        req.id,
        json!({"session_id": session_id, "mode": mode.as_str()}),
    );
    if write_response(&mut stream, &ack).await.is_err() {
        cleanup(
            home,
            emitter,
            table,
            &short_id,
            &session_id,
            mode,
            "ack_failed",
            0,
            0,
        )
        .await;
        return;
    }

    // Upgrade the same stream to a WebSocket (LD21), bounded by a timeout: a
    // client that received the ack but never completes the handshake (crash,
    // stalled socket) must not pin the controlling slot + authority window
    // forever. On timeout or handshake error, cleanup runs so the slot frees
    // (Codex P1).
    let ws = match tokio::time::timeout(
        UPGRADE_HANDSHAKE_TIMEOUT,
        tokio_tungstenite::accept_async(stream),
    )
    .await
    {
        Ok(Ok(ws)) => ws,
        Ok(Err(_)) | Err(_) => {
            let reason = "upgrade_abandoned";
            cleanup(
                home,
                emitter,
                table,
                &short_id,
                &session_id,
                mode,
                reason,
                0,
                0,
            )
            .await;
            return;
        }
    };

    let (reason, keystrokes, duration_ms) =
        run_session(home, emitter, table, ws, &short_id, &session_id, mode).await;

    cleanup(
        home,
        emitter,
        table,
        &short_id,
        &session_id,
        mode,
        &reason,
        keystrokes,
        duration_ms,
    )
    .await;
}

/// The live drive session: resize handshake, output pump, input routing, until
/// a detach trigger. Returns `(reason, keystroke_count)`.
#[allow(clippy::too_many_arguments)]
async fn run_session(
    home: &AgentsHome,
    emitter: &EventEmitter,
    table: &DriveTable,
    ws: Ws,
    short_id: &str,
    session_id: &str,
    mode: DriveMode,
) -> (String, u64, u64) {
    let sock = home.worker_sock(short_id);
    let (sink, mut source) = ws.split();
    let sink = Arc::new(Mutex::new(sink));
    let start = MonotonicTimestamp::now();
    let keystrokes = Arc::new(AtomicU64::new(0));

    // Shared signals for this session (close trigger + heartbeat clock + the
    // attributed close reason), cloned out of the registered handle so the
    // watchdog, takeover, and `stop --force` can all fire the same `close`.
    let signals = session_signals(table, short_id, session_id).await;

    // --- Heartbeat watchdog (AC3-EDGE-2, LD17): force-close a session whose
    // client stopped pinging. Monotonic age (count-during-sleep) so wall-clock
    // skew / laptop sleep does not falsely keep or expire the window. ---
    let watchdog = {
        let close_tx = Arc::clone(&signals.close_tx);
        let last_heartbeat = Arc::clone(&signals.last_heartbeat);
        let close_reason = Arc::clone(&signals.close_reason);
        let timeout = heartbeat_timeout();
        tokio::spawn(async move {
            // Check at a finer cadence than the timeout so eviction fires close
            // to the deadline rather than up to a full second late.
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let age = last_heartbeat
                    .lock()
                    .map(|h| h.elapsed())
                    .unwrap_or_default();
                if age >= timeout {
                    if let Ok(mut r) = close_reason.lock() {
                        if r.is_none() {
                            *r = Some("heartbeat_lost".to_string());
                        }
                    }
                    let _ = close_tx.send(true);
                    break;
                }
            }
        })
    };

    // --- Initial resize handshake (LD18): wait briefly for the client's first
    // resize before starting the output stream; default 24x80 on timeout. ---
    match tokio::time::timeout(INITIAL_RESIZE_TIMEOUT, source.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => {
            if let Some((r, c)) = parse_resize(&t) {
                let _ = resize_worker(&sock, r, c).await;
            }
        }
        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
            return (
                "connection_lost".into(),
                0,
                start.elapsed().as_millis() as u64,
            );
        }
        // Any non-resize first frame: proceed with the default size.
        _ => {}
    }

    // The pump signals the input loop when it ends on its own (child exit /
    // sink fault) via a oneshot carrying the reason. A oneshot LATCHES, so the
    // input loop sees it even if the pump finished before the loop polled.
    let (pump_done_tx, mut pump_done_rx) = oneshot::channel::<String>();

    // --- Output pump: stream PTY output to the client via read_since. ---
    let pump = {
        let sink = Arc::clone(&sink);
        let sock = sock.clone();
        let mut close_rx = signals.close_tx.subscribe();
        tokio::spawn(async move {
            let mut cursor = 0u64;
            // A failed worker poll is NOT proof the child exited: under load the
            // fresh per-poll worker connection can transiently fail even though
            // the agent is alive. Only conclude child-exit on an affirmative
            // child_alive:false, or after the worker has been unreachable for a
            // sustained WALL-CLOCK window (process gone, socket with it). A
            // duration budget (not a poll count) is robust to the variable poll
            // latency under load, and is comfortably longer than any heartbeat
            // timeout so a silent-but-alive client is evicted by the heartbeat
            // watchdog (with reason heartbeat_lost), not mis-attributed here.
            const UNREACHABLE_GIVE_UP: Duration = Duration::from_secs(3);
            let mut unreachable_since: Option<std::time::Instant> = None;
            let mut poll = tokio::time::interval(OUTPUT_POLL_INTERVAL);
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Some(reason) means the pump ended itself; None means a forced
            // close fired, and the input loop attributes the reason instead.
            let end_reason: Option<String> = loop {
                tokio::select! {
                    _ = close_rx.changed() => break None,
                    _ = poll.tick() => {
                        match read_since_from_worker(&sock, cursor).await {
                            Some((bytes, next, gap, alive)) => {
                                unreachable_since = None;
                                cursor = next;
                                if gap {
                                    let _ = send(&sink, Message::Text(
                                        json!({"t":"dropped"}).to_string().into())).await;
                                }
                                if !bytes.is_empty()
                                    && send(&sink, Message::Binary(bytes.into())).await.is_err()
                                {
                                    break Some("connection_lost".to_string());
                                }
                                if !alive {
                                    // Child exited: tell the client, then stop.
                                    let _ = send(&sink, Message::Text(
                                        json!({"t":"server_event","event":"child_exited"})
                                            .to_string().into())).await;
                                    break Some("child_exited".to_string());
                                }
                            }
                            None => {
                                let since =
                                    *unreachable_since.get_or_insert_with(std::time::Instant::now);
                                if since.elapsed() >= UNREACHABLE_GIVE_UP {
                                    // Sustained unreachability: the worker is gone.
                                    break Some("worker_unreachable".to_string());
                                }
                                // Transient blip: keep the drive alive and retry.
                            }
                        }
                    }
                }
            };
            if let Some(r) = end_reason {
                let _ = pump_done_tx.send(r);
            }
        })
    };

    // --- Input loop: route client frames until a detach trigger. ---
    let mut close_rx = signals.close_tx.subscribe();
    let reason = loop {
        tokio::select! {
            _ = close_rx.changed() => {
                // Read the forcer's reason from the captured Arc, NOT the table:
                // a takeover removes the old handle from the table before this
                // loop ends, so a table lookup would miss and fall back to
                // "forced" instead of "takeover_after_stale" (Gemini medium).
                break signals
                    .close_reason
                    .lock()
                    .ok()
                    .and_then(|g| g.clone())
                    .unwrap_or_else(|| "forced".to_string());
            }
            r = &mut pump_done_rx => match r {
                // The pump ended itself (child exit / unreachable / sink fault)
                // and reported why.
                Ok(reason) => break reason,
                // The pump's sender was dropped WITHOUT a reason: that only
                // happens when a forced close (heartbeat / takeover / stop)
                // woke the pump's close_rx, so the pump broke with None. The
                // authoritative reason is the forcer's, NOT "child_exited" (the
                // old fallback here mislabeled every forced close as a child
                // exit, since the RecvError raced the input loop's own close_rx).
                Err(_) => {
                    break signals
                        .close_reason
                        .lock()
                        .ok()
                        .and_then(|g| g.clone())
                        .unwrap_or_else(|| "forced".to_string())
                }
            },
            msg = source.next() => {
                match msg {
                    None => break "connection_lost".into(),
                    Some(Err(_)) => break "connection_lost".into(),
                    Some(Ok(Message::Close(_))) => break "connection_lost".into(),
                    Some(Ok(Message::Binary(bytes))) => {
                        if mode.is_watch() {
                            // Defense-in-depth (AC4-ERR): a watch client must not
                            // send input. Drop it, emit an audit event, keep the
                            // connection open. Client-side suppression is primary;
                            // this server-side rejection is the backstop.
                            let _ = emitter.emit(
                                "drive_watch_input_rejected",
                                &json!({"session_id": session_id, "bytes": bytes.len()}),
                            );
                            continue;
                        }
                        keystrokes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
                        // Step / paranoid: the client gates per line / per byte,
                        // so each frame that arrives here is operator-confirmed.
                        // Record it as a stepped keystroke for the control-plane
                        // audit (LD16: events.jsonl, not timeline).
                        if matches!(mode, DriveMode::Step | DriveMode::Paranoid) {
                            let _ = emitter.emit(
                                "drive_keystroke_stepped",
                                &json!({"session_id": session_id, "bytes": bytes.len()}),
                            );
                        }
                        let _ = write_bytes_to_worker(&sock, &bytes).await;
                    }
                    Some(Ok(Message::Text(t))) => {
                        match parse_control(&t) {
                            Control::Resize(r, c) => { let _ = resize_worker(&sock, r, c).await; }
                            Control::Ping => {
                                // Bump the captured heartbeat Arc directly rather
                                // than re-locking the table by session_id (Gemini
                                // medium): same Arc the watchdog reads.
                                if let Ok(mut h) = signals.last_heartbeat.lock() {
                                    *h = MonotonicTimestamp::now();
                                }
                                let _ = send(&sink, Message::Text(
                                    json!({"t":"pong"}).to_string().into())).await;
                            }
                            Control::Detach(reason) => break reason,
                            Control::Unknown => {}
                        }
                    }
                    Some(Ok(_)) => {} // Ping/Pong/Frame: ignore
                }
            }
        }
    };

    pump.abort();
    let _ = pump.await;
    watchdog.abort();
    let _ = watchdog.await;
    (
        reason,
        keystrokes.load(Ordering::Relaxed),
        start.elapsed().as_millis() as u64,
    )
}

/// The heartbeat-staleness threshold, overridable via `FNO_AGENTS_DRIVE_HEARTBEAT_MS`
/// (tests set a short value; production uses [`HEARTBEAT_TIMEOUT`]).
fn heartbeat_timeout() -> Duration {
    std::env::var("FNO_AGENTS_DRIVE_HEARTBEAT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(HEARTBEAT_TIMEOUT)
}

/// The stale-driver takeover threshold. Tests override it via
/// `FNO_AGENTS_DRIVE_STALE_MS` to exercise the takeover-after-stale path with a
/// short value (which deliberately inverts the production invariant); production
/// always uses [`STALE_DRIVER_IDLE`].
///
/// The override is honored ONLY in debug builds. The compile-time invariant
/// `STALE_DRIVER_IDLE > HEARTBEAT_TIMEOUT` guards the constants, but a runtime
/// env var sits outside that guard: an inherited/operational
/// `FNO_AGENTS_DRIVE_STALE_MS` <= the heartbeat would let a new driver evict a
/// still-connected one as `takeover_after_stale` before its watchdog fires,
/// reintroducing the exact race this change prevents. Gating on
/// `debug_assertions` keeps the override a test-only affordance: a release
/// daemon (built via `cargo install`) ignores it, while `cargo test` builds the
/// daemon bin in the dev profile and still gets the short threshold.
fn stale_driver_idle() -> Duration {
    if cfg!(debug_assertions) {
        if let Some(ms) = std::env::var("FNO_AGENTS_DRIVE_STALE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            return Duration::from_millis(ms);
        }
    }
    STALE_DRIVER_IDLE
}

/// The shared per-session signals, cloned out of the registered handle.
struct SessionSignals {
    close_tx: Arc<watch::Sender<bool>>,
    last_heartbeat: Arc<std::sync::Mutex<MonotonicTimestamp>>,
    close_reason: Arc<std::sync::Mutex<Option<String>>>,
}

/// Resolve this session's signals from the table (so the watchdog, takeover, and
/// `stop --force` share the same close trigger + heartbeat clock). Falls back to
/// detached fresh signals if the handle is gone (the session ends promptly).
async fn session_signals(table: &DriveTable, short_id: &str, session_id: &str) -> SessionSignals {
    let t = table.lock().await;
    if let Some(slot) = t.get(short_id) {
        let h = slot
            .controlling
            .as_ref()
            .filter(|h| h.session_id == session_id)
            .or_else(|| slot.watchers.iter().find(|h| h.session_id == session_id));
        if let Some(h) = h {
            return SessionSignals {
                close_tx: Arc::clone(&h.close_tx),
                last_heartbeat: Arc::clone(&h.last_heartbeat),
                close_reason: Arc::clone(&h.close_reason),
            };
        }
    }
    let (close_tx, _rx) = watch::channel(false);
    SessionSignals {
        close_tx: Arc::new(close_tx),
        last_heartbeat: Arc::new(std::sync::Mutex::new(MonotonicTimestamp::now())),
        close_reason: Arc::new(std::sync::Mutex::new(None)),
    }
}

/// Tear down: clear the drive window (controlling), drop the handle, emit
/// `drive_detached`. The single exit point for a non-crash drive end.
#[allow(clippy::too_many_arguments)]
async fn cleanup(
    home: &AgentsHome,
    emitter: &EventEmitter,
    table: &DriveTable,
    short_id: &str,
    session_id: &str,
    mode: DriveMode,
    reason: &str,
    keystrokes: u64,
    duration_ms: u64,
) {
    // Remove the handle from the table.
    {
        let mut t = table.lock().await;
        if let Some(slot) = t.get_mut(short_id) {
            if slot
                .controlling
                .as_ref()
                .map(|h| h.session_id == session_id)
                .unwrap_or(false)
            {
                slot.controlling = None;
            }
            slot.watchers.retain(|h| h.session_id != session_id);
            if slot.controlling.is_none() && slot.watchers.is_empty() {
                t.remove(short_id);
            }
        }
    }

    // Clear the state.json drive window iff it still names THIS session (a
    // takeover may have already replaced it with the new driver's window). The
    // ownership check runs INSIDE the locked read-modify-write so a concurrent
    // admit cannot slip its window in between our read and clear (review
    // MEDIUM #1).
    if mode.opens_authority_window() {
        let state_path = home.state_json(short_id);
        let _ = state::update_state_atomic(&state_path, |st| {
            let owns = st
                .pty
                .as_ref()
                .and_then(|p| p.drive.as_ref())
                .and_then(|d| d.session_id.as_deref())
                == Some(session_id);
            if owns {
                if let Some(p) = st.pty.as_mut() {
                    p.drive = None;
                }
            }
        });
    }

    let _ = emitter.emit(
        "drive_detached",
        &json!({
            "session_id": session_id,
            "mode": mode.as_str(),
            "reason": reason,
            "keystroke_count": keystrokes,
            "duration_ms": duration_ms,
        }),
    );
}

// ---------------------------------------------------------------------------
// Control-frame parsing.
// ---------------------------------------------------------------------------

enum Control {
    Resize(u16, u16),
    Ping,
    Detach(String),
    Unknown,
}

fn parse_control(text: &str) -> Control {
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return Control::Unknown,
    };
    match v.get("t").and_then(|t| t.as_str()) {
        Some("resize") => match parse_resize(text) {
            Some((r, c)) => Control::Resize(r, c),
            None => Control::Unknown,
        },
        Some("ping") => Control::Ping,
        Some("detach") => {
            let reason = v
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("user_sentinel")
                .to_string();
            Control::Detach(reason)
        }
        _ => Control::Unknown,
    }
}

fn parse_resize(text: &str) -> Option<(u16, u16)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let rows = v.get("rows").and_then(|x| x.as_u64())? as u16;
    let cols = v.get("cols").and_then(|x| x.as_u64())? as u16;
    if rows == 0 || cols == 0 {
        return None;
    }
    Some((rows, cols))
}

// ---------------------------------------------------------------------------
// Worker IPC helpers (fresh connection per call, matching the daemon's model).
// ---------------------------------------------------------------------------

async fn send(sink: &Arc<Mutex<WsSink>>, msg: Message) -> Result<(), ()> {
    let mut s = sink.lock().await;
    s.send(msg).await.map_err(|_| ())
}

async fn write_bytes_to_worker(sock: &std::path::Path, bytes: &[u8]) -> Option<()> {
    let mut conn = UnixStream::connect(sock).await.ok()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    write_request(
        &mut conn,
        &Request::new(1, "worker.write", json!({"bytes_b64": b64})),
    )
    .await
    .ok()?;
    let _ = read_response(&mut conn).await;
    Some(())
}

async fn read_since_from_worker(
    sock: &std::path::Path,
    cursor: u64,
) -> Option<(Vec<u8>, u64, bool, bool)> {
    let mut conn = UnixStream::connect(sock).await.ok()?;
    write_request(
        &mut conn,
        &Request::new(2, "worker.read_since", json!({"cursor": cursor})),
    )
    .await
    .ok()?;
    let resp = read_response(&mut conn).await.ok()?;
    let r = resp.result()?;
    let b64 = r.get("bytes_b64").and_then(|v| v.as_str())?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let next = r.get("next_offset").and_then(|v| v.as_u64())?;
    let gap = r.get("gap").and_then(|v| v.as_bool()).unwrap_or(false);
    let alive = r
        .get("child_alive")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    Some((bytes, next, gap, alive))
}

async fn resize_worker(sock: &std::path::Path, rows: u16, cols: u16) -> Option<()> {
    let mut conn = UnixStream::connect(sock).await.ok()?;
    write_request(
        &mut conn,
        &Request::new(3, "worker.resize", json!({"rows": rows, "cols": cols})),
    )
    .await
    .ok()?;
    let _ = read_response(&mut conn).await;
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_session_claim_state_reads_live_holder_and_fails_open() {
        // free / stale / missing-state / live-without-holder all read as FreeOrUnknown.
        assert_eq!(
            parse_session_claim_state(&json!({"key": "session:x", "state": "free"})),
            SessionClaimState::FreeOrUnknown
        );
        assert_eq!(
            parse_session_claim_state(
                &json!({"key": "session:x", "state": "stale", "holder": "pty:dead"})
            ),
            SessionClaimState::FreeOrUnknown
        );
        assert_eq!(
            parse_session_claim_state(&json!({"key": "session:x", "state": "live"})),
            SessionClaimState::FreeOrUnknown
        );
        // A live claim with a holder pins a writer.
        assert_eq!(
            parse_session_claim_state(&json!({"state": "live", "holder": "relay:abc"})),
            SessionClaimState::Live {
                holder: "relay:abc".into()
            }
        );
    }

    #[test]
    fn external_session_writer_allows_self_refuses_other() {
        let self_holder = "pty:short1";
        // Self-held (the daemon's own interactive spawn) -> allow.
        assert_eq!(
            external_session_writer(
                &SessionClaimState::Live {
                    holder: "pty:short1".into()
                },
                self_holder
            ),
            None
        );
        // Free/unknown -> allow (fail-open).
        assert_eq!(
            external_session_writer(&SessionClaimState::FreeOrUnknown, self_holder),
            None
        );
        // An external writer (e.g. the relay) -> refuse BusyElsewhere{holder}.
        assert_eq!(
            external_session_writer(
                &SessionClaimState::Live {
                    holder: "relay:abc".into()
                },
                self_holder
            ),
            Some("relay:abc".to_string())
        );
    }

    /// Regression (rail -> drive freeze): the cross-process claim probe shells
    /// the `fno` Python CLI on the interactive-claude drive-open path. A slow
    /// probe must fail OPEN within `budget` rather than stall the drive open
    /// (which, unbounded, exceeds the client's 3s budget and freezes the grid on
    /// every refocus).
    #[tokio::test]
    async fn external_claude_writer_fails_open_when_probe_is_slow() {
        let script = std::env::temp_dir().join(format!("slowfno_{}.sh", std::process::id()));
        std::fs::write(&script, "#!/bin/sh\nsleep 5\necho '{}'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let start = std::time::Instant::now();
        let holder = external_claude_writer_bin(
            "some-uuid",
            "pty:x",
            script.clone().into_os_string(),
            Duration::from_millis(300),
        )
        .await;
        let elapsed = std::time::Instant::now().saturating_duration_since(start);
        let _ = std::fs::remove_file(&script);
        assert_eq!(
            holder, None,
            "a slow probe must fail open (no external writer)"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "probe was not bounded: {elapsed:?}"
        );
    }

    #[test]
    fn session_claim_lock_path_mirrors_fno_encoding() {
        let dir = Path::new("/tmp/claims");
        // A standard uuid is url-unreserved, so only the key's ':' escapes.
        assert_eq!(
            session_claim_lock_path_in(dir, "9f1c-abcd"),
            Some(dir.join("session%3A9f1c-abcd.lock"))
        );
        // A uuid carrying a byte that WOULD url-encode -> None (fall through to the
        // CLI, never a path that silently disagrees with `fno`'s encoding).
        assert_eq!(session_claim_lock_path_in(dir, "a/b"), None);
        assert_eq!(session_claim_lock_path_in(dir, "a b"), None);
        assert_eq!(session_claim_lock_path_in(dir, ""), None);
    }

    #[test]
    fn global_claims_dir_treats_empty_env_as_unset() {
        use std::ffi::OsString;
        let os = |s: &str| OsString::from(s);
        // A set root wins.
        assert_eq!(
            global_claims_dir_from(Some(os("/root")), Some(os("/home"))),
            Some(PathBuf::from("/root/.fno/claims"))
        );
        // EMPTY root falls back to home (Python's empty-is-unset), NOT a
        // cwd-relative `.fno/claims` that would blind the interlock.
        assert_eq!(
            global_claims_dir_from(Some(os("")), Some(os("/home"))),
            Some(PathBuf::from("/home/.fno/claims"))
        );
        // Empty/absent both -> None; the gate then falls through to the CLI.
        assert_eq!(global_claims_dir_from(Some(os("")), Some(os(""))), None);
        assert_eq!(global_claims_dir_from(None, None), None);
    }

    /// The gate must SKIP the CLI entirely when no claim lockfile exists (the
    /// common case: nothing else co-writing). Point it at an `fno` that would sleep
    /// past the budget; short-circuiting returns fast without ever running it.
    #[tokio::test]
    async fn gate_skips_probe_when_no_lockfile() {
        let dir = std::env::temp_dir().join(format!("noclaims_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir); // empty: no lockfile for our uuid
        let slow = std::env::temp_dir().join(format!("slowfno_g_{}.sh", std::process::id()));
        std::fs::write(&slow, "#!/bin/sh\nsleep 5\necho '{}'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let start = std::time::Instant::now();
        let holder = external_claude_writer_gated(
            "gate-uuid",
            "pty:x",
            Some(dir.clone()),
            slow.clone().into_os_string(),
            Duration::from_secs(5),
        )
        .await;
        let elapsed = std::time::Instant::now().saturating_duration_since(start);
        let _ = std::fs::remove_file(&slow);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(holder, None, "no lockfile -> no external writer");
        assert!(
            elapsed < Duration::from_secs(1),
            "gate did not skip the probe (would have blocked on the slow fno): {elapsed:?}"
        );
    }

    /// When a lockfile IS present the gate falls through to the authoritative probe
    /// (a slow stub here -> bounded fail-open), proving the gate never blinds the
    /// interlock when a claim actually exists.
    #[tokio::test]
    async fn gate_runs_probe_when_lockfile_present() {
        let dir = std::env::temp_dir().join(format!("hasclaim_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("session%3Ahas-uuid.lock"),
            "holder: other\npid: 1\n",
        )
        .unwrap();
        let slow = std::env::temp_dir().join(format!("slowfno_h_{}.sh", std::process::id()));
        std::fs::write(&slow, "#!/bin/sh\nsleep 5\necho '{}'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let start = std::time::Instant::now();
        let holder = external_claude_writer_gated(
            "has-uuid",
            "pty:x",
            Some(dir.clone()),
            slow.clone().into_os_string(),
            Duration::from_millis(300),
        )
        .await;
        let elapsed = std::time::Instant::now().saturating_duration_since(start);
        let _ = std::fs::remove_file(&slow);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(holder, None, "slow probe fails open");
        // It WAITED on the probe (~budget) rather than short-circuiting instantly,
        // which is how we know the present lockfile routed through the CLI.
        assert!(
            elapsed >= Duration::from_millis(250),
            "gate short-circuited despite a present lockfile: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "probe not bounded: {elapsed:?}"
        );
    }

    #[test]
    fn drive_mode_parse_and_authority() {
        assert_eq!(
            DriveMode::parse("interactive"),
            Some(DriveMode::Interactive)
        );
        assert_eq!(DriveMode::parse("watch"), Some(DriveMode::Watch));
        assert_eq!(DriveMode::parse("step"), Some(DriveMode::Step));
        assert_eq!(DriveMode::parse("paranoid"), Some(DriveMode::Paranoid));
        assert_eq!(DriveMode::parse("bogus"), None);

        assert!(DriveMode::Interactive.opens_authority_window());
        assert!(DriveMode::Step.opens_authority_window());
        assert!(DriveMode::Paranoid.opens_authority_window());
        assert!(!DriveMode::Watch.opens_authority_window());
        assert!(DriveMode::Watch.is_watch());
    }

    #[test]
    fn parse_control_variants() {
        assert!(matches!(
            parse_control(r#"{"t":"resize","rows":40,"cols":120}"#),
            Control::Resize(40, 120)
        ));
        assert!(matches!(parse_control(r#"{"t":"ping"}"#), Control::Ping));
        assert!(matches!(
            parse_control(r#"{"t":"detach","reason":"user_sentinel"}"#),
            Control::Detach(r) if r == "user_sentinel"
        ));
        assert!(matches!(parse_control("not json"), Control::Unknown));
        assert!(matches!(
            parse_control(r#"{"t":"resize","rows":0,"cols":0}"#),
            Control::Unknown
        ));
    }

    #[test]
    fn drive_eligibility_matches_ld28() {
        for s in [
            AgentStatus::Ready,
            AgentStatus::Idle,
            AgentStatus::Busy,
            AgentStatus::Live,
        ] {
            assert!(drive_eligible(s), "{s:?} should be drive-eligible");
        }
        for s in [
            AgentStatus::Spawning,
            AgentStatus::Restarting,
            AgentStatus::Orphaned,
            AgentStatus::Failed,
            AgentStatus::Exited,
            AgentStatus::PermanentDead,
        ] {
            assert!(!drive_eligible(s), "{s:?} should NOT be drive-eligible");
        }
    }

    /// Insert a controlling driver for `short_id` directly into the table (the
    /// daemon path goes through `admit`; these unit tests exercise the
    /// lifecycle-verb query surface in isolation).
    async fn put_controlling(
        table: &DriveTable,
        short_id: &str,
        session_id: &str,
        mode: DriveMode,
    ) {
        let mut t = table.lock().await;
        t.entry(short_id.to_string()).or_default().controlling =
            Some(DriverHandle::new(session_id.to_string(), mode));
    }

    async fn put_watcher(table: &DriveTable, short_id: &str, session_id: &str) {
        let mut t = table.lock().await;
        t.entry(short_id.to_string())
            .or_default()
            .watchers
            .push(DriverHandle::new(session_id.to_string(), DriveMode::Watch));
    }

    #[tokio::test]
    async fn controlling_driver_reports_only_the_controller() {
        let table = new_table();
        // No driver -> None; a lone watcher does NOT count as controlling.
        assert!(controlling_driver(&table, "wkA").await.is_none());
        put_watcher(&table, "wkA", "watch-1").await;
        assert!(
            controlling_driver(&table, "wkA").await.is_none(),
            "a watcher must not register as a controlling driver"
        );
        // A controlling driver is reported with its session id + mode.
        put_controlling(&table, "wkA", "drv-1", DriveMode::Interactive).await;
        let got = controlling_driver(&table, "wkA").await;
        assert_eq!(got, Some(("drv-1".to_string(), DriveMode::Interactive)));
    }

    #[tokio::test]
    async fn force_close_signals_only_when_a_driver_is_present() {
        let table = new_table();
        // No driver -> nothing to close.
        assert!(!force_close_controlling(&table, "wkA", "stop_force").await);
        put_controlling(&table, "wkA", "drv-1", DriveMode::Step).await;
        assert!(force_close_controlling(&table, "wkA", "stop_force").await);
        // The handle's close reason is attributed for the drive_detached event.
        let t = table.lock().await;
        let reason = t["wkA"]
            .controlling
            .as_ref()
            .unwrap()
            .close_reason
            .lock()
            .unwrap()
            .clone();
        assert_eq!(reason.as_deref(), Some("stop_force"));
    }

    #[tokio::test]
    async fn await_driver_cleared_times_out_when_slot_persists() {
        let table = new_table();
        put_controlling(&table, "wkA", "drv-1", DriveMode::Interactive).await;
        // Nothing clears the slot here (no session loop), so the bounded wait
        // returns false rather than hanging.
        assert!(!await_driver_cleared(&table, "wkA", Duration::from_millis(60)).await);
        // An agent with no driver is "cleared" immediately.
        assert!(await_driver_cleared(&table, "other", Duration::from_millis(60)).await);
    }

    #[tokio::test]
    async fn active_drive_count_counts_controllers_across_agents() {
        let table = new_table();
        assert_eq!(active_drive_count(&table).await, 0);
        put_controlling(&table, "wkA", "drv-a", DriveMode::Interactive).await;
        put_controlling(&table, "wkB", "drv-b", DriveMode::Step).await;
        put_watcher(&table, "wkA", "watch-1").await; // watchers excluded
        assert_eq!(active_drive_count(&table).await, 2);
    }
}
