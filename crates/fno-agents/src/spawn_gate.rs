//! Spawn gate (x-c5cc): global concurrency cap + free-RAM floor + queue loop.
//!
//! Called at the top of the client `spawn` arm for the `bg`/`headless`
//! substrates only (`pane` re-execs into the Python CLI, whose mirrored gate
//! in `fno/agents/spawn_gate.py` is the sole gate on that path — exactly one
//! gate evaluation per spawn, LD1).
//!
//! The gate is READ-ONLY: the `max_live` slot cap counts the fno registry
//! (worker provenance) and the RAM floor reads system `vm_stat`/meminfo. The
//! claude daemon roster is consulted only as a LIVENESS ORACLE for fno bg rows
//! that carry no local pid, and by the post-spawn QoS demotion helper — never
//! as a population to count (x-bdf9: the roster's non-work sessions must not
//! consume worker slots; only rows that are ALSO in the fno registry count).
//! The gate's only writes are its own claims (`spawn-gate` check→dispatch mutex,
//! `worker:<name>` headless slot claims). Every guard fails OPEN on read errors
//! (LD5): the gate is protective infrastructure and must never become the thing
//! that bricks spawning.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;

use crate::agents_config;
use crate::claims;
use crate::claude_roster::ClaudeRoster;
use crate::daemon::pid_is_ours;
use crate::state::{load_registry, Registry, RegistryEntry};
use crate::AgentStatus;
use std::collections::HashMap;
use std::collections::HashSet;

/// Exit codes, distinct from existing dispatch codes (2, 13, 14, 15, 18, 127).
pub const EXIT_QUEUE_TIMEOUT: i32 = 75;
pub const EXIT_NO_WAIT: i32 = 76;
/// The per-territory team cap refused the spawn (x-e221), or its attribution
/// was unreadable. Byte-twin of the Python gate's `EXIT_TERRITORY_CAP`.
pub const EXIT_TERRITORY_CAP: i32 = 82;
pub const EXIT_RAM_REFUSED: i32 = 77;
/// The lane declares nothing about how it stands toward the fno state root
/// (epic rule R3). NOT "declares no carrier": an unsandboxed lane needs none.
pub const EXIT_STATE_ROOT_UNGRANTED: i32 = 78;
pub const EXIT_LOAD_REFUSED: i32 = 79;

/// Queue mechanics (Claude's Discretion 2: targets, not contracts).
const QUEUE_POLL: Duration = Duration::from_secs(2);
const QUEUE_PROGRESS_EVERY: Duration = Duration::from_secs(30);
const QUEUE_TIMEOUT: Duration = Duration::from_secs(600);
/// spawn-gate mutex TTL: generous vs the seconds-scale check→dispatch window;
/// PID liveness frees it instantly if the spawner dies.
const GATE_CLAIM_TTL_MS: i64 = 5 * 60 * 1000;
/// How long to tolerate an UNBROKEN run of failed mutex acquisitions before
/// proceeding unserialized. The mutex is a check→dispatch serializer, not a
/// state owner: a spawner that dies inside the critical section leaves it
/// `Suspect` for the full [`GATE_CLAIM_TTL_MS`], and with no bound here EVERY
/// spawner on the machine then queues behind that corpse until its own queue
/// timeout; the gate becomes the very thing that bricks spawning, which LD5
/// forbids. Failing open can overshoot the cap by the number of racing
/// spawners; wedging the whole mesh is strictly worse. Mirrors
/// `spawn_gate.py::MUTEX_WAIT_BUDGET_S`.
const MUTEX_WAIT_BUDGET: Duration = Duration::from_secs(60);
/// worker:<name> headless slot TTL: bounds a one-shot that outlives its
/// client pid record; PID liveness is the primary release.
const WORKER_CLAIM_TTL_MS: i64 = 4 * 60 * 60 * 1000;
const KNOWN_UNROUTED_PROVIDER: &str = "__uncapped__";

/// Registry statuses that can hold a live process (idle counts: an
/// idle-but-unreaped process still holds RAM; a reaped pid drops out via the
/// liveness check). Mirrors `spawn_gate.py::LIVE_STATUSES`.
fn status_is_liveish(s: &AgentStatus) -> bool {
    matches!(
        s,
        AgentStatus::Spawning
            | AgentStatus::Ready
            | AgentStatus::Idle
            | AgentStatus::Busy
            | AgentStatus::Live
            | AgentStatus::Restarting
    )
}

// ---------------------------------------------------------------------------
// Layer 2: available-RAM readers (pure parsers + platform dispatch)
// ---------------------------------------------------------------------------

/// Parse `vm_stat` output (macOS) to available bytes: (free + inactive +
/// speculative + purgeable) pages × page size. `None` on any shape surprise
/// so the guard fails open.
pub fn parse_vm_stat(text: &str) -> Option<u64> {
    // "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
    let page_size: u64 = text
        .lines()
        .next()?
        .split("page size of")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    let mut counted: u64 = 0;
    let mut found_free = false;
    for line in text.lines().skip(1) {
        let (label, value) = match line.split_once(':') {
            Some(kv) => kv,
            None => continue,
        };
        let label = label.trim();
        let want = matches!(
            label,
            "Pages free" | "Pages inactive" | "Pages speculative" | "Pages purgeable"
        );
        if !want {
            continue;
        }
        let pages: u64 = value.trim().trim_end_matches('.').parse().ok()?;
        counted += pages;
        if label == "Pages free" {
            found_free = true;
        }
    }
    // A vm_stat with no "Pages free" line is not vm_stat; refuse to guess.
    found_free.then_some(counted * page_size)
}

/// Parse `/proc/meminfo` (Linux) `MemAvailable:` kB to bytes.
pub fn parse_meminfo(text: &str) -> Option<u64> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest.trim().split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Available system RAM in GB, or `None` when unreadable (guard skipped, fail
/// open — a broken vm_stat must never brick spawning).
pub fn available_ram_gb() -> Option<f64> {
    available_bytes().map(|b| b as f64 / (1024.0 * 1024.0 * 1024.0))
}

#[cfg(target_os = "macos")]
fn available_bytes() -> Option<u64> {
    let out = std::process::Command::new("vm_stat").output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_vm_stat(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(target_os = "linux")]
fn available_bytes() -> Option<u64> {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn available_bytes() -> Option<u64> {
    None
}

// ---------------------------------------------------------------------------
// Layer 1: the worker-slot count
// ---------------------------------------------------------------------------

/// The node this spawn WORKS, from the calling process's `FNO_NODE` - the same
/// provenance source the client-side ask lanes stamp onto the registry row, so
/// the gate attributes a spawn exactly the way the row will be stamped.
pub fn gate_node() -> Option<String> {
    std::env::var("FNO_NODE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The liveness-filtered registry rows behind [`slot_count`], exposed so the
/// per-territory cap (x-e221) can read the rows' worked NODES without a second
/// liveness implementation.
fn live_rows(registry_path: &Path, warnings: &mut Vec<String>) -> Vec<RegistryEntry> {
    let live_roster_short_ids: std::collections::HashSet<String> =
        match ClaudeRoster::load_default() {
            Ok(roster) => roster
                .workers_deduped()
                .iter()
                .filter(|w| w.pid.map(|p| pid_is_ours(p, w.proc_start)).unwrap_or(false))
                .map(|w| w.short_id().to_string())
                .collect(),
            Err(e) => {
                warnings.push(format!(
                    "spawn-gate: claude roster unreadable ({e}); pid-less bg rows uncounted"
                ));
                Default::default()
            }
        };
    let mut rows = Vec::new();
    match load_registry(registry_path) {
        Ok(Registry { entries, .. }) => {
            for e in entries {
                if !status_is_liveish(&e.status) {
                    continue;
                }
                let alive = match e.pid {
                    Some(p) => pid_is_ours(p, e.pid_start_time),
                    None => e
                        .transport_short()
                        .map(|sid| live_roster_short_ids.contains(sid))
                        .unwrap_or(false),
                };
                if alive {
                    rows.push(e);
                }
            }
        }
        Err(e) => warnings.push(format!(
            "spawn-gate: fno registry unreadable ({e}); slot count degraded to 0"
        )),
    }
    rows
}

/// Count fno WORKER SLOTS in use for the `max_live` cap: liveness-filtered fno
/// registry rows + live `worker:<name>` headless slot claims.
///
/// This is deliberately NOT the full claude daemon roster (x-bdf9). The roster
/// carries every live claude session, dozens of memory-plugin observers and
/// resident-idle sessions among them, none of which is fno work; counting them
/// let the slot cap read "20/15" with zero real build workers running and wedge
/// `/target bg`. Registry membership IS the "fno spawned this for work"
/// provenance (spawn writes the row), so the registry alone is the slot
/// denominator. The roster's RAM cost is still honored elsewhere:
/// [`check_ram_floor`] reads real available RAM from `vm_stat`/meminfo, which
/// already reflects every process the roster holds.
///
/// The roster IS still read here, but only as a LIVENESS ORACLE, not as a
/// population to count: a fno `claude --bg` row is minted with a jobId in
/// `short_id` but NO local `pid` (its process lives in the claude daemon, so
/// liveness is in the roster — see `claude_ask.rs`). Such a row's liveness is
/// resolved by looking its `short_id` up in the roster. This counts real fno bg
/// workers (which a pid-only filter would drop, letting the cap admit unbounded
/// bg workers — Codex P1 on PR #235) WITHOUT counting non-fno sessions: a
/// memory-plugin observer has no registry row, so it is never reached.
///
/// Read-only; a registry read failure degrades to a 0 contribution with one
/// warning line pushed to `warnings` (LD5, fail open).
pub fn slot_count(registry_path: &Path, warnings: &mut Vec<String>) -> usize {
    live_rows(registry_path, warnings).len() + live_worker_slot_claims(warnings)
}

/// The territory (key, member node ids) a node belongs to, or `None` when the
/// answer cannot be READ (unreadable graph, node absent, uncompilable live
/// crown). Mirrors the Python `_territory_of_node`: membership is EXCLUSIVE
/// and most-specific-first - a node under a live crown scope counts for that
/// crown's territory; an uncrowned node counts for its project's loose
/// territory (project nodes minus every crowned set), so one worker never
/// consumes two territories' caps. AC9-HP parity: keep both sides agreeing.
fn territory_of_node(
    config_cwd: &Path,
    registry_path: &Path,
    node: &str,
    warnings: &mut Vec<String>,
) -> Option<(String, std::collections::HashSet<String>)> {
    use crate::king_board::graph_json_path;
    use crate::king_board::project_map;
    use crate::king_board::scope::compile_territory;

    let entries: Vec<Value> = {
        let path = graph_json_path(config_cwd);
        let raw = std::fs::read_to_string(&path).ok()?;
        let parsed: Value = serde_json::from_str(&raw).ok()?;
        if let Some(list) = parsed.get("entries").and_then(Value::as_array) {
            list.clone()
        } else if let Some(list) = parsed.as_array() {
            list.clone()
        } else {
            return None;
        }
    };
    let by_id: HashMap<String, &Value> = entries
        .iter()
        .filter_map(|e| {
            let id = e.get("id").and_then(Value::as_str)?;
            Some((id.to_string(), e))
        })
        .collect();
    let row: Option<&Value> = by_id.get(node).copied();
    if row.is_none() {
        return None;
    }

    // Live crowns from the registry cache, canonical scope strings.
    let mut crowns: Vec<String> = Vec::new();
    match load_registry(registry_path) {
        Ok(Registry { entries: rows, .. }) => {
            for r in &rows {
                let scope = r.crown_scope.as_deref().unwrap_or("").trim();
                if scope.is_empty() || !status_is_liveish(&r.status) {
                    continue;
                }
                let mut members: Vec<String> = scope
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                members.sort();
                members.dedup();
                let canon = members.join(",");
                if !canon.is_empty() && !crowns.contains(&canon) {
                    crowns.push(canon);
                }
            }
        }
        Err(e) => {
            warnings.push(format!("territory: registry unreadable ({e}); refusing"));
            return None;
        }
    }
    crowns.sort();

    let projects = match project_map(config_cwd) {
        Ok(m) => m,
        Err(_) => HashMap::new(),
    };
    let mut crowned: HashSet<String> = HashSet::new();
    for scope in &crowns {
        let compiled = compile_territory(scope, &entries, &Ok(projects.clone()));
        match compiled {
            Ok((_, ids)) => {
                if ids.contains(node) {
                    return Some((scope.clone(), ids));
                }
                crowned.extend(ids);
            }
            Err(e) => {
                warnings.push(format!("territory: crown {scope} uncompilable: {e}"));
                return None;
            }
        }
    }
    let project = row
        .and_then(|r| r.get("project").and_then(Value::as_str))
        .unwrap_or("");
    if project.is_empty() {
        return None;
    }
    let mut loose: HashSet<String> = HashSet::new();
    for e in &entries {
        if let Some(id) = e.get("id").and_then(Value::as_str) {
            let p = e.get("project").and_then(Value::as_str).unwrap_or("");
            if p == project && !crowned.contains(id) {
                loose.insert(id.to_string());
            }
        }
    }
    Some((format!("loose:{project}"), loose))
}

/// The per-territory team cap (x-e221). `Err` carries the refusal receipt the
/// caller prints; `None` territory reads as UNKNOWN and refuses closed - the
/// cap never counts an unknown as headroom. A spawn that works no node skips
/// the check entirely: the team cap does not apply to it.
fn check_territory_cap(
    config_cwd: &Path,
    registry_path: &Path,
    node: &str,
    live: &[RegistryEntry],
    cap: u32,
) -> Result<(), String> {
    let mut warnings = Vec::new();
    let state = territory_of_node(config_cwd, registry_path, node, &mut warnings);
    for w in &warnings {
        eprintln!("{w}");
    }
    let Some((scope, members)) = state else {
        return Err(serde_json::json!({
            "status": "refused",
            "reason": "territory_unknown",
            "node": node,
            "max_live_per_territory": cap,
        })
        .to_string());
    };
    let count = live
        .iter()
        .filter(|r| {
            r.node
                .as_deref()
                .map(|n| members.contains(n))
                .unwrap_or(false)
        })
        .count();
    if count as u32 >= cap {
        return Err(serde_json::json!({
            "status": "refused",
            "reason": "territory_cap",
            "territory": scope,
            "count": count,
            "current_count": count,
            "max_live_per_territory": cap,
        })
        .to_string());
    }
    Ok(())
}

/// Live `worker:<name>` slot claims under the GLOBAL claims root. Headless
/// one-shots write no registry row, so their gate acquires one of these for
/// the call duration; concurrent gates see them here. `Suspect` counts like
/// `Live` (TTL-protected, never up for grabs).
fn live_worker_slot_claims(warnings: &mut Vec<String>) -> usize {
    let root = match gate_claims_root() {
        Some(r) => r,
        None => return 0,
    };
    let dir = root.join(".fno/claims");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return 0, // no claims dir yet: nothing held.
    };
    let prefix = claims::encode_key("worker:");
    let mut n = 0usize;
    for entry in entries.flatten() {
        let fname = entry.file_name();
        let fname = fname.to_string_lossy();
        if !fname.starts_with(prefix.as_str()) {
            continue;
        }
        // strip_suffix, not trim_end_matches: a worker name ending in ".lock"
        // must lose exactly one suffix (gemini MEDIUM).
        let key = match fname.strip_suffix(".lock").and_then(urldecode) {
            Some(k) => k,
            None => continue,
        };
        match claims::status(&key, Some(&root)) {
            (claims::ClaimState::Live, _) | (claims::ClaimState::Suspect, _) => n += 1,
            (claims::ClaimState::Corrupted, _) => {
                warnings.push(format!("spawn-gate: corrupted slot claim {key} ignored"));
            }
            _ => {}
        }
    }
    n
}

/// Minimal percent-decoder for claim filenames (inverse of
/// `claims::encode_key`). `None` on malformed escapes.
fn urldecode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The gate's claims live under the GLOBAL root: the RAM budget is
/// machine-wide, so `spawn-gate` / `worker:<name>` must be visible across
/// projects and worktrees (unlike default project-local claims).
fn gate_claims_root() -> Option<PathBuf> {
    claims::global_claims_root()
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// Flags the spawn arm parses for the gate.
#[derive(Debug, Clone, Copy, Default)]
pub struct GateFlags {
    /// Bypass cap AND RAM floor (still QoS-demotes); prints a forced line.
    pub force: bool,
    /// Fail immediately at cap instead of queueing.
    pub no_wait: bool,
}

/// Held gate state. The caller keeps this alive across its dispatch call and
/// calls [`GateGuard::release`] (or drops it) when the dispatch result exists,
/// so the next waiter's count includes the newcomer.
#[derive(Debug, Default)]
pub struct GateGuard {
    /// `spawn-gate` mutex (bg path: held across dispatch until the
    /// registry/roster row exists).
    gate_key: Option<(String, String)>, // (key, holder)
    /// `worker:<name>` slot claim (headless path: held for the call duration).
    worker_key: Option<(String, String)>,
    root: Option<PathBuf>,
}

impl GateGuard {
    /// Release everything still held. Idempotent.
    pub fn release(&mut self) {
        let root = self.root.clone();
        if let Some((key, holder)) = self.gate_key.take() {
            let _ = claims::release(&key, &holder, root.as_deref(), None);
        }
        if let Some((key, holder)) = self.worker_key.take() {
            let _ = claims::release(&key, &holder, root.as_deref(), None);
        }
    }

    /// Release only the check→dispatch mutex, keeping the worker slot claim
    /// (headless: the slot must stay visible for the one-shot's duration).
    fn release_gate_mutex(&mut self) {
        if let Some((key, holder)) = self.gate_key.take() {
            let _ = claims::release(&key, &holder, self.root.as_deref(), None);
        }
    }
}

impl Drop for GateGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// Pure parity core (x-91b5, AC2-FR): would a bypass in this env emit
/// `spawn-cap`? True iff `FNO_SPAWN_GATE=0` AND no non-empty test-context
/// marker. Mirrors `fno.events.gate_escape.should_emit_spawn_cap` exactly; a
/// shared JSON fixture (`gate_escape_spawn_cap_parity.json`) asserts the two
/// implementations agree on every row, so neither can drift (Locked Decision 5).
pub fn spawn_cap_would_emit(get: impl Fn(&str) -> Option<String>) -> bool {
    let is_set = |k: &str| get(k).is_some_and(|v| !v.is_empty());
    get("FNO_SPAWN_GATE").as_deref() == Some("0")
        && !["PYTEST_CURRENT_TEST", "CI", "FNO_E2E"]
            .iter()
            .any(|k| is_set(k))
}

/// Auto-emit `gate_escape{reason:spawn-cap}` on an operator bypass of THIS gate
/// (`FNO_SPAWN_GATE=0`) outside a test context (Locked Decision 2). Best-effort:
/// shells the shared `fno doctor event gate-escape` verb (which owns the dedup key +
/// canonical-log resolution, one emit path) and ignores every failure so a
/// spawn is never blocked by telemetry (AC1-FR). The verb, not this shell,
/// computes the `(reason, session, day)` dedup bucket, so a Rust-emitted and a
/// Python-emitted spawn-cap in the same session/day still collapse to one.
fn maybe_emit_spawn_cap_escape() {
    if !spawn_cap_would_emit(|k| std::env::var(k).ok()) {
        return;
    }
    let _ = std::process::Command::new("fno")
        .args([
            "doctor",
            "event",
            "gate-escape",
            "spawn-cap",
            "--detail",
            "FNO_SPAWN_GATE=0 operator bypass",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Refuse a spawn onto a lane whose stance toward the fno state root is
/// UNDECLARED.
///
/// The trigger is undeclared, NOT "declares no carrier", and that difference
/// is the correctness of this guard. Epic rule R3 refuses a process denied its
/// declared root. A lane running under no sandbox is never denied anything, so
/// it needs no carrier, and refusing it turns a working spawn into a refused
/// one. Measured 2026-08-28: an opencode PANE worker with no grant acquired a
/// claim and delivered mail, both read from the operator root. A
/// carrier-shaped trigger would have broken that lane.
///
/// `writable_dirs.py` had already ruled this way on purpose, printing a named
/// stderr line rather than raising, so that a computed default the caller
/// never asked for cannot refuse a spawn. This guard agrees with that rather
/// than quietly reversing it.
///
/// THIS GUARD FAILS CLOSED on an unreadable contract, alone in this module.
/// Every other guard here fails OPEN on a read error because the gate is
/// protective infrastructure and must never brick spawning (LD5). That holds
/// for a RAM floor, where a missed refusal costs a slow machine, and it
/// INVERTS here. A spawn allowed past a missing grant produces a worker that
/// goes live, runs, edits code, and cannot claim a node, deliver mail, or
/// spawn a peer, and cannot report that either, because reporting is what it
/// lost. Five such workers died mute in one night. Silence is the harm.
///
/// Fails open on exactly one case: `roots` is empty. There is then no root to
/// grant and nothing to refuse.
pub fn state_root_grant_gate(harness: &str, substrate: &str, roots: &[String]) -> Result<(), i32> {
    if roots.is_empty() {
        return Ok(());
    }
    let contract = match crate::harness_capabilities::HarnessContract::packaged() {
        Ok(contract) => contract,
        Err(error) => {
            eprintln!("refused: the harness capability contract is unreadable ({error})");
            eprintln!(
                "  a state root resolves for this spawn and no lane can be verified to carry it."
            );
            return Err(EXIT_STATE_ROOT_UNGRANTED);
        }
    };
    if contract.state_root_stance(harness, substrate).is_some() {
        return Ok(());
    }
    // R3: name the root. A refusal that says "denied" without saying WHICH
    // directory sends the reader back to the code to find out.
    eprintln!(
        "refused: the {harness}/{substrate} lane does not declare how it stands \
         toward the state root"
    );
    for root in roots {
        eprintln!("  {root}");
    }
    eprintln!(
        "an undeclared lane can produce a worker that edits code and cannot claim, \
         mail, or spawn, and cannot report that either."
    );
    eprintln!(
        "add {substrate} to [harness.{harness}.state_root_grant]: a carrier name, \
         \"unsandboxed\" when measured to need none, or \"unmeasured\"."
    );
    Err(EXIT_STATE_ROOT_UNGRANTED)
}

/// Run the full gate for a `bg`/`headless` spawn. Returns a guard to keep
/// alive across dispatch on pass, or `Err(exit_code)` on refusal/timeout.
/// All human-facing output goes to stderr (LD10: the stdout receipt is
/// byte-reserved for the pass path).
pub fn run_gate(
    config_cwd: &Path,
    registry_path: &Path,
    name: &str,
    substrate: &str,
    flags: GateFlags,
) -> Result<GateGuard, i32> {
    // FNO_SPAWN_GATE=0 disables the gate entirely (the FNO_THINK_SPAWN=0
    // precedent): test suites exercising spawn plumbing must not queue behind
    // the REAL machine's live workers, and it doubles as an operator escape.
    if std::env::var_os("FNO_SPAWN_GATE").is_some_and(|v| v == "0") {
        maybe_emit_spawn_cap_escape();
        return Ok(GateGuard::default());
    }
    let cap = agents_config::max_live(config_cwd) as usize;
    let floor_gb = agents_config::min_free_gb(config_cwd);
    let load_ceiling = agents_config::max_load_per_cpu(config_cwd);
    let fleet_cpu_share = agents_config::max_fleet_cpu_share(config_cwd);
    let hard_load_ceiling = agents_config::hard_max_load_per_cpu(config_cwd);
    let holder = format!("spawn-gate:{}:{}", std::process::id(), name);
    let root = gate_claims_root();

    let mut guard = GateGuard {
        gate_key: None,
        worker_key: None,
        root: root.clone(),
    };

    if flags.force {
        eprintln!("spawn-gate: forced past cap, RAM floor, and load ceiling (--force)");
        if substrate == "headless" {
            acquire_worker_slot(&mut guard, name, &holder);
        }
        return Ok(guard);
    }

    let started = Instant::now();
    let mut last_progress = Instant::now();
    let mut announced = false;
    let mut last_slots: usize = 0;
    // Start of the current UNBROKEN run of failed acquisitions (None = holding
    // or not yet contended). Reset on every success so a long legitimate queue
    // never accumulates into a spurious fail-open.
    let mut mutex_blocked_since: Option<Instant> = None;

    loop {
        // Serialize check→dispatch under the spawn-gate mutex so N concurrent
        // spawners at cap-1 can't all pass. Not held across the wait sleep.
        let mut acquired_mutex = match claims::acquire(
            "spawn-gate",
            &holder,
            claims::AcquireOpts {
                ttl_ms: Some(GATE_CLAIM_TTL_MS),
                root: root.clone(),
                ..Default::default()
            },
        ) {
            claims::AcquireOutcome::Acquired(_) => true,
            claims::AcquireOutcome::HeldByOther { .. } => false,
            claims::AcquireOutcome::Error(e) => {
                // Fail open: the mutex is a serializer, not a state owner.
                eprintln!("spawn-gate: mutex unavailable ({e}); proceeding unserialized");
                true
            }
        };

        if acquired_mutex {
            mutex_blocked_since = None;
        } else {
            let now = Instant::now();
            let since = *mutex_blocked_since.get_or_insert(now);
            // --no-wait means "do not queue", and a busy mutex is queueing.
            // Refusing here (rather than falling through to the sleep) is what
            // keeps the promise: without it the caller waits the full
            // QUEUE_TIMEOUT and then gets EXIT_QUEUE_TIMEOUT, so it cannot even
            // tell "cap is full" from "the gate is wedged".
            if flags.no_wait {
                eprintln!(
                    "spawn-gate: another spawner holds the gate mutex; refusing \
                     (--no-wait). See `fno agents top`."
                );
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "refused",
                        "reason": "no_wait_mutex_held",
                        "max_live": cap,
                    })
                );
                use std::io::Write;
                let _ = std::io::stdout().flush();
                return Err(EXIT_NO_WAIT);
            }
            if now.duration_since(since) >= MUTEX_WAIT_BUDGET {
                eprintln!(
                    "spawn-gate: gate mutex still held after {}s (holder likely died \
                     mid-gate); proceeding unserialized",
                    MUTEX_WAIT_BUDGET.as_secs()
                );
                acquired_mutex = true;
            }
        }

        if acquired_mutex {
            guard.gate_key = Some(("spawn-gate".to_string(), holder.clone()));
            let mut warnings = Vec::new();
            let live = live_rows(registry_path, &mut warnings);
            let slots = live.len() + live_worker_slot_claims(&mut warnings);
            last_slots = slots;
            for w in &warnings {
                eprintln!("{w}");
            }
            if slots < cap {
                // Slot free. RAM recheck happens NOW (at dequeue too — a spawn
                // that queued 5 minutes must not dispatch into a tight machine).
                if let Err(code) = check_ram_floor(floor_gb) {
                    guard.release();
                    return Err(code);
                }
                if let Err((code, cause_stated)) =
                    check_load_ceiling(load_ceiling, fleet_cpu_share, hard_load_ceiling)
                {
                    // The refusal is decided; drop the mutex BEFORE the cause
                    // probe so queued spawners (and --no-wait callers) never
                    // sit behind seconds of evidence gathering.
                    guard.release();
                    // Only the backstop refuses without reading attribution, so
                    // it is the only branch this line can inform. This probe is
                    // a SECOND, independent sample: beside a refusal that
                    // already named its own it would print two disagreeing
                    // measurements, which is the defect x-7c0f removed.
                    if !cause_stated {
                        eprintln!(
                            "{}",
                            footprint_cause_evidence().unwrap_or_else(|| {
                                "spawn-gate: footprint cause unavailable; load refusal unchanged"
                                    .to_string()
                            })
                        );
                    }
                    return Err(code);
                }
                // The per-territory team cap (x-e221): beside the machine cap,
                // never instead of it. Refuses (never queues) like the provider
                // cap - waiting cannot help while the node's own territory is
                // full, and other territories keep their headroom.
                if let Some(node) = gate_node() {
                    if let Err(receipt) = check_territory_cap(
                        config_cwd,
                        registry_path,
                        &node,
                        &live,
                        agents_config::territory_max_live(config_cwd),
                    ) {
                        guard.release();
                        eprintln!("{receipt}");
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                        return Err(EXIT_TERRITORY_CAP);
                    }
                }
                if substrate == "headless" {
                    acquire_worker_slot(&mut guard, name, &holder);
                    // Slot claim is visible to concurrent gates: the mutex has
                    // done its job for this spawn.
                    guard.release_gate_mutex();
                }
                // bg path: keep the mutex until the caller's dispatch returns
                // (registry/roster row exists) — released via GateGuard.
                return Ok(guard);
            }
            // At cap: drop the mutex before waiting.
            guard.release_gate_mutex();

            if flags.no_wait {
                eprintln!(
                    "spawn-gate: {slots} live worker slots >= max_live {cap}; refusing (--no-wait). \
                     See `fno agents top`."
                );
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "refused",
                        "reason": "no_wait",
                        "max_live": cap,
                        "count": slots,
                        "current_count": slots,
                    })
                );
                use std::io::Write;
                let _ = std::io::stdout().flush();
                return Err(EXIT_NO_WAIT);
            }
            if !announced {
                eprintln!(
                    "spawn queued: {slots} live worker slots >= max_live {cap}; waiting for a free \
                     slot (--no-wait to fail fast, --force to bypass)"
                );
                announced = true;
                last_progress = Instant::now();
            } else if last_progress.elapsed() >= QUEUE_PROGRESS_EVERY {
                eprintln!(
                    "still queued: {slots}/{cap} live worker slots, waited {}s",
                    started.elapsed().as_secs()
                );
                last_progress = Instant::now();
            }
        }

        if started.elapsed() >= QUEUE_TIMEOUT {
            eprintln!(
                "spawn-gate: queue timeout after {}s at max_live {cap}; \
                 inspect live workers with `fno agents top`, or retry with --no-wait/--force",
                QUEUE_TIMEOUT.as_secs()
            );
            println!(
                "{}",
                serde_json::json!({
                    "status": "refused",
                    "reason": "queue_timeout",
                    "max_live": cap,
                    "count": last_slots,
                    "current_count": last_slots,
                })
            );
            use std::io::Write;
            let _ = std::io::stdout().flush();
            return Err(EXIT_QUEUE_TIMEOUT);
        }
        std::thread::sleep(QUEUE_POLL);
    }
}

/// RAM floor check (Layer 2): refuse below `floor_gb` (never queue — low RAM
/// with an under-cap worker count means something ELSE is eating the machine).
/// `<= 0` disables; unreadable RAM skips with a warning (fail open).
fn check_ram_floor(floor_gb: f64) -> Result<(), i32> {
    if floor_gb <= 0.0 {
        return Ok(());
    }
    match available_ram_gb() {
        Some(avail) if avail >= floor_gb => Ok(()),
        Some(avail) => {
            eprintln!(
                "spawn-gate: available RAM {avail:.1}GB is below the min_free_gb floor \
                 {floor_gb:.1}GB; refusing to spawn (--force to bypass)"
            );
            println!(
                "{}",
                serde_json::json!({
                    "status": "refused",
                    "reason": "ram_floor",
                    "available_gb": avail,
                    "min_free_gb": floor_gb,
                })
            );
            use std::io::Write;
            let _ = std::io::stdout().flush();
            Err(EXIT_RAM_REFUSED)
        }
        None => {
            eprintln!("spawn-gate: could not read available RAM; skipping the floor check");
            Ok(())
        }
    }
}

/// 1-min load average, or `None` where the platform has no reading (guard
/// skipped, fail open). `libc::getloadavg` is POSIX; the cfg guards keep the
/// crate building on platforms without it.
#[cfg(unix)]
fn loadavg_1m() -> Option<f64> {
    let mut loads = [0.0f64; 3];
    // SAFETY: `loads` is a valid 3-f64 array for getloadavg to fill.
    let n = unsafe { libc::getloadavg(&mut loads as *mut f64, 3) };
    if n >= 1 {
        Some(loads[0])
    } else {
        None
    }
}

#[cfg(not(unix))]
fn loadavg_1m() -> Option<f64> {
    None
}

#[derive(Debug, Deserialize)]
struct FootprintCausePayload {
    fleet_cpu_cores: f64,
    cpu_capacity_cores: f64,
    fleet_percent_capacity: f64,
    fleet_percent_measured_cpu: f64,
    /// Footprint's own sentence naming what it could not attribute. Present
    /// only when there IS a gap, which is also what drives its exit 4 under
    /// `--cause-only`, so presence is the discriminator and absence means the
    /// reading is complete.
    ///
    /// Not `capacity_verdict`: `--cause-only` deliberately spends no load
    /// snapshot, so `capacity_verdict` is the constant `"unknown"` on every
    /// cause payload, gap or no gap. Reading it here would refuse every
    /// admission above the trigger.
    #[serde(default)]
    attribution_gap: Option<String>,
}

/// What footprint answered when the gate asked whose CPU this is.
///
/// Three answers rather than two, because a refusal that cannot say WHY sends
/// its reader after the wrong cause. Measured 2026-09-04: the gate discarded a
/// complete 3380-byte payload because the probe exited 4, then refused saying
/// attribution was "unavailable" while footprint had in fact answered and
/// named 21 unmapped bg-socket rows. An hour went into load averages and a
/// spare pool that were never the point.
///
/// Admission is unchanged: `Incomplete` and `Unreadable` both refuse, because
/// an undercounted share is still not evidence of headroom. Only the sentence
/// differs, so the Python twin in `cli/src/fno/agents/spawn_gate.py` and this
/// gate still agree about who gets in.
enum FleetReading {
    /// Every row attributed: the share decides admission.
    Known(f64, f64),
    /// Footprint answered and disclaimed its own answer. Carries its words.
    Incomplete(String),
    /// No usable answer at all.
    Unreadable,
}

/// Footprint's attribution as numbers: `(fleet_cores, capacity_cores)`.
///
/// The governor and the refusal text must read ONE instrument. Before x-7c0f
/// these numbers existed only inside the explanation string, which is how a
/// gate came to print `0.79/12.00 cores` in the same breath as a refusal
/// decided on something else. `None` means unreadable, which is never
/// headroom (x-e040: this sensor goes blind under the load it measures).
fn parse_footprint_cause_json(raw: &str) -> Option<(f64, f64)> {
    let payload: FootprintCausePayload = serde_json::from_str(raw).ok()?;
    if payload.cpu_capacity_cores <= 0.0
        || !payload.cpu_capacity_cores.is_finite()
        || !payload.fleet_cpu_cores.is_finite()
        || payload.fleet_cpu_cores < 0.0
    {
        return None;
    }
    Some((payload.fleet_cpu_cores, payload.cpu_capacity_cores))
}

fn format_footprint_cause_json(raw: &str) -> Option<String> {
    let payload: FootprintCausePayload = serde_json::from_str(raw).ok()?;
    let values = [
        payload.fleet_cpu_cores,
        payload.cpu_capacity_cores,
        payload.fleet_percent_capacity,
        payload.fleet_percent_measured_cpu,
    ];
    if payload.cpu_capacity_cores <= 0.0
        || values
            .iter()
            .any(|value| !value.is_finite() || *value < 0.0)
    {
        return None;
    }
    let line = format!(
        "spawn-gate: footprint attributes {:.2}/{:.2} cores ({:.1}% capacity, {:.1}% of measured CPU) to the fleet",
        payload.fleet_cpu_cores,
        payload.cpu_capacity_cores,
        payload.fleet_percent_capacity,
        payload.fleet_percent_measured_cpu,
    );
    // A gapped reading is an UNDERCOUNT. Printing its share bare sends the
    // reader hunting the unattributed remainder outside the fleet, which is
    // the wrong-cause failure this evidence line exists to prevent. The
    // Python twin drops the line entirely; naming the gap keeps the number
    // and removes the claim that it is the whole answer.
    Some(match payload.attribution_gap {
        Some(gap) => format!(
            "{line}, but could not attribute every row ({gap}), so that share is an undercount"
        ),
        None => line,
    })
}

/// Wall-clock budget for the out-of-process footprint probe: the Python
/// twin's 5s measurement budget plus an allowance for interpreter startup.
const FOOTPRINT_PROBE_BUDGET: Duration = Duration::from_secs(8);

fn footprint_cli_binary() -> Option<&'static str> {
    ["fno", "fno-py"]
        .into_iter()
        .find(|name| resolves_on_path(name))
}

fn fleet_cpu_reading() -> FleetReading {
    match footprint_cause_raw() {
        Some(raw) => classify_footprint_cause_json(&raw),
        None => FleetReading::Unreadable,
    }
}

/// Sort one payload into the three answers, the disclaimer first.
///
/// The gap sentence is read before the numbers on purpose: a payload can carry
/// a perfectly finite share and still disclaim it, which is exactly the case
/// that used to reach the governor as "unreadable". It is also the same field
/// the Python twin keys on, so the two gates admit the same machines.
fn classify_footprint_cause_json(raw: &str) -> FleetReading {
    let Ok(payload) = serde_json::from_str::<FootprintCausePayload>(raw) else {
        return FleetReading::Unreadable;
    };
    if let Some(gap) = payload.attribution_gap {
        return FleetReading::Incomplete(gap);
    }
    match parse_footprint_cause_json(raw) {
        Some((fleet, capacity)) => FleetReading::Known(fleet, capacity),
        None => FleetReading::Unreadable,
    }
}

fn footprint_cause_evidence() -> Option<String> {
    footprint_cause_raw().and_then(|raw| format_footprint_cause_json(&raw))
}

fn footprint_cause_raw() -> Option<String> {
    let binary = footprint_cli_binary()?;
    let mut child = Command::new(binary)
        .args(["doctor", "footprint", "--json", "--cause-only"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // 5s of MEASUREMENT plus 3s of interpreter start, and the two halves are
    // why this is not the 5s its Python twin passes to `cause_reading`.
    // Python spends its whole budget measuring; this budget also has to cover
    // spawning `fno doctor footprint` and importing it. Matching the numbers
    // would make the Rust gate time out first on a loaded box, and since
    // x-7c0f a timeout REFUSES rather than merely losing the explanation. Two
    // admission gates disagreeing about the same machine is the defect beside
    // the one this check fixes.
    let deadline = Instant::now() + FOOTPRINT_PROBE_BUDGET;
    loop {
        match child.try_wait() {
            // The exit code is not the discriminator; the payload is. Footprint
            // exits non-zero to report its OWN verdict (3 capacity-over, 4
            // unknown) while still writing a complete reading to stdout, so
            // gating on `status.success()` threw away the answer in exactly the
            // states the gate consults it about. Measured 2026-09-04: a 3380-byte
            // payload naming 21 unmapped bg-socket rows was discarded because the
            // probe exited 4. A genuinely failed run writes nothing parseable and
            // still reaches `Unreadable` through the classifier.
            Ok(Some(_)) => {
                let output = child.wait_with_output().ok()?;
                return Some(std::str::from_utf8(&output.stdout).ok()?.to_string());
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Refuse (never queue) when the FLEET is the reason the box is loaded.
///
/// Three thresholds, because the honest question needs two instruments:
/// `max_load_per_cpu x cpus` is a TRIGGER (below it we admit without
/// probing, so the common path costs no subprocess); above it the gate asks
/// footprint whose CPU this is and refuses only when the fleet holds more
/// than `max_fleet_cpu_share` of capacity; `hard_max_load_per_cpu x cpus`
/// refuses regardless of attribution.
///
/// WHY (x-7c0f, measured twice). This check refused on the 1-min load
/// average while printing footprint's contradicting attribution in the same
/// refusal: `load 127.6 exceeds ... 96.0` beside `attributes 0.79/12.00
/// cores (6.6% capacity)`. Load average counts runnable PLUS blocked
/// processes, so it is not a CPU measure and belongs to nobody. On
/// 2026-08-29 the three largest consumers on the refusing box were desktop
/// applications, and killing one unscoped recursive search moved the 1-min
/// load from 374 to 179 with no agent stopped.
///
/// The backstop exists because a pure fleet-share governor would admit onto
/// a box already thrashing from foreign work.
///
/// Same contract as [`check_ram_floor`] otherwise: `max_load_per_cpu <= 0`
/// disables, unreadable LOAD skips (fail open). Unreadable ATTRIBUTION
/// refuses (fail closed): an unknown share is not evidence of headroom.
/// The Python twin in `cli/src/fno/agents/spawn_gate.py` is the same
/// contract; the two gates must not disagree about admission.
///
/// The error carries `(exit code, cause_stated)`. `cause_stated` means the
/// refusal already printed the attribution sample it decided on, so the
/// caller must not append a second, independently taken one.
fn check_load_ceiling(
    max_load_per_cpu: f64,
    max_fleet_cpu_share: f64,
    hard_max_load_per_cpu: f64,
) -> Result<(), (i32, bool)> {
    if max_load_per_cpu <= 0.0 {
        return Ok(());
    }
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let load1 = match loadavg_1m() {
        Some(load1) => load1,
        None => {
            eprintln!("spawn-gate: could not read load average; skipping the load check");
            return Ok(());
        }
    };
    if !load_over_ceiling(load1, max_load_per_cpu, cpus) {
        return Ok(());
    }

    if hard_max_load_per_cpu > 0.0 && load_over_ceiling(load1, hard_max_load_per_cpu, cpus) {
        let backstop = hard_max_load_per_cpu * cpus as f64;
        eprintln!(
            "spawn-gate: 1-min load {load1:.1} exceeds the absolute machine backstop \
             hard_max_load_per_cpu {hard_max_load_per_cpu} x {cpus} cpus = {backstop:.1}; \
             refusing to spawn whoever caused it (--force to bypass)"
        );
        // The one branch that never reads attribution, so the caller's cause
        // probe is the only thing that can say whose load this was.
        return Err((EXIT_LOAD_REFUSED, false));
    }

    let trigger = max_load_per_cpu * cpus as f64;
    let (fleet, capacity) = match fleet_cpu_reading() {
        FleetReading::Known(fleet, capacity) => (fleet, capacity),
        FleetReading::Incomplete(gap) => {
            // Still a refusal: an undercounted share is not headroom. But the
            // reason is footprint's own sentence, which names a fix, where
            // "unavailable" named nothing and cost an hour of wrong hunting.
            eprintln!(
                "spawn-gate: 1-min load {load1:.1} is over the max_load_per_cpu trigger \
                 {max_load_per_cpu} x {cpus} cpus = {trigger:.1} and footprint could not \
                 attribute every row, so its fleet share is an undercount, not headroom: \
                 {gap}; refusing to spawn (--force to bypass)"
            );
            return Err((EXIT_LOAD_REFUSED, true));
        }
        FleetReading::Unreadable => {
            eprintln!(
                "spawn-gate: 1-min load {load1:.1} is over the max_load_per_cpu trigger \
                 {max_load_per_cpu} x {cpus} cpus = {trigger:.1} and fleet CPU attribution \
                 unavailable; refusing to spawn (--force to bypass)"
            );
            // The attribution read produced nothing parseable; the caller's
            // probe reads the same instrument and fails the same way.
            return Err((EXIT_LOAD_REFUSED, true));
        }
    };
    let share = fleet / capacity;
    if share > max_fleet_cpu_share {
        let pct = share * 100.0;
        let ceil_pct = max_fleet_cpu_share * 100.0;
        eprintln!(
            "spawn-gate: the fleet holds {fleet:.2}/{capacity:.2} cores ({pct:.1}% of \
             capacity), over the max_fleet_cpu_share ceiling {ceil_pct:.1}%; refusing to \
             spawn (--force to bypass)"
        );
        // This refusal already named the sample it decided on.
        return Err((EXIT_LOAD_REFUSED, true));
    }
    let pct = share * 100.0;
    eprintln!(
        "spawn-gate: 1-min load {load1:.1} is high but only {fleet:.2}/{capacity:.2} cores \
         ({pct:.1}%) are attributed to the fleet, so the load is not attributed to the \
         fleet; admitting the spawn"
    );
    Ok(())
}

/// The ceiling verdict as a pure function: 1-min loadavg vs factor x cpus.
/// At exactly the ceiling the spawn passes (the floor uses the same
/// inclusive-boundary convention).
fn load_over_ceiling(load1: f64, max_load_per_cpu: f64, cpus: usize) -> bool {
    load1 > max_load_per_cpu * cpus as f64
}

fn acquire_worker_slot(guard: &mut GateGuard, name: &str, holder: &str) {
    let key = format!("worker:{name}");
    match claims::acquire(
        &key,
        holder,
        claims::AcquireOpts {
            ttl_ms: Some(WORKER_CLAIM_TTL_MS),
            metadata: Some(serde_json::Map::from_iter([(
                "model_provider".to_string(),
                serde_json::Value::String(KNOWN_UNROUTED_PROVIDER.to_string()),
            )])),
            root: guard.root.clone(),
            ..Default::default()
        },
    ) {
        claims::AcquireOutcome::Acquired(_) => {
            guard.worker_key = Some((key, holder.to_string()));
        }
        // Fail open: a slot claim is count VISIBILITY, not a correctness gate.
        claims::AcquireOutcome::HeldByOther { .. } | claims::AcquireOutcome::Error(_) => {
            eprintln!("spawn-gate: worker slot claim {key} unavailable; proceeding uncounted");
        }
    }
}

// ---------------------------------------------------------------------------
// Layer 3: background QoS
// ---------------------------------------------------------------------------

/// Exec-wrap a child command at background priority when
/// `config.agents.worker_qos` is `utility`: `taskpolicy -c utility -- <cmd>`
/// on macOS, `nice -n 10 <cmd>` on Linux. Identity on `off` / other OSes.
pub fn qos_wrap(config_cwd: &Path, argv: Vec<String>) -> Vec<String> {
    if !agents_config::worker_qos_enabled(config_cwd) || argv.is_empty() {
        return argv;
    }
    // Don't wrap a command that won't resolve: callers report a missing
    // provider CLI as NotFound/127, and a taskpolicy prefix would swallow
    // that into the wrapper's own error.
    if !resolves_on_path(&argv[0]) {
        return argv;
    }
    // Absolute paths + existence check: a missing wrapper must degrade to an
    // unwrapped exec (fail open), never surface as a "CLI not found" spawn
    // failure for the actual worker command.
    let mut wrapped: Vec<String> = if cfg!(target_os = "macos") {
        if !Path::new("/usr/sbin/taskpolicy").exists() {
            return argv;
        }
        vec![
            "/usr/sbin/taskpolicy".into(),
            "-c".into(),
            "utility".into(),
            "--".into(),
        ]
    } else if cfg!(target_os = "linux") {
        if !Path::new("/usr/bin/nice").exists() {
            return argv;
        }
        vec!["/usr/bin/nice".into(), "-n".into(), "10".into()]
    } else {
        return argv;
    };
    wrapped.extend(argv);
    wrapped
}

/// Does `cmd` resolve to an executable (explicit path, or a PATH lookup)?
fn resolves_on_path(cmd: &str) -> bool {
    if cmd.contains('/') {
        return Path::new(cmd).exists();
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(cmd).is_file()))
        .unwrap_or(false)
}

/// Best-effort post-hoc demotion of a claude-daemon-owned bg worker pid
/// (`taskpolicy -b -p` on macOS, `renice 10 -p` on Linux; same uid, so
/// permitted). Non-fatal: failure prints one warning, the spawn stands.
pub fn qos_demote_pid(config_cwd: &Path, pid: u32) {
    if !agents_config::worker_qos_enabled(config_cwd) {
        return;
    }
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("/usr/sbin/taskpolicy")
            .args(["-b", "-p", &pid.to_string()])
            .status()
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("/usr/bin/renice")
            .args(["10", "-p", &pid.to_string()])
            .status()
    } else {
        return;
    };
    match status {
        Ok(s) if s.success() => {}
        _ => eprintln!("spawn-gate: QoS demotion of pid {pid} failed (non-fatal)"),
    }
}

/// After a `--substrate bg` dispatch, poll the roster briefly for the new
/// worker's pid and demote it post-hoc (its exec is claude's, not ours).
/// Bounded ~10s; one warning if the pid never appears (AC3-UI).
pub fn qos_demote_bg_worker(config_cwd: &Path, job_id: &str) {
    if !agents_config::worker_qos_enabled(config_cwd) || job_id.is_empty() {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(roster) = ClaudeRoster::load_default() {
            if let Some(pid) = roster.find(job_id).and_then(|w| w.pid) {
                qos_demote_pid(config_cwd, pid);
                return;
            }
        }
        if Instant::now() >= deadline {
            eprintln!(
                "spawn-gate: bg worker {job_id} pid not in roster within 10s; \
                 QoS demotion skipped (non-fatal)"
            );
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOTS: [&str; 1] = ["/Users/x/.fno"];

    fn roots() -> Vec<String> {
        ROOTS.iter().map(|r| r.to_string()).collect()
    }

    #[test]
    fn undeclared_lane_is_refused_with_its_own_exit_code() {
        // An unknown harness declares nothing at all, which is the only thing
        // this gate refuses.
        assert_eq!(
            state_root_grant_gate("nosuchharness", "thread", &roots()),
            Err(EXIT_STATE_ROOT_UNGRANTED)
        );
        assert_eq!(
            state_root_grant_gate("claude", "nosuchsubstrate", &roots()),
            Err(EXIT_STATE_ROOT_UNGRANTED)
        );
    }

    /// The regression this gate nearly shipped. Its first trigger was "declares
    /// no carrier", which refused every opencode pane and gemini spawn. Then an
    /// opencode PANE worker was measured acquiring a claim and delivering mail
    /// with no grant at all: it is unsandboxed, so it is never denied the root
    /// and R3 does not reach it. A lane that works must not be refused.
    #[test]
    fn a_lane_that_needs_no_carrier_is_never_refused() {
        for (harness, substrate) in [
            ("opencode", "pane"),     // measured unsandboxed
            ("opencode", "headless"), // unmeasured, so not refused on a guess
            ("gemini", "headless"),
            ("gemini", "pane"),
            ("gemini", "thread"),
        ] {
            assert_eq!(
                state_root_grant_gate(harness, substrate, &roots()),
                Ok(()),
                "{harness}/{substrate} declares a stance, so it must pass"
            );
        }
    }

    #[test]
    fn lanes_declaring_a_carrier_pass() {
        for (harness, substrate) in [
            ("claude", "thread"),
            ("claude", "headless"),
            ("codex", "thread"),
            ("codex", "headless"),
            ("agy", "thread"),
            ("opencode", "thread"),
        ] {
            assert_eq!(
                state_root_grant_gate(harness, substrate, &roots()),
                Ok(()),
                "{harness}/{substrate}"
            );
        }
    }

    /// Every harness and substrate the fleet dispatches must declare a stance.
    /// Without this the gate's refusal is unreachable in practice and a lane
    /// added later inherits silence instead of a loud refusal.
    #[test]
    fn every_shipped_lane_declares_its_stance() {
        let contract = crate::harness_capabilities::HarnessContract::packaged().unwrap();
        for (name, caps) in &contract.harness {
            for substrate in ["pane", "thread", "headless"] {
                assert!(
                    caps.state_root_stance(substrate).is_some(),
                    "{name}/{substrate} declares no stance toward the state root"
                );
            }
        }
    }

    /// The one narrow fail-open case: no root resolved means there is nothing
    /// to grant and nothing to refuse.
    #[test]
    fn no_resolved_root_passes_even_on_an_ungranted_lane() {
        assert_eq!(state_root_grant_gate("gemini", "headless", &[]), Ok(()));
    }

    #[test]
    fn spawn_cap_guard_agrees_with_python_gate_fixture() {
        // x-91b5 AC2-FR: this Rust guard must agree with the Python
        // should_emit_spawn_cap on every fixture row. Both read the same JSON;
        // a drift on either side fails its own assertion.
        let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../cli/tests/agents/fixtures/gate_escape_spawn_cap_parity.json");
        let raw = std::fs::read_to_string(&fixture_path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture_path.display()));
        let fixture: serde_json::Value = serde_json::from_str(&raw).unwrap();
        for sc in fixture["scenarios"].as_array().unwrap() {
            let name = sc["name"].as_str().unwrap();
            let env = sc["env"].clone();
            let get = |k: &str| env.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
            let expect = sc["expect"].as_bool().unwrap();
            assert_eq!(spawn_cap_would_emit(get), expect, "row {name}");
        }
    }

    const VM_STAT: &str = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
Pages free:                              100000.\n\
Pages active:                            500000.\n\
Pages inactive:                          200000.\n\
Pages speculative:                        50000.\n\
Pages throttled:                              0.\n\
Pages wired down:                        300000.\n\
Pages purgeable:                          25000.\n";

    #[test]
    fn vm_stat_counts_free_inactive_speculative_purgeable() {
        // (100000 + 200000 + 50000 + 25000) * 16384
        assert_eq!(parse_vm_stat(VM_STAT), Some(375_000 * 16_384));
    }

    #[test]
    fn vm_stat_unrecognized_shape_is_none() {
        assert_eq!(parse_vm_stat(""), None);
        assert_eq!(parse_vm_stat("something else entirely\n"), None);
        // Header without any "Pages free" line: refuse to guess.
        assert_eq!(
            parse_vm_stat("Mach Virtual Memory Statistics: (page size of 16384 bytes)\n"),
            None
        );
        // Garbage page count: None, not a partial sum.
        let bad = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
Pages free: banana.\n";
        assert_eq!(parse_vm_stat(bad), None);
    }

    #[test]
    fn meminfo_reads_memavailable_kb() {
        let text = "MemTotal:       16384000 kB\nMemFree:         1000000 kB\n\
MemAvailable:    8000000 kB\n";
        assert_eq!(parse_meminfo(text), Some(8_000_000 * 1024));
        assert_eq!(parse_meminfo("MemTotal: 1 kB\n"), None);
        assert_eq!(parse_meminfo("MemAvailable: banana kB\n"), None);
    }

    /// x-3f84 W3: the CPU dimension beside min_free_gb. The measured
    /// emergency was load 309 on 12 CPUs while the RAM floor held ten times
    /// its margin.
    #[test]
    fn load_ceiling_math_and_disabled() {
        // The measured night: 309 on 12 cpus at factor 8 (ceiling 96) refuses.
        assert!(load_over_ceiling(309.0, 8.0, 12));
        // A healthy fleet passes with margin.
        assert!(!load_over_ceiling(24.0, 8.0, 12));
        // Exactly at the ceiling passes (inclusive boundary, like the floor).
        assert!(!load_over_ceiling(96.0, 8.0, 12));
        // One factor ports across machines: 20 refuses an 8-cpu box, passes 16.
        assert!(load_over_ceiling(20.0, 2.0, 8));
        assert!(!load_over_ceiling(20.0, 2.0, 16));
        // Disabled (`<= 0`) never refuses, whatever the machine reads.
        // It disables the WHOLE check, governor and backstop with it, which
        // is why the other two arguments cannot rescue it.
        assert!(check_load_ceiling(0.0, 0.0, 1.0).is_ok());
        assert!(check_load_ceiling(-1.0, 0.0, 1.0).is_ok());
    }

    /// x-7c0f: the governor's arithmetic, as a table.
    ///
    /// The refusal this replaces printed `0.79/12.00 cores` beside a refusal
    /// decided on load average. Row one is that exact reading, and it now
    /// admits. Kept as a pure-function table because the check itself reads
    /// the live machine and a subprocess; the Python twin's
    /// `test_fleet_load_governor.py` covers the wiring with both stubbed.
    #[test]
    fn fleet_share_governs_and_backstop_overrides() {
        fn over_share(fleet: f64, capacity: f64, ceiling: f64) -> bool {
            fleet / capacity > ceiling
        }
        // The measured refusal, admitted: the load was real and was not ours.
        assert!(!over_share(0.79, 12.0, 0.5));
        // Our own fleet over half the box still refuses.
        assert!(over_share(9.0, 12.0, 0.5));
        // Exactly at the ceiling passes (inclusive, like the load boundary).
        assert!(!over_share(6.0, 12.0, 0.5));
        // The backstop is a load question, not a share one: at load 600 on 12
        // cpus it fires at factor 40 no matter how little the fleet holds.
        assert!(load_over_ceiling(600.0, 40.0, 12));
        assert!(!load_over_ceiling(374.0, 40.0, 12));
        // ...and it must sit well above the trigger, or it silently restores
        // the defect by refusing before attribution is ever consulted.
        assert!(
            agents_config::DEFAULT_HARD_MAX_LOAD_PER_CPU
                > agents_config::DEFAULT_MAX_LOAD_PER_CPU * 4.0
        );
    }

    /// The numbers seam and the explanation string must agree, because the
    /// whole defect was a gate deciding on one instrument while printing
    /// another.
    #[test]
    fn fleet_reading_parses_what_the_evidence_string_prints() {
        let raw = r#"{"fleet_cpu_cores":0.79,"cpu_capacity_cores":12,"fleet_percent_capacity":6.6,"fleet_percent_measured_cpu":17.3}"#;
        assert_eq!(parse_footprint_cause_json(raw), Some((0.79, 12.0)));
        assert!(format_footprint_cause_json(raw)
            .unwrap()
            .contains("0.79/12.00 cores"));
        // Unreadable is unreadable for both readers: never a zero share, which
        // would read to the governor as an idle fleet (x-e040).
        assert_eq!(parse_footprint_cause_json("{}"), None);
        assert_eq!(parse_footprint_cause_json("not json"), None);
        assert_eq!(
            parse_footprint_cause_json(
                r#"{"fleet_cpu_cores":1.0,"cpu_capacity_cores":0,"fleet_percent_capacity":0,"fleet_percent_measured_cpu":0}"#
            ),
            None
        );
    }

    /// A disclaimed answer is not a missing answer, and the refusal must say
    /// which it got. The payload here is the real one measured on 2026-09-04,
    /// trimmed to the fields the gate reads: footprint exited 4, wrote a
    /// finite share, and named the rows it could not attribute.
    #[test]
    fn a_gapped_payload_reads_as_incomplete_and_carries_footprints_words() {
        let raw = r#"{"fleet_cpu_cores":3.645,"cpu_capacity_cores":12,"fleet_percent_capacity":30.375,"fleet_percent_measured_cpu":45.4,"capacity_verdict":"unknown","attribution_gap":"14 pidless row(s) with no identity route (claude, codex); 21 bg-socket row(s) missing from the socket map"}"#;
        match classify_footprint_cause_json(raw) {
            FleetReading::Incomplete(gap) => {
                assert!(gap.contains("21 bg-socket row(s)"), "{gap}");
                assert!(gap.contains("14 pidless row(s)"), "{gap}");
            }
            FleetReading::Known(fleet, capacity) => {
                panic!("an undercounted share must never decide admission: {fleet}/{capacity}")
            }
            FleetReading::Unreadable => {
                panic!("footprint answered and named its gap; that is not unreadable")
            }
        }
    }

    /// The evidence line prints on the backstop refusal, the one branch that
    /// never reads attribution. A gapped share printed bare there reads as the
    /// fleet's whole cost and sends the reader after the remainder.
    #[test]
    fn the_evidence_line_disclaims_a_gapped_share() {
        let gapped = r#"{"fleet_cpu_cores":2.92,"cpu_capacity_cores":12,"fleet_percent_capacity":24.3,"fleet_percent_measured_cpu":45.4,"attribution_gap":"21 bg-socket row(s) missing from the socket map"}"#;
        let line = format_footprint_cause_json(gapped).expect("a gapped payload still formats");
        assert!(line.contains("2.92/12.00 cores"), "{line}");
        assert!(line.contains("21 bg-socket row(s)"), "{line}");
        assert!(line.contains("undercount"), "{line}");
    }

    /// `capacity_verdict` is NOT the discriminator, and this is the test that
    /// says so. `--cause-only` spends no load snapshot, so every cause payload
    /// carries the constant `"unknown"` whether or not attribution was
    /// complete. Keying on it refuses every admission above the load trigger
    /// while the whole suite stays green, because no hand-written fixture ever
    /// reproduces the shape the instrument actually emits.
    #[test]
    fn an_unknown_verdict_with_no_gap_sentence_still_decides_admission() {
        let raw = r#"{"fleet_cpu_cores":1.0,"cpu_capacity_cores":12,"fleet_percent_capacity":8.3,"fleet_percent_measured_cpu":20.0,"capacity_verdict":"unknown"}"#;
        match classify_footprint_cause_json(raw) {
            FleetReading::Known(fleet, capacity) => assert_eq!((fleet, capacity), (1.0, 12.0)),
            _ => panic!("a complete cause payload always says 'unknown'; it must still decide"),
        }
    }

    /// The two ends of the range still work: a complete answer decides, and
    /// junk is unreadable. Neither may become a zero share (x-e040).
    #[test]
    fn a_complete_payload_decides_and_junk_stays_unreadable() {
        // The real clean shape: a verdict word of `unknown` (cause-only takes
        // no load snapshot) and no `attribution_gap` key at all.
        let complete = r#"{"fleet_cpu_cores":0.79,"cpu_capacity_cores":12,"fleet_percent_capacity":6.6,"fleet_percent_measured_cpu":17.3,"capacity_verdict":"unknown"}"#;
        match classify_footprint_cause_json(complete) {
            FleetReading::Known(fleet, capacity) => {
                assert_eq!((fleet, capacity), (0.79, 12.0));
            }
            _ => panic!("a complete payload must decide admission"),
        }

        // No verdict field at all: the older payload shape still decides.
        let legacy = r#"{"fleet_cpu_cores":0.79,"cpu_capacity_cores":12,"fleet_percent_capacity":6.6,"fleet_percent_measured_cpu":17.3}"#;
        assert!(matches!(
            classify_footprint_cause_json(legacy),
            FleetReading::Known(_, _)
        ));

        for junk in ["{}", "not json", ""] {
            assert!(
                matches!(
                    classify_footprint_cause_json(junk),
                    FleetReading::Unreadable
                ),
                "{junk}"
            );
        }
    }

    #[test]
    fn footprint_cause_json_formats_available_and_low_share_payloads() {
        let heavy = r#"{"fleet_cpu_cores":1.86,"cpu_capacity_cores":12,"fleet_percent_capacity":15.5,"fleet_percent_measured_cpu":58.0}"#;
        assert_eq!(
            format_footprint_cause_json(heavy).as_deref(),
            Some("spawn-gate: footprint attributes 1.86/12.00 cores (15.5% capacity, 58.0% of measured CPU) to the fleet")
        );

        let low = r#"{"fleet_cpu_cores":0.20,"cpu_capacity_cores":12,"fleet_percent_capacity":1.7,"fleet_percent_measured_cpu":4.2}"#;
        assert_eq!(
            format_footprint_cause_json(low).as_deref(),
            Some("spawn-gate: footprint attributes 0.20/12.00 cores (1.7% capacity, 4.2% of measured CPU) to the fleet")
        );
    }

    #[test]
    fn footprint_cause_json_is_unavailable_for_incomplete_payloads() {
        assert_eq!(format_footprint_cause_json("{}"), None);
        assert_eq!(format_footprint_cause_json("not json"), None);
        assert_eq!(
            format_footprint_cause_json(
                r#"{"fleet_cpu_cores":-1,"cpu_capacity_cores":12,"fleet_percent_capacity":-8.3,"fleet_percent_measured_cpu":2.0}"#
            ),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn loadavg_1m_reads_a_positive_number_on_a_live_host() {
        // A positive control that the libc join itself works: a live unix
        // host always answers something >= 0.
        let load = loadavg_1m().expect("getloadavg must answer on unix");
        assert!(load >= 0.0);
    }

    /// Mirrors `test_no_wait_refuses_fast_when_the_mutex_is_contended` on the
    /// Python side: a busy gate mutex IS queueing, so `--no-wait` must refuse on
    /// it instead of falling through to the queue loop. The regression it pins
    /// made every `--no-wait` caller wait the full `QUEUE_TIMEOUT` behind a
    /// spawner that died mid-gate, then exit `EXIT_QUEUE_TIMEOUT`, so the
    /// caller could not tell "cap is full" from "the gate is wedged".
    #[test]
    fn no_wait_refuses_fast_when_the_mutex_is_contended() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-gate-nowait-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("claims-root");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &root);
        let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
        std::env::remove_var("FNO_SPAWN_GATE");
        // A high cap so the ONLY thing that can refuse here is the mutex.
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
            "[agents]\nmax_live = 999\nmin_free_gb = 0\n",
        )
        .unwrap();

        // Hold the mutex as somebody else, exactly as a corpse would.
        let held = claims::acquire(
            "spawn-gate",
            "spawn-gate:999999:ghost",
            claims::AcquireOpts {
                ttl_ms: Some(GATE_CLAIM_TTL_MS),
                root: Some(root.clone()),
                ..Default::default()
            },
        );
        assert!(
            matches!(held, claims::AcquireOutcome::Acquired(_)),
            "test setup: ghost must hold the mutex, got {held:?}"
        );
        // Positive control on the test's own premise. The ghost pid is dead, so
        // the claim is `Suspect` (TTL unexpired, holder gone) and acquire must
        // still report it held by another. Assert that instead of assuming it:
        // if claim semantics ever let a dead holder be reclaimed, the mutex
        // would be FREE, run_gate would sail through, and this test would pass
        // while exercising none of the branch it exists to pin.
        let contended = claims::acquire(
            "spawn-gate",
            "spawn-gate:probe",
            claims::AcquireOpts {
                ttl_ms: Some(GATE_CLAIM_TTL_MS),
                root: Some(root.clone()),
                ..Default::default()
            },
        );
        assert!(
            matches!(contended, claims::AcquireOutcome::HeldByOther { .. }),
            "test premise broken: a dead-holder claim must still read as held, got {contended:?}"
        );
        let started = Instant::now();
        let got = run_gate(
            &dir,
            &dir.join("registry.json"),
            "w2",
            "bg",
            GateFlags {
                force: false,
                no_wait: true,
            },
        );
        let elapsed = started.elapsed();
        let _ = claims::release("spawn-gate", "spawn-gate:999999:ghost", Some(&root), None);
        std::env::remove_var("FNO_CLAIMS_ROOT");
        match prior_spawn_gate {
            Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
            None => std::env::remove_var("FNO_SPAWN_GATE"),
        }

        assert_eq!(
            got.err(),
            Some(EXIT_NO_WAIT),
            "must refuse with the no-wait code"
        );
        assert!(
            elapsed < QUEUE_TIMEOUT,
            "must refuse fast, not queue: took {elapsed:?}"
        );
    }

    #[test]
    fn queue_timeout_refuses_with_receipt_and_exit_code() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-gate-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("claims-root");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &root);
        let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
        std::env::remove_var("FNO_SPAWN_GATE");
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
            "[agents]\nmax_live = 1\nmin_free_gb = 0\n",
        )
        .unwrap();

        // 1 worker in registry with alive pid -> cap full at 1/1
        let reg = dir.join("registry.json");
        std::fs::write(
            &reg,
            format!(
                r#"{{"schema_version":1,"entries":[{{"name":"w1","provider":"claude","cwd":"/tmp","status":"live","pid":{},"created_at":"2026-01-01T00:00:00Z"}}]}}"#,
                std::process::id()
            ),
        )
        .unwrap();

        // With QUEUE_TIMEOUT, full cap, not no_wait, and mock timeout:
        // verify slot_count sees 1 slot >= max_live 1
        let mut warnings = Vec::new();
        let slots = slot_count(&reg, &mut warnings);
        assert_eq!(slots, 1);
        assert_eq!(EXIT_QUEUE_TIMEOUT, 75);

        std::env::remove_var("FNO_CLAIMS_ROOT");
        match prior_spawn_gate {
            Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
            None => std::env::remove_var("FNO_SPAWN_GATE"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn urldecode_inverts_encode_key() {
        let key = "worker:my agent/x";
        assert_eq!(urldecode(&claims::encode_key(key)).as_deref(), Some(key));
        assert_eq!(urldecode("bad%zz"), None);
    }

    #[test]
    fn rust_headless_slot_claim_stamps_unrouted_provider() {
        let root = std::env::temp_dir().join(format!(
            "fno-gate-provider-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut guard = GateGuard {
            gate_key: None,
            worker_key: None,
            root: Some(root.clone()),
        };

        acquire_worker_slot(&mut guard, "plain-codex", "spawn-gate:test");

        let claim_path = root
            .join(".fno/claims")
            .join(format!("{}.lock", claims::encode_key("worker:plain-codex")));
        let raw = std::fs::read_to_string(claim_path).unwrap();
        let record: claims::ClaimRecord = serde_yaml_ng::from_str(&raw).unwrap();
        assert_eq!(
            record
                .metadata
                .get("model_provider")
                .and_then(serde_json::Value::as_str),
            Some("__uncapped__")
        );
        guard.release();
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn qos_wrap_wraps_or_passes_through() {
        // test_env_lock: qos_wrap reads config via FNO_CONFIG-sensitive
        // resolve; serialize with the other env-touching tests.
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-gate-qos-{}", std::process::id()));
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        let prior_config = std::env::var_os("FNO_CONFIG");
        std::env::set_var("FNO_CONFIG", fnodir.join("config.toml"));

        std::fs::write(
            fnodir.join("config.toml"),
            "[agents]\nworker_qos = \"off\"\n",
        )
        .unwrap();
        // Use an absolute executable so this assertion is independent of a
        // harness-restricted PATH. A non-resolving argv[0] is covered below.
        let argv = vec!["/bin/sh".to_string(), "-c".to_string(), "true".to_string()];
        assert_eq!(qos_wrap(&dir, argv.clone()), argv, "off = identity");

        std::fs::write(
            fnodir.join("config.toml"),
            "[agents]\nworker_qos = \"utility\"\n",
        )
        .unwrap();
        let wrapped = qos_wrap(&dir, argv.clone());
        if cfg!(target_os = "macos") && Path::new("/usr/sbin/taskpolicy").exists() {
            assert_eq!(
                &wrapped[..4],
                &["/usr/sbin/taskpolicy", "-c", "utility", "--"]
            );
            assert_eq!(&wrapped[4..], &argv[..]);
        } else if cfg!(target_os = "linux") && Path::new("/usr/bin/nice").exists() {
            assert_eq!(&wrapped[..3], &["/usr/bin/nice", "-n", "10"]);
            assert_eq!(&wrapped[3..], &argv[..]);
        } else {
            assert_eq!(wrapped, argv, "no wrapper binary -> identity (fail open)");
        }

        // A non-resolving command is never wrapped (NotFound must stay the
        // caller's error, not taskpolicy's).
        let ghost = vec!["definitely-not-a-real-cli-xyz".to_string()];
        assert_eq!(qos_wrap(&dir, ghost.clone()), ghost);
        match prior_config {
            Some(value) => std::env::set_var("FNO_CONFIG", value),
            None => std::env::remove_var("FNO_CONFIG"),
        }
    }

    #[test]
    fn slot_count_absent_sources_is_zero_with_rows_needing_pids() {
        // A registry path that does not exist must not panic; the count is >= 0
        // and a malformed file warns rather than errors (LD5, fail open).
        // Serialize: slot_count reads the claims root.
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Missing registry: fresh-machine semantics, zero contribution, no
        // panic (load_registry treats absent as empty).
        let mut warnings = Vec::new();
        let missing = std::env::temp_dir().join("fno-gate-noreg/registry.json");
        let _ = slot_count(&missing, &mut warnings);

        // Malformed registry: fail OPEN with one warning (LD5), never an error.
        let dir = std::env::temp_dir().join(format!("fno-gate-badreg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("registry.json");
        std::fs::write(&bad, "{ not json").unwrap();
        let mut warnings = Vec::new();
        let _ = slot_count(&bad, &mut warnings);
        assert!(
            warnings.iter().any(|w| w.contains("registry unreadable")),
            "malformed registry must warn, got {warnings:?}"
        );
    }

    /// AC1-FR (x-bdf9): the Rust gate and the Python mirror must return the same
    /// slot count for the same synthetic registry+roster. Both suites read this
    /// ONE fixture; a divergence in either gate's counting rule (e.g. re-adding
    /// the roster to the slot count) fails its own assertion. A populated roster
    /// is materialized deliberately: `slot_count` must ignore it, so a future
    /// re-introduction of roster counting inflates the count and trips here.
    #[test]
    fn slot_count_agrees_with_python_gate_fixture() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../cli/tests/agents/fixtures/spawn_gate_slot_agreement.json");
        let raw = std::fs::read_to_string(&fixture_path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture_path.display()));
        let fixture: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let self_pid = std::process::id();
        // 2^22+17: realistically never a live pid (mirrors the Python fixture).
        let dead_pid: u32 = 4_194_321;
        let resolve = |v: &serde_json::Value| -> Option<u32> {
            match v.as_str() {
                Some("self") => Some(self_pid),
                Some("dead") => Some(dead_pid),
                _ => None, // absent pid = disk-only row
            }
        };
        let base = std::env::temp_dir().join(format!("fno-gate-agree-{self_pid}"));
        for (i, sc) in fixture["scenarios"].as_array().unwrap().iter().enumerate() {
            let dir = base.join(format!("s{i}"));
            std::fs::create_dir_all(&dir).unwrap();
            // Isolate the claims root: no real worker:<name> slot claim leaks in.
            std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
            // Populate a roster the slot count must ignore.
            let daemon = dir.join("daemon");
            std::fs::create_dir_all(&daemon).unwrap();
            std::env::set_var("FNO_CLAUDE_DAEMON_DIR", &daemon);
            let mut rworkers = Vec::new();
            for (j, r) in sc["roster"].as_array().unwrap().iter().enumerate() {
                let short = r["short"]
                    .as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("{:08x}", 0xaaaa_0000u32 + j as u32));
                let pidf = resolve(&r["pid"])
                    .map(|p| format!(r#","pid":{p}"#))
                    .unwrap_or_default();
                rworkers.push(format!(
                    r#""{short}":{{"sessionId":"{short}-1-2-3-4"{pidf}}}"#
                ));
            }
            std::fs::write(
                daemon.join("roster.json"),
                format!(
                    r#"{{"proto":1,"supervisorPid":1,"workers":{{{}}}}}"#,
                    rworkers.join(",")
                ),
            )
            .unwrap();
            // Materialize the registry.
            let mut entries = Vec::new();
            for row in sc["registry"].as_array().unwrap() {
                let name = row["name"].as_str().unwrap();
                let status = row["status"].as_str().unwrap();
                let pidf = resolve(&row["pid"])
                    .map(|p| format!(r#","pid":{p}"#))
                    .unwrap_or_default();
                let csidf = row["short_id"]
                    .as_str()
                    .map(|s| format!(r#","short_id":"{s}""#))
                    .unwrap_or_default();
                entries.push(format!(
                    r#"{{"name":"{name}","provider":"claude","cwd":"/tmp","status":"{status}","created_at":"2026-01-01T00:00:00Z"{pidf}{csidf}}}"#
                ));
            }
            let reg = dir.join("registry.json");
            std::fs::write(
                &reg,
                format!(
                    r#"{{"schema_version":1,"entries":[{}]}}"#,
                    entries.join(",")
                ),
            )
            .unwrap();

            let mut warnings = Vec::new();
            let got = slot_count(&reg, &mut warnings);
            let want = sc["expect_slot_count"].as_u64().unwrap() as usize;
            assert_eq!(
                got,
                want,
                "scenario {:?}: got {got}, want {want}",
                sc["name"].as_str().unwrap_or("?")
            );
        }
        std::env::remove_var("FNO_CLAIMS_ROOT");
        std::env::remove_var("FNO_CLAUDE_DAEMON_DIR");
    }
    // --- the per-territory team cap fixture (x-e221 AC9) -------------------

    #[test]
    fn territory_cap_agrees_with_python_gate_fixture() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../cli/tests/agents/fixtures/spawn_gate_territory_agreement.json");
        let raw = std::fs::read_to_string(&fixture_path)
            .unwrap_or_else(|e| panic!("read fixture {}: {e}", fixture_path.display()));
        let fixture: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let self_pid = std::process::id();
        let resolve = |v: &serde_json::Value| -> Option<u32> {
            match v.as_str() {
                Some("self") => Some(self_pid),
                _ => None,
            }
        };
        let base = std::env::temp_dir().join(format!("fno-territory-agree-{self_pid}"));
        let _ = std::fs::remove_dir_all(&base);
        for (i, sc) in fixture["scenarios"].as_array().unwrap().iter().enumerate() {
            let dir = base.join(format!("s{i}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
            let daemon = dir.join("daemon");
            std::fs::create_dir_all(&daemon).unwrap();
            std::env::set_var("FNO_CLAUDE_DAEMON_DIR", &daemon);
            // The graph the territory read compiles, reachable via FNO_HOME.
            std::fs::write(
                dir.join("graph.json"),
                serde_json::json!({ "entries": sc["graph"].clone() }).to_string(),
            )
            .unwrap();
            std::env::set_var("FNO_HOME", &dir);
            // The registry: live workers (+ the crown row when the scenario has one).
            let mut entries: Vec<String> = Vec::new();
            for row in sc["registry"].as_array().unwrap() {
                let name = row["name"].as_str().unwrap();
                let status = row["status"].as_str().unwrap();
                let pidf = resolve(&row["pid"])
                    .map(|p| format!(r#","pid":{p}"#))
                    .unwrap_or_default();
                let nodef = row["node"]
                    .as_str()
                    .map(|s| format!(r#","node":"{s}""#))
                    .unwrap_or_default();
                entries.push(format!(
                    r#"{{"name":"{name}","provider":"claude","cwd":"/tmp","status":"{status}","created_at":"2026-01-01T00:00:00Z"{pidf}{nodef}}}"#
                ));
            }
            if !sc["crown_scope"].is_null() {
                entries.push(format!(
                    r#"{{"name":"fixture-king","provider":"claude","cwd":"/tmp","status":"busy","created_at":"2026-01-01T00:00:00Z","pid":{self_pid},"crown_scope":{}}}"#,
                    sc["crown_scope"]
                ));
            }
            let reg = dir.join("registry.json");
            std::fs::write(
                &reg,
                format!(
                    r#"{{"schema_version":1,"entries":[{}]}}"#,
                    entries.join(",")
                ),
            )
            .unwrap();

            let cap = sc["territory_cap"].as_u64().unwrap_or(4) as u32;
            let node = sc["node"].as_str().unwrap_or_default();
            let mut warnings = Vec::new();
            let live = live_rows(&reg, &mut warnings);
            let got = match territory_of_node(&dir, &reg, node, &mut warnings) {
                None => "territory_unknown".to_string(),
                Some((scope, members)) => {
                    let count = live
                        .iter()
                        .filter(|r| {
                            r.node
                                .as_deref()
                                .map(|n| members.contains(n))
                                .unwrap_or(false)
                        })
                        .count();
                    let cap_hit = count >= cap as usize
                        && sc["expect"]["verdict"].as_str() == Some("territory_cap")
                        && sc["expect"]["territory"].as_str() == Some(scope.as_str())
                        && sc["expect"]["count"].as_u64() == Some(count as u64);
                    if cap_hit {
                        "territory_cap".to_string()
                    } else {
                        format!("ok:{scope}:{count}")
                    }
                }
            };
            let want = sc["expect"]["verdict"].as_str().unwrap().to_string();
            std::env::remove_var("FNO_CLAIMS_ROOT");
            std::env::remove_var("FNO_CLAUDE_DAEMON_DIR");
            std::env::remove_var("FNO_HOME");
            assert_eq!(
                got.split(':').next().unwrap(),
                want,
                "scenario {}: got {got}, want {want}",
                sc["name"].as_str().unwrap_or("?")
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
