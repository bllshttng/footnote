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

use crate::agents_config;
use crate::claims;
use crate::claude_roster::ClaudeRoster;
use crate::daemon::pid_is_ours;
use crate::state::{load_registry, Registry};
use crate::AgentStatus;

/// Exit codes, distinct from existing dispatch codes (2, 13, 14, 15, 18, 127).
pub const EXIT_QUEUE_TIMEOUT: i32 = 75;
pub const EXIT_NO_WAIT: i32 = 76;
pub const EXIT_RAM_REFUSED: i32 = 77;
/// The lane declares nothing about how it stands toward the fno state root
/// (epic rule R3). NOT "declares no carrier": an unsandboxed lane needs none.
pub const EXIT_STATE_ROOT_UNGRANTED: i32 = 78;
pub const EXIT_LOAD_REFUSED: i32 = 79;

/// Queue mechanics (Claude's Discretion 2: targets, not contracts).
const QUEUE_POLL: Duration = Duration::from_secs(2);
const QUEUE_PROGRESS_EVERY: Duration = Duration::from_secs(30);
const QUEUE_TIMEOUT: Duration = Duration::from_secs(600);
/// x-7783 LD4: the CPU-hold re-sample gap and the admission debounce. Longer
/// than the slot poll because the `ps` CPU column is a decaying average on
/// macOS - two reads 2s apart are one sample twice. Mirrors
/// `spawn_gate.py::CPU_HOLD_POLL_S`.
const CPU_HOLD_POLL: Duration = Duration::from_secs(15);
const CPU_ADMIT_SAMPLES: u32 = 2;
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
    // Live roster short_ids: the liveness oracle for pid-less fno bg rows only.
    // A roster read failure degrades this to empty (bg rows then fall back to
    // their local pid, i.e. uncounted) — fail open, never wedge.
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
    let mut count = 0usize;
    match load_registry(registry_path) {
        Ok(Registry { entries, .. }) => {
            for e in &entries {
                if !status_is_liveish(&e.status) {
                    continue;
                }
                let alive = match e.pid {
                    // Local pid: liveness by PID/start-time, same as claims.
                    Some(p) => pid_is_ours(p, e.pid_start_time),
                    // No local pid: a fno bg/adopted row whose process is the
                    // claude daemon's — resolve liveness via the roster by its
                    // jobId (in short_id since v9). (A row without either signal
                    // is a disk-only ghost and stays uncounted.)
                    None => e
                        .transport_short()
                        .map(|sid| live_roster_short_ids.contains(sid))
                        .unwrap_or(false),
                };
                if alive {
                    count += 1;
                }
            }
        }
        Err(e) => warnings.push(format!(
            "spawn-gate: fno registry unreadable ({e}); slot count degraded to 0"
        )),
    }

    count + live_worker_slot_claims(warnings)
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
    // x-7783 AC7: the retired trigger (max_load_per_cpu) is not read here;
    // the CPU axis consumes the payload's admission, which the Python decider
    // computed from its own config read.
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
    // x-7783 LD4: a fleet-over sample holds, and admission after a hold is
    // debounced to CPU_ADMIT_SAMPLES consecutive under-ceiling samples.
    let mut held_on_cpu = false;
    let mut under_streak: u32 = 0;
    // Start of the current UNBROKEN run of failed acquisitions (None = holding
    // or not yet contended). Reset on every success so a long legitimate queue
    // never accumulates into a spurious fail-open.
    let mut mutex_blocked_since: Option<Instant> = None;
    // Axes read so far, accumulating across passes exactly like the Python
    // twin's dict, so the timeout receipt can name what was read (AC13).
    let mut axes_read = serde_json::Map::new();

    loop {
        let mut pause = QUEUE_POLL;
        // The footprint probe runs OUTSIDE the gate mutex (it costs seconds
        // and the mutex serializes every spawner), re-taken each pass so a
        // spawn that held does not decide on a reading from minutes ago.
        // An Err is a real answer (the probe's own failure words) and
        // travels into the refusal (LD3).
        let (prefetched, probe_err) = match footprint_cause_raw() {
            Ok(raw) => (Some(raw), None),
            Err(why) => (None, Some(why)),
        };
        // Serialize check→dispatch under the spawn-gate mutex so N concurrent
        // spawners at cap-1 can't all pass. Not held across the wait sleep.
        let mut acquired_mutex = match claims::acquire(
            "gate:spawn",
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
            guard.gate_key = Some(("gate:spawn".to_string(), holder.clone()));
            // x-7783 Change 3: the CPU axis decides BEFORE the census, so a
            // hold never pays the registry scan and the slot cap stays the
            // backstop behind it (LD1).
            let cpu = check_cpu_axis(prefetched.as_deref(), probe_err.as_deref());
            let admission = &cpu.payload;
            if admission.axis == "load_15m" && admission.verdict == "refuse" {
                axes_read.insert("load_15m".into(), serde_json::json!("over"));
                axes_read.insert("cpu".into(), serde_json::json!("not-read"));
            } else {
                axes_read.insert(
                    "load_15m".into(),
                    serde_json::json!(if admission.load_15m.is_some() {
                        "ok"
                    } else {
                        "unavailable"
                    }),
                );
                axes_read.insert("cpu".into(), serde_json::json!(admission.verdict));
            }
            let receipt_fields = serde_json::json!({
                "axis": admission.axis,
                "detail": admission.reason,
                "share_low": admission.share_low,
                "share_high": admission.share_high,
                "bound": admission.bound,
                "fleet_cores": admission.fleet_cores,
                "machine_cores": admission.machine_cores,
                "capacity_cores": admission.capacity_cores,
                "ceiling": admission.ceiling,
                "load_15m": admission.load_15m,
                "backstop": admission.backstop,
            });
            match admission.verdict.as_str() {
                "refuse" | "undecidable" => {
                    // The refusal is decided; drop the mutex BEFORE printing
                    // so queued spawners (and --no-wait callers) never sit
                    // behind anything.
                    guard.release();
                    eprintln!("{}", admission.reason);
                    let mut receipt = serde_json::json!({
                        "status": "refused",
                        "reason": cpu.token,
                        "axes_read": axes_read.clone(),
                    });
                    for (k, v) in receipt_fields.as_object().into_iter().flatten() {
                        receipt[k] = v.clone();
                    }
                    println!("{receipt}");
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                    return Err(EXIT_LOAD_REFUSED);
                }
                "hold" => {
                    // LD4: over is a HOLD - the fleet's own work drains - not
                    // a refusal. Re-sample on the slower CPU poll; --no-wait
                    // fails on the first over sample.
                    held_on_cpu = true;
                    under_streak = 0;
                    guard.release_gate_mutex();
                    if flags.no_wait {
                        eprintln!("{}", admission.reason);
                        let mut receipt = serde_json::json!({
                            "status": "refused",
                            "reason": "fleet_cpu_share",
                            "samples": 1,
                            "held_on": "fleet_cpu_share",
                            "axes_read": axes_read.clone(),
                        });
                        for (k, v) in receipt_fields.as_object().into_iter().flatten() {
                            receipt[k] = v.clone();
                        }
                        println!("{receipt}");
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                        return Err(EXIT_LOAD_REFUSED);
                    }
                    if !announced {
                        eprintln!("{}", admission.reason);
                        announced = true;
                        last_progress = Instant::now();
                    } else if last_progress.elapsed() >= QUEUE_PROGRESS_EVERY {
                        eprintln!(
                            "still held: fleet {:.1}% over {:.1}%, waited {}s",
                            admission.share_low * 100.0,
                            admission.ceiling * 100.0,
                            started.elapsed().as_secs()
                        );
                        last_progress = Instant::now();
                    }
                    pause = CPU_HOLD_POLL;
                }
                "admit" => {
                    // A held spawn needs CPU_ADMIT_SAMPLES consecutive
                    // under-ceiling samples before it believes the drain
                    // (LD4); a spawn that was never held admits on the first.
                    let mut hold_pause = false;
                    if held_on_cpu {
                        under_streak += 1;
                        if under_streak < CPU_ADMIT_SAMPLES {
                            guard.release_gate_mutex();
                            pause = CPU_HOLD_POLL;
                            hold_pause = true;
                        } else {
                            eprintln!(
                                "spawn-gate: fleet share {:.1}% under the ceiling for \
                                 {under_streak} consecutive samples; admitting",
                                admission.share_low * 100.0
                            );
                            // The hold is served. Clear it so a later timeout
                            // names the queue actually eating the budget and
                            // queued passes do not reprint the admit line.
                            held_on_cpu = false;
                            under_streak = 0;
                        }
                    }
                    if !hold_pause {
                        let mut warnings = Vec::new();
                        let slots = slot_count(registry_path, &mut warnings);
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
                            // Stamped only once the floor actually answered, so a
                            // receipt never claims an axis it did not read.
                            axes_read.insert("ram".into(), serde_json::json!("ok"));
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
                        axes_read.insert(
                            "slots".into(),
                            serde_json::json!(format!("{slots}/{cap} queued")),
                        );

                        if flags.no_wait {
                            eprintln!(
                                "spawn-gate: {slots} live worker slots >= max_live {cap}; a quiet \
                                 row still holds a slot (fno agents list --status quiet); \
                                 refusing (--no-wait). See `fno agents top`."
                            );
                            println!(
                                "{}",
                                serde_json::json!({
                                    "status": "refused",
                                    "reason": "no_wait",
                                    "axis": "max_live",
                                    "axes_read": axes_read.clone(),
                                    "held_on": "max_live",
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
                                "spawn queued: {slots} live worker slots >= max_live {cap}; a quiet \
                                 row still holds a slot (fno agents list --status quiet); waiting \
                                 for a free slot (--no-wait to fail fast, --force to bypass)"
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
                }
                other => {
                    // An unknown verdict word is an unreadable instrument, not
                    // an admit: fail closed (LD3).
                    guard.release();
                    eprintln!(
                        "spawn-gate: the CPU instrument is unreadable (the payload carries the \
                         unknown verdict {other:?}); refusing to spawn (--force to bypass)"
                    );
                    println!(
                        "{}",
                        serde_json::json!({
                            "status": "refused",
                            "reason": "cpu_instrument_unreadable",
                            "axis": "cpu_instrument",
                            "axes_read": axes_read.clone(),
                        })
                    );
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                    return Err(EXIT_LOAD_REFUSED);
                }
            }
        }

        if started.elapsed() >= QUEUE_TIMEOUT {
            let held_on = if mutex_blocked_since.is_some() {
                "gate_mutex"
            } else if held_on_cpu {
                "fleet_cpu_share"
            } else {
                "max_live"
            };
            let reason = if mutex_blocked_since.is_some() {
                "gate_mutex_busy"
            } else {
                "queue_timeout"
            };
            eprintln!(
                "spawn-gate: {reason} after {}s held on {held_on}; \
                 inspect live workers with `fno agents top`, or retry with --no-wait/--force",
                QUEUE_TIMEOUT.as_secs()
            );
            println!(
                "{}",
                serde_json::json!({
                    "status": "refused",
                    "reason": reason,
                    "held_on": held_on,
                    "axis": held_on,
                    "axes_read": axes_read.clone(),
                    "max_live": cap,
                    "count": last_slots,
                    "current_count": last_slots,
                })
            );
            use std::io::Write;
            let _ = std::io::stdout().flush();
            return Err(EXIT_QUEUE_TIMEOUT);
        }
        std::thread::sleep(pause);
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

/// x-7783 Change 3: the payload's `admission` object, computed by the ONE
/// Python decider (`cpu_admission`) and consumed verbatim by this gate. The
/// Rust gate computes no verdict of its own.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AdmissionPayload {
    verdict: String,
    axis: String,
    reason: String,
    #[serde(default)]
    share_low: f64,
    #[serde(default)]
    share_high: f64,
    #[serde(default)]
    bound: String,
    #[serde(default)]
    fleet_cores: f64,
    #[serde(default)]
    machine_cores: f64,
    #[serde(default)]
    capacity_cores: f64,
    #[serde(default)]
    ceiling: f64,
    #[serde(default)]
    gap: Option<String>,
    #[serde(default)]
    load_15m: Option<f64>,
    #[serde(default)]
    backstop: f64,
}

#[derive(Debug, Default, Deserialize)]
pub struct FootprintCausePayload {
    /// Kept only so an older admission-less payload still parses; the verdict
    /// comes from `admission` now, never from the gap's presence.
    #[serde(default)]
    attribution_gap: Option<String>,
    /// The Claude Code background daemon's idle pre-warm pool, for the
    /// `fno agents status` machine line.
    #[serde(default)]
    spare_pool_process_count: u64,
    #[serde(default)]
    spare_pool_cpu_cores: f64,
    /// 1-min load average, for the status line only. It decides nothing
    /// anywhere (x-7783 LD1).
    #[serde(default)]
    load_1m: Option<f64>,
    #[serde(default)]
    cpu_capacity_cores: f64,
    /// The `_emit_failure` shape: when footprint cannot measure at all it
    /// still answers, carrying this key and exit 4. Its words travel into
    /// the instrument refusal (x-7783 keeps that contract).
    #[serde(default)]
    error: Option<String>,
    /// The decider's answer. Absent on a degraded payload: the gate refuses
    /// as `cpu_instrument_unreadable` rather than guessing (LD3).
    #[serde(default)]
    admission: Option<AdmissionPayload>,
    /// The whole-machine band's verdict from the ONE Python decider
    /// (`machine_pressure`); the machine_watch arm reads it verbatim (x-d6ad
    /// LD3). Absent on a degraded payload: the arm reads that as
    /// `machine_unreadable`, never as calm.
    #[serde(default)]
    pub(crate) machine: Option<MachinePressurePayload>,
    /// Top fleet consumers by summed ps %cpu; the machine_watch escalation
    /// names the first three by their own argv strings (x-d6ad AC7).
    #[serde(default)]
    pub(crate) top: Vec<TopConsumer>,
}

impl FootprintCausePayload {
    /// The arm-and-test seam: a payload carrying only the machine verdict and
    /// the top consumers; everything else defaults.
    #[cfg(test)]
    pub(crate) fn from_parts(
        machine: Option<MachinePressurePayload>,
        top: Vec<TopConsumer>,
    ) -> Self {
        Self {
            machine,
            top,
            ..Default::default()
        }
    }
}

/// One `top` row of the footprint payload.
#[derive(Debug, Clone, Deserialize)]
pub struct TopConsumer {
    #[serde(default)]
    pub(crate) cpu_percent: f64,
    #[serde(default)]
    pub(crate) command: String,
}

/// The payload's `machine` object (x-d6ad LD3/LD4): computed by
/// `machine_pressure` in doctor_footprint.py, read verbatim here. This module
/// computes no machine verdict of its own.
#[derive(Debug, Clone, Deserialize)]
pub struct MachinePressurePayload {
    pub(crate) verdict: String,
    #[serde(default)]
    pub(crate) reason: String,
    #[serde(default)]
    pub(crate) busy_fraction: Option<f64>,
    #[serde(default)]
    pub(crate) band: f64,
    #[serde(default)]
    pub(crate) machine_cores: Option<f64>,
    #[serde(default)]
    pub(crate) capacity_cores: f64,
    #[serde(default)]
    pub(crate) runnable: Option<u64>,
    #[serde(default)]
    pub(crate) processes: Option<u64>,
    #[serde(default)]
    pub(crate) load_15m: Option<f64>,
    #[serde(default)]
    pub(crate) throttle_minutes: u64,
}

/// The CPU axis's answer for THIS spawn: the admission to branch on plus the
/// receipt `reason` token a refusal carries (AC13). A synthetic instrument
/// refusal is built when the payload carries no decidable admission.
struct CpuAdmission {
    payload: AdmissionPayload,
    /// refuse|undecidable -> load_backstop | cpu_share_undecidable |
    /// cpu_instrument_unreadable. Empty for admit/hold (they never refuse).
    token: &'static str,
}

/// Read the CPU axis from the prefetched footprint payload (x-7783 LD3).
///
/// The Python decider `cpu_admission` (doctor_footprint.py) is the ONE
/// decider; this gate maps its `admission.verdict` to the same four branches
/// the Python gate takes and prints `admission.reason` verbatim. No payload,
/// an unparseable payload, or a payload without `admission` refuses as
/// `cpu_instrument_unreadable`: the sensor blinding under the load it
/// measures is itself a symptom, and an unknown share is not headroom. The
/// probe's own failure words (`probe_err`) travel into that refusal.
fn check_cpu_axis(prefetched: Option<&str>, probe_err: Option<&str>) -> CpuAdmission {
    fn instrument_refusal(why: &str) -> CpuAdmission {
        CpuAdmission {
            payload: AdmissionPayload {
                verdict: "refuse".to_string(),
                axis: "cpu_instrument".to_string(),
                reason: format!(
                    "spawn-gate: the CPU instrument is unreadable ({why}); \
                     refusing to spawn (--force to bypass)"
                ),
                share_low: 0.0,
                share_high: 0.0,
                bound: "exact".to_string(),
                fleet_cores: 0.0,
                machine_cores: 0.0,
                capacity_cores: 0.0,
                ceiling: 0.0,
                gap: None,
                load_15m: None,
                backstop: 0.0,
            },
            token: "cpu_instrument_unreadable",
        }
    }
    let raw = match prefetched {
        Some(raw) => raw,
        None => {
            return instrument_refusal(
                probe_err.unwrap_or("the footprint probe produced no payload"),
            );
        }
    };
    let payload: FootprintCausePayload = match serde_json::from_str(raw) {
        Ok(payload) => payload,
        Err(_) => {
            return instrument_refusal("the footprint probe wrote an unparseable payload");
        }
    };
    // An answered failure (`{"error": ..., "exit_code": 4}`) is a different
    // fact from a probe that never answered: its words travel (main's
    // contract, kept under the admission regime).
    if let Some(err) = payload.error {
        return instrument_refusal(&err);
    }
    match payload.admission {
        Some(admission) => {
            let token = match (admission.verdict.as_str(), admission.axis.as_str()) {
                ("undecidable", _) => "cpu_share_undecidable",
                ("refuse", "load_15m") => "load_backstop",
                ("refuse", _) => "cpu_instrument_unreadable",
                _ => "",
            };
            CpuAdmission {
                payload: admission,
                token,
            }
        }
        None => instrument_refusal("the payload carries no admission"),
    }
}

/// Wall-clock budget for the out-of-process footprint probe: the Python
/// twin's 5s measurement budget plus an allowance for a ONE-module
/// interpreter start. The allowance is not sized for a full CLI boot on
/// purpose: the probe binary (`fno-footprint-cause`) imports only
/// `fno.doctor_footprint` (measured 0.11s wall at load 117), where the old
/// `fno` shim route paid a full typer-app import plus provisioning waits and
/// timed out under exactly the load this gate exists to measure.
const FOOTPRINT_PROBE_BUDGET: Duration = Duration::from_secs(8);

/// The probe argv: the narrow console script when it resolves, else the
/// same `--json --cause-only` reading through `fno-py` (version skew: an
/// older wheel without the script). `fno` itself is deliberately NOT a
/// candidate: it is the Rust shim, and a gate probe must not route through
/// its provisioning waits. `None` means no probe resolves, and the refusal
/// names that instead of pretending the instrument answered.
fn footprint_probe_argv() -> Option<Vec<String>> {
    if resolves_on_path("fno-footprint-cause") {
        return Some(vec!["fno-footprint-cause".to_string()]);
    }
    if !resolves_on_path("fno-py") {
        return None;
    }
    let mut argv = crate::king_board::fno_py_cmd();
    argv.extend(
        ["doctor", "footprint", "--json", "--cause-only"]
            .iter()
            .map(|s| s.to_string()),
    );
    Some(argv)
}

/// The status footer's reading and the store keeper's path note, from ONE
/// footprint probe (x-d6ad): `(machine line, keeper note)`. Both best-effort
/// - a machine whose footprint cannot be read yields `(None, None)`, never a
/// stale or fabricated line.
pub fn machine_reading_notes() -> (Option<String>, Option<String>) {
    // ONE parse serves both notes; the raw string is never read twice.
    let payload: Option<FootprintCausePayload> = footprint_cause_raw()
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let footer = payload.as_ref().and_then(|p| machine_footer_line(p));
    let keeper = payload.as_ref().and_then(|payload| {
        let commands: Vec<String> = payload.top.iter().map(|c| c.command.clone()).collect();
        crate::drift::keeper_path_note(&commands, std::env::current_exe().ok().as_deref())
    });
    (footer, keeper)
}

/// The footer line from a parsed payload. It reads the `machine` object - the
/// ONE Python decider's verdict - and leads with the busy fraction against
/// the band, never with a bare load figure (x-d6ad AC9/LD2).
fn machine_footer_line(payload: &FootprintCausePayload) -> Option<String> {
    let machine = payload.machine.as_ref()?;
    let load = machine
        .load_15m
        .filter(|v| v.is_finite())
        .map(|v| format!("{v:.1}"))
        .unwrap_or_else(|| "unknown".to_string());
    let pool = if payload.spare_pool_process_count > 0 {
        format!(
            " claude_spare_pool={}proc/{:.2}cores",
            payload.spare_pool_process_count, payload.spare_pool_cpu_cores
        )
    } else {
        String::new()
    };
    match machine.busy_fraction {
        Some(busy) => Some(format!(
            "{:.0}% busy of {:.2} cores against band {:.0}% -> {} · load_15m {} · \
             {} runnable of {} processes{pool}",
            busy * 100.0,
            machine.capacity_cores,
            machine.band * 100.0,
            machine.verdict,
            load,
            machine.runnable.unwrap_or(0),
            machine.processes.unwrap_or(0)
        )),
        None => Some(format!("{} · load_15m {load}{pool}", machine.verdict)),
    }
}

/// The test seam for the footer: the same formatter over a raw payload string.
#[cfg(test)]
fn format_machine_status_line(raw: &str) -> Option<String> {
    machine_footer_line(&serde_json::from_str(raw).ok()?)
}

pub(crate) fn footprint_cause_raw() -> Result<String, String> {
    let argv = footprint_probe_argv().ok_or_else(|| {
        "no footprint probe resolves on PATH (fno-footprint-cause, fno-py)".to_string()
    })?;
    footprint_cause_raw_with(&argv, FOOTPRINT_PROBE_BUDGET)
}

/// The transport, split from [`footprint_cause_raw`] so tests can pass a
/// short budget and a script instead of loading a real machine.
///
/// `Err` carries WHY the probe has no answer, as text a refusal can print.
/// A deadline miss is a fact about the probe's clock, never about the
/// machine: printing a clock miss as if it were a CPU reading is how a
/// healthy instrument once read as "attribution unavailable" at load 511
/// while the same instrument, read in process, answered 3.69/12.00 in 1.6s.
fn footprint_cause_raw_with(argv: &[String], budget: Duration) -> Result<String, String> {
    let bin = argv[0].clone();
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("footprint probe {bin} could not run: {e}"))?;
    let deadline = Instant::now() + budget;
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
                let output = child
                    .wait_with_output()
                    .map_err(|e| format!("footprint probe {bin} could not run: {e}"))?;
                return std::str::from_utf8(&output.stdout)
                    .map(|s| s.to_string())
                    .map_err(|_| format!("footprint probe {bin} wrote non-UTF-8 output"));
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let budget_s = if budget.as_secs() > 0 {
                    format!("{}s", budget.as_secs())
                } else {
                    format!("{}ms", budget.as_millis())
                };
                return Err(format!(
                    "footprint probe {bin} did not answer inside {budget_s}; \
                     that is the probe's clock, not a reading of fleet CPU"
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("footprint probe {bin} could not run: {e}"));
            }
        }
    }
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

    /// The `fno agents status` machine line: the busy fraction against the
    /// band, the verdict, and the load and runnable census beside them
    /// (x-d6ad AC9), with the pool named so a caller sees the pool's share
    /// before a spawn is ever refused on it.
    #[test]
    fn machine_status_line_names_band_verdict_and_census() {
        let raw = r#"{"fleet_cpu_cores":0.06,"cpu_capacity_cores":12,"fleet_percent_capacity":0.5,"fleet_percent_measured_cpu":1.2,"spare_pool_process_count":45,"spare_pool_cpu_cores":7.98,"machine":{"verdict":"hot","reason":"r","busy_fraction":0.917,"band":0.9,"machine_cores":11.0,"capacity_cores":12.0,"runnable":160,"processes":1100,"load_15m":279.12,"throttle_minutes":30}}"#;
        let line = format_machine_status_line(raw).expect("payload formats");
        assert_eq!(
            line,
            "92% busy of 12.00 cores against band 90% -> hot · load_15m 279.1 · \
             160 runnable of 1100 processes claude_spare_pool=45proc/7.98cores"
        );
    }

    /// Negative control: no pool, no load reading. The line still prints -
    /// best-effort status is not all-or-nothing on one field.
    #[test]
    fn machine_status_line_omits_pool_and_reads_load_unknown() {
        let raw = r#"{"cpu_capacity_cores":12,"machine":{"verdict":"calm","reason":"r","busy_fraction":0.432,"band":0.9,"machine_cores":5.186,"capacity_cores":12.0,"runnable":66,"processes":1010,"load_15m":null,"throttle_minutes":60}}"#;
        let line = format_machine_status_line(raw).expect("payload formats");
        assert_eq!(
            line,
            "43% busy of 12.00 cores against band 90% -> calm · load_15m unknown · \
             66 runnable of 1010 processes"
        );
    }

    /// An absent machine object yields no line at all rather than a
    /// fabricated one, and the verdict-only shape prints for an unreadable
    /// sensor (busy_fraction null).
    #[test]
    fn machine_status_line_is_none_without_a_machine_object() {
        assert_eq!(format_machine_status_line("{}"), None);
        assert_eq!(format_machine_status_line("not json"), None);
        assert_eq!(
            format_machine_status_line(r#"{"cpu_capacity_cores":12}"#),
            None
        );
        let unreadable = r#"{"cpu_capacity_cores":12,"machine":{"verdict":"unreadable","reason":"footprint probe did not answer inside 8s","busy_fraction":null,"band":0.9,"machine_cores":null,"capacity_cores":12.0,"runnable":null,"processes":null,"load_15m":null,"throttle_minutes":60}}"#;
        let line = format_machine_status_line(unreadable).expect("payload formats");
        assert!(line.starts_with("unreadable · load_15m unknown"), "{line}");
    }

    /// x-7783 AC9: the shared fixture pins the branch this gate takes per
    /// payload. The Python suite feeds the same file to `cpu_admission`, so
    /// neither runtime can grow its own opinion about who gets in.
    #[test]
    fn admission_payload_branches_agree_with_python_fixture() {
        let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../cli/tests/agents/fixtures/spawn_gate_admission.json");
        let raw = std::fs::read_to_string(&fixture_path)
            .expect("the shared fixture must exist beside the Python suite");
        let doc: serde_json::Value = serde_json::from_str(&raw).expect("fixture is JSON");
        let cases = doc["cases"].as_array().expect("fixture carries cases");
        assert_eq!(cases.len(), 4, "one case per AC branch");
        for case in cases {
            let expected = case["payload"]["admission"]["verdict"].as_str().unwrap();
            let payload = serde_json::to_string(&case["payload"]).unwrap();
            let cpu = check_cpu_axis(Some(&payload), None);
            let got = cpu.payload.verdict.as_str();
            assert_eq!(got, expected, "case {} drifted", case["name"]);
            assert_eq!(
                cpu.payload.reason, case["payload"]["admission"]["reason"],
                "case {} must print the payload's sentence verbatim",
                case["name"]
            );
            let token = if got == "undecidable" {
                "cpu_share_undecidable"
            } else if got == "refuse" {
                cpu.token
            } else {
                ""
            };
            assert_eq!(cpu.token, token, "case {} token", case["name"]);
        }
    }

    /// Junk, an admission-less payload, and no payload at all all refuse as
    /// the unreadable instrument (LD3) - never as an idle machine; the
    /// probe's own failure words travel into the sentence.
    #[test]
    fn junk_and_admission_less_payloads_refuse_as_unreadable() {
        let cases = [
            (None, Some("footprint probe fno did not answer inside 8s")),
            (Some("{}"), None),
            (Some("not json"), None),
            (
                Some(r#"{"fleet_cpu_cores":0.79,"cpu_capacity_cores":12}"#),
                None,
            ),
        ];
        for (payload, err) in cases {
            let cpu = check_cpu_axis(payload, err);
            assert_eq!(cpu.payload.verdict, "refuse", "{payload:?}");
            assert_eq!(cpu.payload.axis, "cpu_instrument", "{payload:?}");
            assert_eq!(cpu.token, "cpu_instrument_unreadable", "{payload:?}");
            assert!(
                cpu.payload.reason.contains("--force to bypass"),
                "{payload:?}"
            );
        }
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
            "gate:spawn",
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
            "gate:spawn",
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
        let _ = claims::release("gate:spawn", "spawn-gate:999999:ghost", Some(&root), None);
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
}
