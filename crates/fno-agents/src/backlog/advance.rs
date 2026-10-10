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
/// Fail-safe: ANY exception reading settings degrades to (false, "default")
/// rather than raising into the merge ritual.
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
    match crate::backlog::advance_settings::auto_continue_enabled_setting(project_root) {
        Ok(enabled) => (enabled, "config"),
        Err(_) => (false, "default"),
    }
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

/// `config.backlog.staleness_days` (default 21), fail-open to the default.
pub fn guard_staleness_days(project_root: Option<&Path>) -> i64 {
    match crate::backlog::advance_settings::load_merged(project_root) {
        Ok(doc) => doc
            .get("backlog")
            .and_then(|b| b.get("staleness_days"))
            .and_then(Value::as_i64)
            .unwrap_or(21),
        Err(_) => 21,
    }
}

/// One bounded child run's captured output.
pub(crate) struct BoundedOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// Run a child to completion with a hard wall-clock bound, capturing text
/// stdout/stderr. On timeout the child is killed and an error names the
/// bound.
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
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(bound_s);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut out) = child.stdout.take() {
                    use std::io::Read;
                    let _ = out.read_to_string(&mut stdout);
                }
                if let Some(mut err) = child.stderr.take() {
                    use std::io::Read;
                    let _ = err.read_to_string(&mut stderr);
                }
                return Ok(BoundedOutput {
                    stdout,
                    stderr,
                    code: status.code().unwrap_or(1),
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("exceeded {bound_s}s bound; killed"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e.to_string()),
        }
    }
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
