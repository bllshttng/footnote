//! Native `fno backlog advance` - merge-triggered auto-continue dispatch.
//!
//! The Python owner this module ports (cli/src/fno/backlog/advance.py) moved
//! here wave by wave; this file carries the decision core: arming
//! precedence, the AdvanceResult contract, decision-event emission, and the
//! spawn-gate refusal vocabulary. Dispatch, lanes and join live in the
//! sibling `advance_dispatch` / `advance_join` modules; the grouped CLI
//! door lives in `advance_cli`.
//!
//! Contract anchors preserved from the Python owner:
//! - exactly one decision event per run, emitted best-effort, never raising;
//! - a dispatch decision is never an error to the host op (exit 0);
//! - fail-safe arming: any settings read failure answers disabled.

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Override knob: the highest-precedence explicit override for tests and
/// same-process force-enable/disable. Read only after the autonomy master
/// switch: a panic switch something else can bypass is not a panic switch.
pub const ENV_OVERRIDE: &str = "FNO_AUTO_CONTINUE";

/// Mirror handoff.sh / dispatch-node.sh: a 3-minute TTL bridge token covers
/// the spawn-to-worker-init boot window. TTL (not PID) liveness is what lets
/// the reservation outlive this process.
pub const DISPATCH_TTL_MS: u64 = 180_000;

pub const TRUTHY: [&str; 4] = ["1", "true", "yes", "on"];

/// Decision-event kinds (registered in cli/src/fno/events/schema.yaml).
pub const EVENT_DISPATCHED: &str = "advance_dispatched";
pub const EVENT_SKIPPED: &str = "advance_skipped";
pub const EVENT_FAILED: &str = "advance_failed";
/// One row per launch, from the spawn worker.
pub const EVENT_SPAWNED: &str = "dispatch_spawned";
/// Paired receipt (not a decision) emitted just before an advance_dispatched
/// when on_exhaustion=failover rotates off an exhausted provider.
pub const EVENT_FAILOVER: &str = "dispatch_failover";
pub const EVENT_CLAIM_OBSERVED: &str = "dispatch_claim_observed";
pub const EVENT_DEAD_FAILURE_LIMIT: &str = "dispatch_dead_failure_limit";
pub const EVENT_SELECTION_DIVERGED: &str = "dispatch_selection_diverged";
pub const EVENT_SOURCE: &str = "backlog";

/// (decision, event) pairs that are legal to construct. Guards against a
/// refactor minting a mismatched result (decision "dispatched" with
/// EVENT_SKIPPED) that would then emit the wrong event kind.
fn valid_decision_event(decision: &str, event: &str) -> bool {
    matches!(
        (decision, event),
        ("dispatched", EVENT_DISPATCHED) | ("skipped", EVENT_SKIPPED) | ("failed", EVENT_FAILED)
    )
}

/// Outcome of one advance() run. `event` is the single kind emitted.
#[derive(Debug, Clone)]
pub struct AdvanceResult {
    /// "dispatched" | "skipped" | "failed"
    pub decision: String,
    pub event: &'static str,
    /// Skip reason / failure category.
    pub reason: Option<String>,
    pub node_id: Option<String>,
    pub short_id: Option<String>,
    pub detail: Option<String>,
    /// Spawn-gate exit code behind a machine-scoped skip; None otherwise.
    pub exit_code: Option<i32>,
    /// Resolved substrate of the launch ("bg" | "thread" | "headless"); set
    /// on dispatched results only. "headless" is synchronous: the worker
    /// already ran and released its claim before this result exists.
    pub substrate: Option<String>,
    /// Spawn-seam receipt lines (`fno agents spawn: ...`) the launch printed
    /// on stderr - every axis the seam did NOT apply. Empty on a quiet spawn.
    pub notes: Vec<String>,
}

impl AdvanceResult {
    /// The constructor that mirrors the Python __post_init__ guard: an
    /// invalid (decision, event) combination is a loud failure, never a
    /// silently-wrong emitted event kind. The struct stays constructible
    /// directly for internal arms that already validated the pair.
    pub fn new(
        decision: &str,
        event: &'static str,
        reason: Option<String>,
        node_id: Option<String>,
    ) -> AdvanceResult {
        assert!(
            valid_decision_event(decision, event),
            "invalid AdvanceResult (decision, event): ({decision:?}, {event:?})"
        );
        AdvanceResult {
            decision: decision.to_string(),
            event,
            reason,
            node_id,
            short_id: None,
            detail: None,
            exit_code: None,
            substrate: None,
            notes: Vec::new(),
        }
    }

    /// The board verb's human stdout: the verdict line, then one `advance: `
    /// line per spawn-seam note, so a dropped pin is named where the
    /// dispatch was reported.
    pub fn render(&self) -> Vec<String> {
        let mut parts = vec![self.decision.clone()];
        if let Some(node) = &self.node_id {
            parts.push(node.clone());
        }
        if let Some(reason) = &self.reason {
            parts.push(format!("reason={reason}"));
        }
        if let Some(short) = &self.short_id {
            parts.push(format!("short_id={short}"));
        }
        let mut lines = vec![parts.join(" ")];
        lines.extend(self.notes.iter().map(|n| format!("advance: {n}")));
        lines
    }

    /// The board verb's --json payload; notes ride beside the verdict.
    pub fn json_receipt(&self) -> Value {
        json!({
            "decision": self.decision,
            "event": self.event,
            "reason": self.reason,
            "node_id": self.node_id,
            "short_id": self.short_id,
            "notes": self.notes,
        })
    }
}

/// Structured family-2 decision shared by every node-dispatch caller.
#[derive(Debug, Clone)]
pub struct DispatchClaimObservation {
    pub verdict: String,
    pub claim_state: Option<String>,
    pub holder: String,
    pub truth_status: String,
    pub action: String,
    pub worker: String,
    pub block_reason: Option<String>,
    pub worked_error: Option<String>,
}

impl DispatchClaimObservation {
    pub fn blocks_dispatch(&self) -> bool {
        matches!(
            self.action.as_str(),
            "blocked" | "auto-deferred" | "defer-failed"
        )
    }

    /// The action token wins when it is the more specific refusal: a node at
    /// its dead-dispatch limit that also hits a roster failure reports
    /// auto-deferred, not the authority error that merely co-occurred.
    pub fn refusal_reason(&self) -> Option<String> {
        if self.action == "auto-deferred" || self.action == "defer-failed" {
            return Some(self.action.clone());
        }
        if let Some(reason) = &self.block_reason {
            return Some(reason.clone());
        }
        if self.action == "blocked" {
            return Some("already-claimed".to_string());
        }
        if self.blocks_dispatch() {
            return Some(self.action.clone());
        }
        None
    }

    pub fn to_value(&self) -> Value {
        json!({
            "verdict": self.verdict,
            "claim_state": self.claim_state,
            "holder": self.holder,
            "truth_status": self.truth_status,
            "action": self.action,
            "worker": self.worker,
            "block_reason": self.block_reason,
            "worked_error": self.worked_error,
        })
    }
}

/// A machine-scoped refusal: raised before any node work, never the node's
/// fault.
#[derive(Debug, Clone)]
pub struct GateRefusal {
    pub reason: String,
    pub exit_code: i32,
    pub detail: String,
    pub retry_at: Option<f64>,
}

/// The seam's typed spawn failure, port of the SpawnError family. `kind`
/// splits the three Python classes: AlreadyRunning (a peer dispatcher / live
/// worker owns the launch), QueueRefused (exit 78, every configured lane
/// exhausted, carries retry_at), General (re-dispatchable failure).
#[derive(Debug, Clone)]
pub struct SpawnFailure {
    pub kind: SpawnFailureKind,
    pub message: String,
    pub exit_code: Option<i32>,
    pub detail: String,
    pub retry_at: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnFailureKind {
    AlreadyRunning,
    QueueRefused,
    General,
}

impl SpawnFailure {
    pub fn general(message: impl Into<String>) -> SpawnFailure {
        SpawnFailure {
            kind: SpawnFailureKind::General,
            message: message.into(),
            exit_code: None,
            detail: String::new(),
            retry_at: None,
        }
    }

    pub fn with_exit(mut self, exit_code: i32, detail: impl Into<String>) -> SpawnFailure {
        self.exit_code = Some(exit_code);
        self.detail = detail.into();
        self
    }

    pub fn already_running(message: impl Into<String>) -> SpawnFailure {
        SpawnFailure {
            kind: SpawnFailureKind::AlreadyRunning,
            message: message.into(),
            exit_code: None,
            detail: String::new(),
            retry_at: None,
        }
    }

    pub fn queue_refused(message: impl Into<String>, retry_at: Option<f64>) -> SpawnFailure {
        SpawnFailure {
            kind: SpawnFailureKind::QueueRefused,
            message: message.into(),
            exit_code: Some(crate::spawn_gate::EXIT_PROVIDER_CAP),
            detail: String::new(),
            retry_at,
        }
    }
}

/// spawn exit -> machine verdict. 75-80 are capacity conditions true for
/// every caller equally; 81 is a registry no spawn on this machine can pass;
/// 82 and 83 are the fleet incident pair: no spawn passes while the stop
/// stands, so they read gate-unavailable; 84 is the state-root refusal:
/// permanent, a human grants, never capacity; 85 is the sandbox probe; 86
/// and 88 are the territory and blueprint caps, which free up when a slot
/// frees; 87 is an unanswered gate.
fn gate_refusal_reason(code: i32) -> Option<&'static str> {
    use crate::spawn_gate as g;
    let sandbox_unreachable = sandbox_probe_exit();
    match code {
        g::EXIT_QUEUE_TIMEOUT
        | g::EXIT_NO_WAIT
        | g::EXIT_RAM_REFUSED
        | g::EXIT_PROVIDER_CAP
        | g::EXIT_LOAD_REFUSED
        | g::EXIT_LEAD_SHARE
        | g::EXIT_TERRITORY_CAP
        | g::EXIT_BLUEPRINT_CAP => Some("capacity-refused"),
        g::EXIT_REGISTRY_SCHEMA
        | g::EXIT_FLEET_STOP
        | g::EXIT_FLEET_STOP_UNAVAILABLE
        | g::EXIT_GATE_UNAVAILABLE => Some("gate-unavailable"),
        g::EXIT_STATE_ROOT_UNGRANTED => Some("state-root-ungranted"),
        c if c == sandbox_unreachable => Some("sandbox-unreachable"),
        _ => None,
    }
}

/// The sandbox probe's exit code lives in the Python runtime
/// (fno.agents.sandbox_probe.EXIT_SANDBOX_UNREACHABLE = 85); the native
/// probe shares the constant.
fn sandbox_probe_exit() -> i32 {
    85
}

/// The refusal sentence: the LAST `spawn-gate:` or `sandbox-probe:` line
/// (the gate warns before its verdict); the whole stderr as fallback. No
/// head window: a capped head cut a refusal mid-flag and the cause was lost.
pub fn gate_refusal_detail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let gate_lines: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| l.starts_with("spawn-gate:") || l.starts_with("sandbox-probe:"))
        .collect();
    match gate_lines.last() {
        Some(last) => (*last).to_string(),
        None => stderr.trim().to_string(),
    }
}

/// A GateRefusal for a machine-scoped gate refusal, else None. The
/// `spawn-gate:` marker (`sandbox-probe:` for the sandbox probe's exit) is
/// REQUIRED provenance: a provider crash propagates a raw exit code, so the
/// number alone cannot prove the machine refused.
pub fn gate_refusal(err: &SpawnFailure) -> Option<GateRefusal> {
    let code = err.exit_code?;
    let reason = gate_refusal_reason(code)?;
    let detail = err.detail.trim().to_string();
    let marker = if code == sandbox_probe_exit() {
        "sandbox-probe:"
    } else {
        "spawn-gate:"
    };
    if !detail.starts_with(marker) {
        return None;
    }
    Some(GateRefusal {
        reason: reason.to_string(),
        exit_code: code,
        detail,
        retry_at: err.retry_at,
    })
}

/// The `retry_at` from the seam's typed queue-refusal JSON, if any.
pub fn slot_queue_retry_at(stdout: &str) -> Option<f64> {
    for line in stdout.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(data) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if data.get("reason").and_then(Value::as_str) == Some("slot_exhausted") {
            return match data.get("retry_at") {
                Some(Value::Number(n)) => n.as_f64(),
                Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
                _ => None,
            };
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Arming precedence
// ---------------------------------------------------------------------------

/// Resolve auto-continue's armed state AND which precedence rank supplied it.
///
/// Precedence (highest first):
/// 0. `config.autonomy.enabled` master switch off -> rank "autonomy".
/// 1. `FNO_AUTO_CONTINUE` env override -> rank "env".
/// 2. `config.auto_continue.enabled` from settings.yaml (local > global) ->
///    rank "config".
/// 3. default False -> rank "default".
///
/// The rank is stamped onto every dispatch decision event so "what armed
/// this" is answerable from the log instead of by inference.
///
/// The read is infallible (the loader degrades in place), so the two
/// lower ranks answer from the merged doc exactly as Python's do.
pub fn auto_continue_resolve(project_root: Option<&Path>) -> (bool, &'static str) {
    if !crate::backlog::advance_settings::autonomy_master_enabled(project_root) {
        return (false, "autonomy");
    }
    if let Ok(env) = std::env::var(ENV_OVERRIDE) {
        // Python answers membership in the truthy set for the WHOLE value:
        // "env.strip().lower() in _TRUTHY". Mirror exactly.
        let value = env.trim().to_ascii_lowercase();
        return (TRUTHY.contains(&value.as_str()), "env");
    }
    // The Python exception arm (rank "default") has no Rust analog: the
    // settings loader degrades in place, so a completed read always
    // answers rank "config".
    (
        crate::backlog::advance_settings::auto_continue_enabled_setting(project_root),
        "config",
    )
}
/// Resolve whether auto-continue is armed for this project. Drops the rank
/// for callers that only need the boolean.
pub fn auto_continue_enabled(project_root: Option<&Path>) -> bool {
    auto_continue_resolve(project_root).0
}

// ---------------------------------------------------------------------------
// Event emission (non-fatal; exactly one per run)
// ---------------------------------------------------------------------------

/// The journal to emit into: the caller's held root, else the space journal
/// (FNO_EVENTS_PATH pin first, matching the Python project_events_json
/// chain). The None leg must never bare-join cwd.
pub fn advance_events_path(project_root: Option<&Path>) -> Option<PathBuf> {
    if let Some(pin) = std::env::var_os("FNO_EVENTS_PATH").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(pin));
    }
    match project_root {
        Some(root) => Some(root.join(".fno").join("events.jsonl")),
        None => {
            let cwd = std::env::current_dir().ok()?;
            crate::paths::space_dir_opt(&cwd).map(|space| space.join("events.jsonl"))
        }
    }
}

/// Best-effort event emit. Never panics; a failure prints one warning line.
/// The row mirrors into the tick's store (the state root's events.jsonl, or
/// the FNO_EVENTS_PATH pin), guarded separately so a dead primary journal
/// never costs the durable copy.
pub fn advance_emit(kind: &str, data: Value, events_path: Option<&Path>) {
    let target = match events_path.map(Path::to_path_buf) {
        Some(p) => Some(p),
        None => advance_events_path(None),
    };
    let Some(target) = target else {
        eprintln!("advance: WARNING: event emit failed ({kind}): no events path");
        return;
    };
    let fields = match data {
        Value::Object(map) => map,
        other => {
            let mut m = Map::new();
            m.insert("data".to_string(), other);
            m
        }
    };
    let emit_to = |path: &Path| -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", path.display()))?;
        }
        crate::events::EventEmitter::new(path, EVENT_SOURCE)
            .emit_fields(kind, fields.clone())
            .map_err(|e| e.to_string())
    };
    if let Err(e) = emit_to(&target) {
        eprintln!("advance: WARNING: event emit failed ({kind}): {e}");
    }
    if let Some(mirror) = tick_store_path() {
        if mirror != target {
            if let Err(e) = emit_to(&mirror) {
                eprintln!("advance: WARNING: event mirror emit failed ({kind}): {e}");
            }
        }
    }
}

/// The journal the arm readouts scan: the FNO_EVENTS_PATH pin when set, else
/// the state root; shared so a paired row mirrors into the same store.
fn tick_store_path() -> Option<PathBuf> {
    if let Some(pin) = std::env::var_os("FNO_EVENTS_PATH").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(pin));
    }
    let cwd = std::env::current_dir().ok()?;
    crate::agents_config::state_dir(&cwd).map(|root| root.join("events.jsonl"))
}

// ---------------------------------------------------------------------------
// Selection (advance.py selection half; owners already native)
// ---------------------------------------------------------------------------

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `config.backlog.staleness_days` (default 21), fail-open to the default
/// (the Python model default governs absent keys the same way).
pub fn guard_staleness_days(project_root: Option<&Path>) -> i64 {
    let doc = crate::backlog::advance_settings::load_merged(project_root);
    doc.get("backlog")
        .and_then(|b| b.get("staleness_days"))
        .and_then(Value::as_i64)
        .unwrap_or(21)
}

/// One bounded child run's captured output.
pub(crate) struct BoundedOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// Run a child to completion with a hard wall-clock bound, capturing text
/// stdout/stderr. On timeout the child is killed and an error names the
/// bound. Rides the crate's bounded runner (process group, pipe capping)
/// rather than its own poll loop.
pub(crate) fn bounded_command_env(
    exe: &Path,
    argv: &[String],
    bound_s: u64,
    extra_env: &[(String, String)],
) -> Result<BoundedOutput, String> {
    use std::process::Stdio;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out =
        crate::bounded_cmd::output_with_timeout_result(cmd, bound_s).map_err(|e| e.to_string())?;
    Ok(BoundedOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(1),
    })
}

/// The select-read door: one bounded exec of this binary's `select-read`
/// verb. The door's own worst case is the select bound (120s default) plus
/// the enrich second exec (30s), so the caller waits a bounded 180s.
pub fn select_read(kind: &str, args: &[String]) -> Result<Value, String> {
    let exe =
        std::env::current_exe().map_err(|_| "select-read: no fno-agents binary".to_string())?;
    let mut argv = vec!["select-read".to_string(), kind.to_string()];
    argv.extend(args.iter().cloned());
    let out = bounded_command_env(&exe, &argv, 180, &[])
        .map_err(|e| format!("select-read {kind}: {e}"))?;
    let receipt: Value = serde_json::from_str(out.stdout.trim())
        .map_err(|e| format!("select-read {kind}: unreadable receipt: {e}"))?;
    match receipt.get("status").and_then(Value::as_str) {
        Some("unmeasured") => Err(format!(
            "select-read {kind}: unmeasured: {}",
            receipt.get("detail").and_then(Value::as_str).unwrap_or("")
        )),
        Some("ok") => Ok(receipt.get("answer").cloned().unwrap_or(Value::Null)),
        _ => Err(format!(
            "select-read {kind}: {}",
            receipt
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("failed")
        )),
    }
}

static HELD_CACHE: Mutex<Option<(Instant, BTreeMap<String, String>)>> = Mutex::new(None);

/// node -> open question id via select-read held; fail-open, 30s cache.
pub fn held_questions() -> BTreeMap<String, String> {
    let mut guard = HELD_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, map)) = guard.as_ref() {
        if at.elapsed() < Duration::from_secs(30) {
            return map.clone();
        }
    }
    let map = select_read("held", &[])
        .ok()
        .and_then(|v| serde_json::from_value::<BTreeMap<String, String>>(v).ok())
        .unwrap_or_default();
    *guard = Some((Instant::now(), map.clone()));
    map
}

/// Return the next ready node summary (or None), via the select door.
/// Project-scoped. Errors on a non-zero/garbled response so advance skips
/// rather than guessing a node.
pub fn next_node(project: Option<&str>) -> Result<Option<Value>, String> {
    let args: Vec<String> = match project {
        Some(p) => vec!["--project".to_string(), p.to_string()],
        None => Vec::new(),
    };
    select_read("next", &args).map(Some)
}

/// Read the independent planned-unclaimed observer receipt.
pub fn undispatched_nodes(project: Option<&str>, mission: Option<&str>) -> Result<Value, String> {
    let mut args: Vec<String> = Vec::new();
    if let Some(p) = project {
        args.push("--project".to_string());
        args.push(p.to_string());
    }
    if let Some(m) = mission {
        args.push("--mission".to_string());
        args.push(m.to_string());
    }
    let receipt = select_read("undispatched", &args)?;
    let ok = receipt.get("status").and_then(Value::as_str) == Some("ok");
    let scanned = receipt
        .get("entries_scanned")
        .and_then(Value::as_i64)
        .is_some();
    let rows = receipt.get("rows").and_then(Value::as_array).is_some();
    if !(ok && scanned && rows) {
        return Err("fno backlog undispatched returned an unreadable receipt".to_string());
    }
    Ok(receipt)
}

/// Reapply selector-only safety guards before lane-fill recovery.
pub fn dispatch_safe_observer(receipt: &Value) -> Result<Value, String> {
    let rows = match receipt.get("rows").and_then(Value::as_array) {
        Some(rows) if !rows.is_empty() => rows.clone(),
        _ => return Ok(receipt.clone()),
    };
    let graph_path = super::settings::graph_path();
    let entries = crate::graph_store::read_rows_strict(&graph_path)
        .map_err(|e| format!("observer safety graph unreadable: {e}"))?;
    let by_id: BTreeMap<String, Value> = entries
        .into_iter()
        .filter_map(|mut e| {
            let id = e.get("id").and_then(Value::as_str)?.to_string();
            Some((id, std::mem::take(&mut e)))
        })
        .collect();
    let now_ms = now_ms_i64();
    let staleness_days = guard_staleness_days(None);
    let held = held_questions();
    let mut safe: Vec<Value> = Vec::new();
    for row in &rows {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(entry) = by_id.get(id) else {
            continue;
        };
        let facts = row.get("facts").cloned().unwrap_or(Value::Null);
        let Some(facts) = facts.as_object() else {
            return Err(format!("observer row {id:?} has no predicate facts"));
        };
        if facts
            .get("has_pr")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || facts
                .get("batch_owner")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            || facts
                .get("completed")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            continue;
        }
        if crate::backlog_ready::selection_guards(entry, &by_id, now_ms, staleness_days, &held)
            .is_some()
        {
            continue;
        }
        safe.push(row.clone());
    }
    let mut out = receipt.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("rows".to_string(), Value::Array(safe));
    }
    Ok(out)
}

pub(crate) fn now_ms_i64() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Claim observation (family-2 pre-dispatch verdict)
// ---------------------------------------------------------------------------

/// `walker:<canonical_repo_root>` - byte-identical to the key the Rust loop
/// runtime writes for walker-scoped claims.
pub fn walker_key() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    crate::paths::canonical_repo_root(&cwd)
        .map(|root| format!("walker:{}", root.display()))
        .or_else(|| Some(format!("walker:{}", cwd.display())))
}

/// True iff `holder` is THIS session's identity (any claim state). Env ids
/// first, then the resolved ambient identity, then the pid arm (skipped for
/// pid_unavailable claims - they carry no pid to compare).
fn holder_is_ours(holder: &str, record: Option<&crate::claims::ClaimRecord>) -> bool {
    for env_var in ["TARGET_SESSION_ID", "CODEX_THREAD_ID"] {
        if let Ok(own) = std::env::var(env_var) {
            if !own.is_empty() && holder == format!("target-session:{own}") {
                return true;
            }
        }
    }
    let (session, _harness) = crate::claims::resolve_identity();
    if let Some(sid) = session {
        if !sid.is_empty() && holder == format!("target-session:{sid}") {
            return true;
        }
    }
    let Some(record) = record else { return false };
    if record.pid_unavailable {
        return false;
    }
    match record.pid {
        Some(pid) => pid == std::process::id() as i32,
        None => false,
    }
}

/// `(verdict, info)` for `node:<id>` from THIS session's view; verdict in
/// {ours, foreign_live, dead_predecessor, free}. Read-only, never fails the
/// caller: a probe failure reads as free (a re-acquire candidate).
pub fn classify_node_claim(
    node_id: &str,
    info: Option<crate::claims::ClaimRecord>,
) -> (String, Option<crate::claims::ClaimRecord>) {
    let record = match info {
        Some(r) => Some(r),
        None => match crate::claims::status(&format!("node:{node_id}"), None) {
            (crate::claims::ClaimState::Free, None) => None,
            (state, rec @ Some(_)) => {
                use crate::claims::ClaimState::*;
                match state {
                    Free | Corrupted => None,
                    _ => rec,
                }
            }
            _ => None,
        },
    };
    let Some(record) = record else {
        return ("free".to_string(), None);
    };
    let state = record_state(&record);
    if state.is_empty() || state == "free" {
        return ("free".to_string(), Some(record));
    }
    if holder_is_ours(&record.holder, Some(&record)) {
        return ("ours".to_string(), Some(record));
    }
    if state == "live" || state == "suspect" {
        return ("foreign_live".to_string(), Some(record));
    }
    ("dead_predecessor".to_string(), Some(record))
}

fn record_state(record: &crate::claims::ClaimRecord) -> String {
    match crate::claims::status(&record.key, None).0 {
        crate::claims::ClaimState::Free => "free".to_string(),
        crate::claims::ClaimState::Live => "live".to_string(),
        crate::claims::ClaimState::Suspect => "suspect".to_string(),
        crate::claims::ClaimState::Stale => "stale".to_string(),
        crate::claims::ClaimState::Corrupted => "corrupted".to_string(),
    }
}

/// The worked overlay for one node, or the roster outage. A full read per
/// call is the Python shape; the callers that batch pass a pre-read map.
pub fn live_worked_overlay() -> Result<BTreeMap<String, Vec<String>>, String> {
    let graph = super::settings::graph_path();
    let entries = crate::graph_store::read_rows_strict(&graph)
        .map_err(|_| "the graph is unreadable".to_string())?;
    Ok(crate::backlog::worked::live_worked_node_ids(&entries)?
        .into_iter()
        .collect())
}

/// The family-2 pre-dispatch verdict shared by every node-dispatch caller.
#[allow(clippy::too_many_arguments)]
pub fn observe_node_claim(
    node_id: &str,
    node_cwd: Option<&str>,
    enforce_failure_limit: bool,
    emit_events: bool,
    native_info: Option<crate::claims::ClaimRecord>,
    worked_nodes: Option<BTreeMap<String, Vec<String>>>,
    worked_error: Option<String>,
    events_path: Option<&Path>,
) -> DispatchClaimObservation {
    let (verdict, info) = classify_node_claim(node_id, native_info);
    let info_ref = info.as_ref();
    // Truth is diagnostic; the claim verdict is authority. An unreadable
    // truth read degrades to unknown, never to a false claim verdict.
    let truth_status = resolve_truth_status_word(node_id);
    let claim_state = info_ref.map(|r| record_state(r)).filter(|s| !s.is_empty());
    let holder = info_ref
        .map(|r| r.holder.clone())
        .unwrap_or_else(|| "unknown".to_string());
    let default_occupied = verdict == "ours" || verdict == "foreign_live";
    let mut occupied = default_occupied;
    let mut worker = String::new();
    let mut worked_nodes = worked_nodes;
    let mut worked_error = worked_error;
    if worked_nodes.is_none() && worked_error.is_none() {
        match live_worked_overlay() {
            Ok(map) => worked_nodes = Some(map),
            Err(e) => worked_error = Some(e),
        }
    }
    if let Some(map) = &worked_nodes {
        if let Some(workers) = map.get(node_id) {
            if !workers.is_empty() {
                occupied = true;
                worker = workers.join(", ");
            }
        }
    }
    // Occupancy outranks the outage; the branch keeps block_reason bound.
    let mut block_reason = if worked_error.is_some() && !occupied {
        Some("worked-authority-unavailable".to_string())
    } else {
        None
    };
    if occupied {
        // task: `blocked`/`already-claimed` starved auto_continue for 97
        // minutes once; name what was consulted and what it found.
        let mut parts: Vec<String> = Vec::new();
        if let Some(state) = &claim_state {
            if state == "live" || state == "suspect" {
                parts.push(format!("claim {state} held by {holder}"));
            }
        }
        if !worker.is_empty() {
            parts.push(format!("worked overlay: {worker}"));
        }
        if !parts.is_empty() {
            block_reason = Some(format!("held: {}", parts.join("; ")));
        }
    }
    let dead_action = if occupied || !enforce_failure_limit {
        None
    } else {
        refuse_repeated_dead_dispatch(node_id, node_cwd)
    };
    let action = if occupied {
        "blocked".to_string()
    } else if let Some(dead) = dead_action {
        dead
    } else if worked_error.is_some() {
        "blocked".to_string()
    } else if verdict == "dead_predecessor" {
        "redispatch".to_string()
    } else {
        "dispatch".to_string()
    };

    if emit_events {
        let mut data = json!({
            "node_id": node_id,
            "claim_verdict": verdict,
            "claim_state": claim_state,
            "holder": holder,
            "truth_status": truth_status,
            "action": action,
        });
        if let Some(obj) = data.as_object_mut() {
            if !worker.is_empty() {
                obj.insert("worker".to_string(), Value::from(worker.clone()));
            }
            if let Some(reason) = &block_reason {
                obj.insert("block_reason".to_string(), Value::from(reason.clone()));
            }
            if let Some(err) = &worked_error {
                obj.insert("worked_error".to_string(), Value::from(err.clone()));
            }
        }
        advance_emit(EVENT_CLAIM_OBSERVED, data, events_path);
    }
    if emit_events {
        if let Some(state) = &claim_state {
            if state == "stale" || state == "suspect" {
                // Lead with the worker when one is on the node: this line
                // pointed at a stale claim while the row was the occupant,
                // and an operator followed it to a claim that read UNCLAIMED.
                let lead = if worker.is_empty() {
                    String::new()
                } else {
                    format!("worker row {worker} is on the node; ")
                };
                let message = format!(
                    "dispatch {action} for {node_id}: {lead}node claim is {state}, \
prior holder={holder}, truth_status={truth_status}"
                );
                eprintln!("advance: WARNING: {message}");
                crate::operator_notice::notify_operator(
                    "footnote: contested node dispatch",
                    &message,
                    None,
                );
            }
        }
    }
    DispatchClaimObservation {
        verdict,
        claim_state,
        holder,
        truth_status,
        action,
        worker,
        block_reason,
        worked_error,
    }
}

/// The truth row's state word, or "unknown" when nothing answers. The claim
/// verdict is authority; truth is diagnostic. Native derivation over the
/// node claim + the newest loop_check fire per session (30-minute recency).
fn resolve_truth_status_word(node_id: &str) -> String {
    let claim_key = format!("node:{node_id}");
    let (state, record) = crate::claims::status(&claim_key, None);
    let holder = record
        .as_ref()
        .map(|r| r.holder.clone())
        .unwrap_or_default();
    let sid = holder
        .strip_prefix("target-session:")
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    match state {
        crate::claims::ClaimState::Stale => "stalled".to_string(),
        crate::claims::ClaimState::Suspect => "suspect".to_string(),
        crate::claims::ClaimState::Live => {
            let age = sid.as_deref().and_then(loop_check_age);
            match age {
                Some(age) if age <= 1800.0 => "working".to_string(),
                _ => "waiting".to_string(),
            }
        }
        _ => "unknown".to_string(),
    }
}

/// Seconds since the newest loop_check fire for `sid`, from a bounded tail
/// of the state root's events journal. None when nothing fired.
fn loop_check_age(sid: &str) -> Option<f64> {
    let path = std::env::var_os("FNO_EVENTS_PATH")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let cwd = std::env::current_dir().ok()?;
            crate::agents_config::state_dir(&cwd).map(|root| root.join("events.jsonl"))
        })?;
    let Ok(meta) = std::fs::metadata(&path) else {
        return None;
    };
    let size = meta.len();
    let start = size.saturating_sub(256 * 1024);
    let mut file = std::fs::File::open(&path).ok()?;
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    let now = now_ms_i64() as f64 / 1000.0;
    let mut newest: Option<f64> = None;
    for line in text.lines().rev() {
        let line = line.trim();
        if line.is_empty() || !line.contains("\"loop_check\"") {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if rec.get("type").and_then(Value::as_str) != Some("loop_check") {
            continue;
        }
        let row_sid = rec
            .get("data")
            .and_then(|d| d.get("session_id"))
            .and_then(Value::as_str)?;
        if row_sid != sid {
            continue;
        }
        let ts = rec.get("ts").and_then(Value::as_str)?;
        let epoch = rfc3339_to_epoch(ts)?;
        newest = Some(now - epoch);
        break;
        // newest fire per session is always near the end (append-ordered)
    }
    newest
}

/// RFC3339 `ts` -> epoch seconds; None when unparsable.
fn rfc3339_to_epoch(ts: &str) -> Option<f64> {
    let dt = chrono::DateTime::parse_from_rfc3339(ts.trim()).ok()?;
    Some(dt.timestamp_millis() as f64 / 1000.0)
}

/// One pre-birth decision for node ownership plus boot reservation.
pub fn node_dispatch_block_reason(
    node_id: &str,
    node_cwd: Option<&str>,
    worked_nodes: Option<BTreeMap<String, Vec<String>>>,
    worked_error: Option<String>,
    events_path: Option<&Path>,
) -> Option<String> {
    let observation = observe_node_claim(
        node_id,
        node_cwd,
        true,
        true,
        None,
        worked_nodes,
        worked_error,
        events_path,
    );
    if observation.blocks_dispatch() {
        return observation.refusal_reason();
    }
    if claim_is_live(&format!("dispatch:{node_id}")) {
        return Some("already-claimed".to_string());
    }
    None
}

/// Live OR suspect blocks selection: suspect is TTL-unexpired but dead pid
/// (respawned worker); the TTL still protects the slot, so selection must
/// skip it, never steal.
pub fn claim_is_live(key: &str) -> bool {
    match crate::claims::status(key, None) {
        (crate::claims::ClaimState::Live, _) | (crate::claims::ClaimState::Suspect, _) => true,
        _ => false,
    }
}

/// Release a claim, swallowing any error. Called on the spawn-failure path
/// BEFORE the decision event is emitted, so a raising release would lose the
/// decision event and leak the reservation. Truly non-raising keeps "exactly
/// one decision event, always" an invariant.
pub fn safe_release(key: &str, holder: &str) {
    let _ = crate::claims::release(key, holder, None, None);
}

/// Auto-defer at the durable failure limit; return the refusal action
/// ("auto-deferred" | "defer-failed"). Reads the failure streak from the
/// merged journals (state root, agents-home mirror, and the node's own
/// project journal), defers via `fno backlog defer` past the limit, and
/// notifies.
pub fn refuse_repeated_dead_dispatch(node_id: &str, node_cwd: Option<&str>) -> Option<String> {
    let failure_limit = crate::backlog::advance_settings::load_merged(
        node_cwd.map(std::path::Path::new),
    )
    .get("active_backlog")
    .and_then(|a| a.get("failure_limit"))
    .and_then(Value::as_i64)
    .unwrap_or(3);
    let events = read_failure_events(node_cwd);
    let streak = consecutive_failures(node_id, &events);
    if streak < failure_limit {
        return None;
    }
    let reason = format!(
        "auto-failure: {streak} consecutive dead dispatches (worker reaped without termination)"
    );
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return Some("defer-failed".to_string()),
    };
    let mut argv = vec![
        "backlog".to_string(),
        "defer".to_string(),
        node_id.to_string(),
        "--reason".to_string(),
        reason,
    ];
    if let Some(cwd) = node_cwd {
        argv.push("--cwd".to_string());
        argv.push(cwd.to_string());
    }
    let out = bounded_command_env(&exe, &argv, 300, &[]);
    let code = out.as_ref().map(|o| o.code).unwrap_or(1);
    let action = if code == 0 {
        "auto-deferred"
    } else {
        "defer-failed"
    };
    advance_emit(
        EVENT_DEAD_FAILURE_LIMIT,
        json!({
            "node_id": node_id,
            "consecutive_failures": streak,
            "failure_limit": failure_limit,
            "action": action,
        }),
        None,
    );
    crate::operator_notice::notify_operator(
        "footnote: dead dispatch limit",
        &format!("{node_id}: {streak} dead dispatches; {action}. No worker launched."),
        None,
    );
    if code != 0 {
        let detail = out
            .ok()
            .map(|o| {
                let raw = if o.stderr.is_empty() {
                    &o.stdout
                } else {
                    &o.stderr
                };
                raw.trim().chars().take(200).collect::<String>()
            })
            .unwrap_or_default();
        eprintln!(
            "dead-dispatch limit reached for {node_id}; defer failed (exit {code}: {detail}); refusing another worker"
        );
    }
    Some(action.to_string())
}

/// The failure-event types the dead-dispatch streak reads. Mirrors
/// fno.graph.failure.FAILURE_EVENT_TYPES (the walker's `node_*` envelopes
/// plus `advance_failed`, which rides through unclassified).
pub(crate) const FAILURE_EVENT_TYPES: &[&str] = &[
    "node_failed",
    "node_undeferred",
    "node_closed",
    "advance_failed",
];

/// The store-side typed read (fno.graph.failure.read_events's
/// `query_rows(target, types=...)` leg): the SQL store beside the journal
/// answers in commit order; when no store exists yet the journal lines
/// themselves are the same rows, oldest first.
fn read_events_types(journal: &Path, types: &[&str]) -> Vec<Value> {
    let query = crate::event_store::EventQuery::of_types(types);
    if let Ok(rows) = crate::event_store::query_events(journal, &query) {
        return rows
            .iter()
            .filter_map(|r| serde_json::from_str::<Value>(&r.line).ok())
            .collect();
    }
    let Ok(text) = std::fs::read_to_string(journal) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

/// Union mirrored histories by occurrence count and timestamp order
/// (fno.graph.failure.merge_event_histories): a row already seen at its
/// highest multiplicity in an earlier history is not repeated, and the merge
/// sorts by (ts, insertion) so the window walk reads newest first.
fn merge_event_histories(histories: Vec<Vec<Value>>) -> Vec<Value> {
    let mut merged: Vec<(usize, Value)> = Vec::new();
    let mut max_counts: BTreeMap<String, usize> = BTreeMap::new();
    for history in &histories {
        let mut local_counts: BTreeMap<String, usize> = BTreeMap::new();
        for rec in history {
            let key = serde_json::to_string(rec).unwrap_or_default();
            let count = local_counts.entry(key.clone()).or_insert(0);
            *count += 1;
            if *count > max_counts.get(&key).copied().unwrap_or(0) {
                merged.push((merged.len(), rec.clone()));
            }
        }
        for (key, count) in local_counts {
            let max = max_counts.entry(key).or_insert(0);
            if count > *max {
                *max = count;
            }
        }
    }
    merged.sort_by(|a, b| {
        let ts_a = a.1.get("ts").and_then(Value::as_str).unwrap_or("");
        let ts_b = b.1.get("ts").and_then(Value::as_str).unwrap_or("");
        (ts_a, a.0).cmp(&(ts_b, b.0))
    });
    merged.into_iter().map(|(_, rec)| rec).collect()
}

/// The journal set the Python refuse leg reads: the state-root journal and
/// the Rust agents-home mirror (fno.graph.failure._default_event_paths,
/// deduped preserving order), then the node's own project journal when it
/// exists.
fn read_failure_events(node_cwd: Option<&str>) -> Vec<Value> {
    let mut journals: Vec<PathBuf> = Vec::new();
    let cwd = std::env::current_dir().unwrap_or_default();
    if let Some(root) = crate::agents_config::state_dir(&cwd) {
        journals.push(root.join("events.jsonl"));
    }
    if let Some(parent) = crate::paths::AgentsHome::from_env_opt().and_then(|h| {
        h.root().parent().map(std::path::Path::to_path_buf)
    }) {
        journals.push(parent.join("events.jsonl"));
    }
    journals.dedup();
    if let Some(cwd) = node_cwd {
        let project = Path::new(cwd).join(".fno").join("events.jsonl");
        if project.is_file() {
            journals.push(project);
        }
    }
    let histories = journals
        .iter()
        .map(|j| read_events_types(j, FAILURE_EVENT_TYPES))
        .collect();
    merge_event_histories(histories)
}

/// One classified streak signal (fno.graph.failure._classify): the walker's
/// `node_*` envelopes and the flat agents shape both carry the node id under
/// `unit_id` / `node_id` / `graph_node_id`.
#[derive(Clone, Copy, PartialEq)]
enum StreakSignal {
    Fail,
    Reset,
}

fn classify_streak_row(raw: &Value) -> Option<(String, StreakSignal)> {
    let etype = raw
        .get("type")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .or_else(|| raw.get("kind").and_then(Value::as_str))
        .unwrap_or("");
    let data = match raw.get("data") {
        Some(d) if d.is_object() => d,
        _ => raw,
    };
    let node_id = ["unit_id", "node_id", "graph_node_id"]
        .iter()
        .find_map(|k| {
            data.get(*k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })?;
    let signal = match etype {
        "node_failed" => StreakSignal::Fail,
        "node_undeferred" => StreakSignal::Reset,
        "node_closed" => match data.get("close").and_then(Value::as_str) {
            Some("parked") => StreakSignal::Fail,
            Some("closed") => StreakSignal::Reset,
            _ => return None,
        },
        _ => return None,
    };
    Some((node_id.to_string(), signal))
}

/// Consecutive failure events for THIS node since the most recent reset
/// boundary (a success close or an undefer), scanning newest -> oldest
/// (fno.graph.failure.consecutive_failures). Unclassified rows and other
/// nodes' rows neither inflate nor reset the streak.
fn consecutive_failures(node_id: &str, events: &[Value]) -> i64 {
    let mut streak: i64 = 0;
    for raw in events.iter().rev() {
        if let Some((event_node, signal)) = classify_streak_row(raw) {
            if event_node != node_id {
                continue;
            }
            match signal {
                StreakSignal::Reset => break,
                StreakSignal::Fail => streak += 1,
            }
        }
    }
    streak
}

// ---------------------------------------------------------------------------
// Cross-project continuation (advance_dependents)
// ---------------------------------------------------------------------------

/// Ready, direct `blocked_by` dependents of the closed node. Reads the graph
/// (statuses recompute at read), so a dependent whose only open blocker was
/// the just-closed node already reads ready here. Returns minimal dicts.
pub fn direct_dependents(
    closed_node_id: &str,
    closed_project: Option<&str>,
) -> Result<Vec<Value>, String> {
    let graph = super::settings::graph_path();
    let entries = crate::graph_store::read_rows(&graph).map_err(|e| e.to_string())?;
    // Containers are never dispatched as workers: a dependent that is itself
    // some other node's parent is an epic, and /target builds its leaves, not
    // the box. The container id set is the one implementation `next` uses.
    let parent_ids = container_ids(&entries);
    let by_id: BTreeMap<String, Value> = entries
        .iter()
        .filter_map(|e| {
            e.get("id")
                .and_then(Value::as_str)
                .map(|id| (id.to_string(), e.clone()))
        })
        .collect();
    let staleness_days = guard_staleness_days(None);
    let now_ms = now_ms_i64();
    let held = held_questions();
    let mut out: Vec<Value> = Vec::new();
    for e in &entries {
        let blocked_by = match e.get("blocked_by").and_then(Value::as_array) {
            Some(rows) => rows
                .iter()
                .filter_map(Value::as_str)
                .any(|b| b == closed_node_id),
            None => false,
        };
        if !blocked_by {
            continue;
        }
        // "now-unblocked" == ready OR a plan-less idea. The stored status is
        // the honest readiness: the derived view still reads blocked from
        // the blocked_by edge the close just satisfied.
        let status = e
            .get("persisted_status")
            .or_else(|| e.get("status"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let cold = crate::backlog_ready::is_cold_dispatchable(e);
        if status != "ready" && !cold {
            continue;
        }
        if crate::backlog_ready::selection_guards(e, &by_id, now_ms, staleness_days, &held)
            .is_some()
        {
            continue; // dead-ancestor or stale-quarantine - do not revive
        }
        // An in-flight PR (pr_number set, not yet merged-and-closed) still
        // reads ready because completed_at is only set at close; the project
        // next path excludes these via the unmerged-PR guard - mirror it.
        let has_pr = e.get("pr_number").map(|v| !v.is_null()).unwrap_or(false);
        let closed = e.get("completed_at").map(|v| !v.is_null()).unwrap_or(false);
        if has_pr && !closed {
            continue;
        }
        let Some(node_id) = e.get("id").and_then(Value::as_str) else {
            continue;
        };
        if parent_ids.contains(node_id) {
            continue; // epic/container dependent - build its leaves, not the box
        }
        out.push(json!({
            "id": node_id,
            "project": e.get("project"),
            "slug": e.get("slug").or_else(|| e.get("title")),
            "cwd": e.get("cwd"),
            "model": e.get("model"),
            "difficulty": e.get("difficulty"),
            "dispatch_verb": e.get("dispatch_verb"),
            "cross_project": e.get("project").and_then(Value::as_str).or(None)
                != closed_project.or(None),
        }));
    }
    Ok(out)
}

/// Ids of container nodes: an owner of only contained children is a delivery
/// unit, not a box; a node that is some other node's parent is. Mirrors the
/// container set the next picker applies so this path cannot drift.
fn container_ids(entries: &[Value]) -> BTreeSet<String> {
    let mut parents: BTreeSet<String> = BTreeSet::new();
    for e in entries {
        if let Some(p) = e.get("parent").and_then(Value::as_str) {
            if !p.is_empty() {
                parents.insert(p.to_string());
            }
        }
    }
    parents
}

use std::collections::BTreeSet;

/// Best-effort graph->doc projection for the given ids (unblocked
/// dependents). Never fails the caller: convergence, never a dispatch
/// blocker.
pub fn project_unblocked(node_ids: &[String]) {
    if node_ids.is_empty() {
        return;
    }
    // The projection owner is the plan projection pass; native callers reach
    // it through the client verb so a plan-doc renderer keeps one owner.
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let argv = vec![
        "plan-project".to_string(),
        serde_json::to_string(node_ids).unwrap_or_default(),
    ];
    if let Err(e) = bounded_command_env(&exe, &argv, 120, &[]) {
        eprintln!("warning: unblocked-dependent projection failed: {e}");
    }
}

/// True when the DEPENDENT project's own walker is live. Its
/// `walker:<root>` claim lives under that root's claims store, which is a
/// different claims root from this process's, so check it there explicitly.
/// Live OR suspect: a suspect walker claim is still an occupied lane.
pub fn walker_live_at(project_root: &str) -> bool {
    let key = format!("walker:{project_root}");
    let root = std::path::PathBuf::from(project_root);
    matches!(
        crate::claims::status(&key, Some(&root)),
        (crate::claims::ClaimState::Live, _) | (crate::claims::ClaimState::Suspect, _)
    )
}

/// The pre-spawn refusal reason for one child, or None to dispatch: a live
/// walker in the target repo, then the node-claim liveness gate. The
/// --explain preview runs the SAME classifier so it cannot describe a
/// selection the drain would not make.
pub fn converge_gate(child: &Value, root: &str) -> Option<String> {
    if walker_live_at(root) {
        return Some("walker-live".to_string());
    }
    let id = child.get("id").and_then(Value::as_str)?;
    node_dispatch_block_reason(id, Some(root), None, None, None)
}

/// The one shared converge-dispatch core: dedup, reserve, spawn, one
/// receipt. Merge-advance's per-dependent dispatch and the epic fan-out run
/// the IDENTICAL choreography so they can never fork. Emits exactly one
/// decision event; never fails the caller: a spawn failure releases the
/// reservation (node stays re-dispatchable) and resolves to failed.
#[allow(clippy::too_many_arguments)]
pub fn converge_one(
    node_meta: &Value,
    root: &str,
    ev_path: Option<&Path>,
    verbose: bool,
    cross_project: bool,
    mission: Option<&str>,
    closed_node_id: Option<&str>,
    model: Option<&str>,
    provider: Option<&str>,
    rank: Option<&str>,
    source: Option<&str>,
) -> AdvanceResult {
    let node_id = node_meta
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let slug = node_meta
        .get("slug")
        .or_else(|| node_meta.get("title"))
        .and_then(Value::as_str);

    let tag = |mut data: Value| -> Value {
        if let Some(obj) = data.as_object_mut() {
            if let Some(closed) = closed_node_id {
                obj.insert("closed_node_id".to_string(), Value::from(closed));
            }
            if let Some(mission) = mission {
                obj.insert("mission".to_string(), Value::from(mission));
            }
            if let Some(rank) = rank {
                obj.insert("rank".to_string(), Value::from(rank));
            }
        }
        data
    };
    let skip = |reason: &str,
                detail: Option<String>,
                retry_at: Option<f64>,
                exit_code: Option<i32>,
                data: Value|
     -> AdvanceResult {
        let mut payload = tag(data);
        if let Some(obj) = payload.as_object_mut() {
            if let Some(r) = retry_at {
                obj.insert("retry_at".to_string(), json!(r));
            }
            if let Some(c) = exit_code {
                obj.insert("exit_code".to_string(), json!(c));
            }
            if let Some(d) = &detail {
                obj.insert(
                    "detail".to_string(),
                    Value::from(d.chars().take(200).collect::<String>()),
                );
            }
        }
        advance_emit(EVENT_SKIPPED, payload, ev_path);
        AdvanceResult {
            decision: "skipped".to_string(),
            event: EVENT_SKIPPED,
            reason: Some(reason.to_string()),
            node_id: Some(node_id.clone()),
            short_id: None,
            detail,
            exit_code,
            substrate: None,
            notes: Vec::new(),
        }
    };
    let failed = |error: &str, data: Value| -> AdvanceResult {
        let mut payload = tag(data);
        if let Some(obj) = payload.as_object_mut() {
            obj.insert(
                "error".to_string(),
                Value::from(error.chars().take(400).collect::<String>()),
            );
        }
        advance_emit(EVENT_FAILED, payload, ev_path);
        AdvanceResult {
            decision: "failed".to_string(),
            event: EVENT_FAILED,
            reason: Some("spawn-failed".to_string()),
            node_id: Some(node_id.clone()),
            short_id: None,
            detail: Some(error.to_string()),
            exit_code: None,
            substrate: None,
            notes: Vec::new(),
        }
    };

    // The pre-spawn gates, shared with the --explain preview.
    if let Some(gate) = converge_gate(node_meta, root) {
        return skip(
            &gate,
            None,
            None,
            None,
            json!({"reason": gate, "node_id": node_id}),
        );
    }

    let dispatch_key = format!("dispatch:{node_id}");
    let holder = format!("advance:{}", std::process::id());
    let reason_tail = {
        let mut tail = String::from("converge dispatch");
        if let Some(m) = mission {
            tail.push_str(&format!(" (mission {m})"));
        }
        if let Some(c) = closed_node_id {
            tail.push_str(&format!(" (dep of {c})"));
        }
        tail.push_str(&format!(" for {node_id}"));
        tail
    };
    match crate::claims::acquire(
        &dispatch_key,
        &holder,
        crate::claims::AcquireOpts {
            ttl_ms: Some(DISPATCH_TTL_MS as i64),
            reason: Some(reason_tail),
            ..Default::default()
        },
    ) {
        crate::claims::AcquireOutcome::Acquired(_) => {}
        crate::claims::AcquireOutcome::Error(e) => {
            return skip(
                "claim-error",
                Some(e),
                None,
                None,
                json!({"reason": "claim-error", "node_id": node_id}),
            );
        }
        _ => {
            return skip(
                "already-claimed",
                None,
                None,
                None,
                json!({"reason": "already-claimed", "node_id": node_id}),
            )
        }
    }

    // Reserve-to-outcome span: every exit that is not a dispatch returns the
    // boot-window reservation, so nothing between acquire and the dispatched
    // receipt can strand the bridge.
    let spawn_outcome = super::advance_dispatch::spawn_worker(
        &node_id,
        root,
        slug,
        node_meta,
        model,
        provider,
        None,
        None,
        None,
        None,
        Some((dispatch_key.as_str(), holder.as_str())),
        "_converge_one",
        source,
        ev_path,
    );
    let (short_id, spawn_receipt) = match spawn_outcome {
        Ok(v) => v,
        Err(super::advance_dispatch::SpawnOutcome::AlreadyRunning(msg)) => {
            safe_release(&dispatch_key, &holder);
            return skip(
                "already-claimed",
                Some(msg),
                None,
                None,
                json!({"reason": "already-claimed", "node_id": node_id}),
            );
        }
        Err(super::advance_dispatch::SpawnOutcome::Failed(err)) => {
            safe_release(&dispatch_key, &holder);
            if let Some(refusal) = err.gate_refusal() {
                return skip(
                    &refusal.reason,
                    Some(refusal.detail.clone()),
                    refusal.retry_at,
                    Some(refusal.exit_code),
                    json!({"reason": refusal.reason, "node_id": node_id}),
                );
            }
            return failed(&err.message, json!({"node_id": node_id}));
        }
    };

    let mut dispatched_data = tag(json!({
        "node_id": node_id,
        "short_id": short_id,
        "agent_name": spawn_receipt.get("agent_name").cloned().unwrap_or_default(),
        "cross_project": cross_project,
        "verb": spawn_receipt.get("verb").cloned().unwrap_or(Value::from("builtin")),
        "verb_source": spawn_receipt.get("verb_source").cloned().unwrap_or(Value::from("field-absent")),
        "notes": spawn_receipt.get("notes").cloned().unwrap_or(Value::Array(vec![])),
    }));
    if let Some(obj) = dispatched_data.as_object_mut() {
        if let Some(brief) = spawn_receipt.get("brief") {
            obj.insert("brief".to_string(), brief.clone());
        }
    }
    advance_emit(EVENT_DISPATCHED, dispatched_data, ev_path);
    if verbose {
        let scope = mission.map(|m| format!("mission {m} ")).unwrap_or_default();
        let kind = if cross_project {
            "cross-project"
        } else {
            "same-project"
        };
        eprintln!(
            "advance: dispatched {scope}{kind} {node_id} -> target worker {short_id} (--cwd {root})"
        );
    }
    AdvanceResult {
        decision: "dispatched".to_string(),
        event: EVENT_DISPATCHED,
        reason: None,
        node_id: Some(node_id),
        short_id: Some(short_id),
        detail: None,
        exit_code: None,
        substrate: spawn_receipt
            .get("substrate")
            .and_then(Value::as_str)
            .map(str::to_string),
        notes: spawn_receipt
            .get("notes")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Resolve one dependent's own project root, then converge-dispatch it. A
/// CROSS-project dependent launches in its work-map root; a SAME-project
/// one launches in its own recorded cwd route (never a foreign root, which
/// could land it on a protected branch where the bg worker dies).
#[allow(clippy::too_many_arguments)]
pub fn dispatch_one_dependent(
    dep: &Value,
    closed_node_id: &str,
    ev_path: Option<&Path>,
    verbose: bool,
    model: Option<&str>,
    provider: Option<&str>,
    rank: Option<&str>,
    source: Option<&str>,
) -> AdvanceResult {
    let node_id = dep.get("id").and_then(Value::as_str).unwrap_or("");
    let cross_project = dep
        .get("cross_project")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let skip = |reason: &str, detail: Option<String>| -> AdvanceResult {
        let mut data = json!({
            "reason": reason,
            "node_id": node_id,
            "closed_node_id": closed_node_id,
        });
        if let (Some(obj), Some(rank)) = (data.as_object_mut(), rank) {
            obj.insert("rank".to_string(), Value::from(rank));
        }
        if let (Some(obj), Some(d)) = (data.as_object_mut(), &detail) {
            obj.insert(
                "detail".to_string(),
                Value::from(d.chars().take(200).collect::<String>()),
            );
        }
        advance_emit(EVENT_SKIPPED, data, ev_path);
        AdvanceResult {
            decision: "skipped".to_string(),
            event: EVENT_SKIPPED,
            reason: Some(reason.to_string()),
            node_id: Some(node_id.to_string()),
            short_id: None,
            detail,
            exit_code: None,
            substrate: None,
            notes: Vec::new(),
        }
    };

    let Some(project) = dep
        .get("project")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
    else {
        return skip("no-project", None);
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let work_map = crate::territory::workspace_paths(&cwd);
    let mapped = work_map.get(project).cloned();
    let root: String = if cross_project {
        match mapped {
            Some(r) => r,
            None => return skip("unmapped-project", Some(project.to_string())),
        }
    } else {
        // Same-project: the work-map root is the cwd authority; recorded cwd
        // is fallback data. Fail closed if neither resolves rather than
        // guess canonical main.
        match mapped.or_else(|| {
            dep.get("cwd")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_string)
        }) {
            Some(r) => r,
            None => return skip("no-cwd", None),
        }
    };

    converge_one(
        dep,
        &root,
        ev_path,
        verbose,
        cross_project,
        None,
        Some(closed_node_id),
        model,
        provider,
        rank,
        source,
    )
}

/// Dispatch the closed node's now-unblocked direct dependents. Gated on the
/// same opt-in as advance(); strictly non-fatal; covers BOTH same-project
/// dependents and cross-project ones. Emits exactly one decision event per
/// dependent; a clean run with no dependents emits nothing.
#[allow(clippy::too_many_arguments)]
pub fn advance_dependents(
    closed_node_id: &str,
    closed_project: Option<&str>,
    project_root: Option<&Path>,
    events_path: Option<&Path>,
    verbose: bool,
    model: Option<&str>,
    provider: Option<&str>,
    source: Option<&str>,
) -> Vec<AdvanceResult> {
    let ev_path_owned = advance_events_path(project_root);
    let ev_path = events_path.or(ev_path_owned.as_deref());
    let (armed, rank) = auto_continue_resolve(project_root);
    if !armed {
        return Vec::new();
    }
    if walker_key().map(|k| claim_is_live(&k)).unwrap_or(false) {
        return Vec::new();
    }
    // Fail closed: without the closed node's project we cannot tell a
    // same-project dependent from a cross-project one, and misrouting lands
    // a worker on a protected branch where it dies. Dispatch nothing.
    let Some(closed_project) = closed_project.filter(|p| !p.is_empty()) else {
        advance_emit(
            EVENT_SKIPPED,
            json!({"reason": "closed-project-unknown", "closed_node_id": closed_node_id, "rank": rank}),
            ev_path,
        );
        return vec![AdvanceResult {
            decision: "skipped".to_string(),
            event: EVENT_SKIPPED,
            reason: Some("closed-project-unknown".to_string()),
            node_id: None,
            short_id: None,
            detail: None,
            exit_code: None,
            substrate: None,
            notes: Vec::new(),
        }];
    };
    let deps = match direct_dependents(closed_node_id, Some(closed_project)) {
        Ok(d) => d,
        Err(e) => {
            advance_emit(
                EVENT_SKIPPED,
                json!({
                    "reason": "dependents-error",
                    "closed_node_id": closed_node_id,
                    "detail": e.chars().take(200).collect::<String>(),
                    "rank": rank,
                }),
                ev_path,
            );
            return vec![AdvanceResult {
                decision: "skipped".to_string(),
                event: EVENT_SKIPPED,
                reason: Some("dependents-error".to_string()),
                node_id: None,
                short_id: None,
                detail: Some(e),
                exit_code: None,
                substrate: None,
                notes: Vec::new(),
            }];
        }
    };
    // Repaint each now-unblocked dependent's doc so a merge-gated dependent
    // carries current mirror fields the moment its blocker closes.
    let ids: Vec<String> = deps
        .iter()
        .filter_map(|d| d.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    project_unblocked(&ids);
    deps.iter()
        .map(|dep| {
            dispatch_one_dependent(
                dep,
                closed_node_id,
                ev_path,
                verbose,
                model,
                provider,
                Some(rank),
                source,
            )
        })
        .collect()
}
