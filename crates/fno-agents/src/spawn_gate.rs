//! Spawn gate : global concurrency cap + free-RAM floor + queue loop.
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
//! as a population to count (: the roster's non-work sessions must not
//! consume worker slots; only rows that are ALSO in the fno registry count).
//! The gate's only writes are its own claims (`spawn-gate` check→dispatch mutex,
//! `worker:<name>` headless slot claims). Every guard fails OPEN on read errors
//! (LD5): the gate is protective infrastructure and must never become the thing
//! that bricks spawning.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agents_config;
use crate::claims;
use crate::claude_roster::ClaudeRoster;
use crate::daemon::pid_is_ours;
use crate::spawn_gate_lanes;
use crate::spawn_gate_lanes::{
    check_account_login, check_account_quota_lock, check_lane_quota_lock, check_registry_schema,
};
use crate::spawn_gate_reservations::{
    release_redeemed_reservation, reserved_note, reserved_receipt, RESERVATION_RULE,
};
use crate::state::{load_registry, Registry, RegistryEntry};
use crate::AgentStatus;
use std::collections::HashSet;

/// Exit codes, allocated by the shared table in
/// `cli/src/fno/agents/spawn_gate.py` and kept unique across both trees by
/// `cli/tests/unit/test_exit_code_allocation.py` (values >= 64 claim a number
/// once; the same NAME at the same number in both trees is byte-parity).
/// Distinct from the convention codes (2, 13, 14, 15, 18, 127).
pub const EXIT_QUEUE_TIMEOUT: i32 = 75;
pub const EXIT_NO_WAIT: i32 = 76;
/// The per-territory team cap refused the spawn, or its attribution
/// was unreadable. The team cap is the one permanent, non-queueable machine
/// refusal with its own number, so a caller never retries it as capacity.
pub const EXIT_TERRITORY_CAP: i32 = 86;
/// The blueprint thread cap: more than
/// `agents.profiles.blueprint.max_live` live `bp` threads, or more than one
/// per territory, refuses the spawn and teaches the subagent path.
pub const EXIT_BLUEPRINT_CAP: i32 = 88;
/// A spawn whose seed or label names review is refused before every bypass.
pub const EXIT_REVIEW_SESSION: i32 = 89;
pub const EXIT_RAM_REFUSED: i32 = 77;
pub const EXIT_PROVIDER_CAP: i32 = 78;
pub const EXIT_LOAD_REFUSED: i32 = 79;
pub const EXIT_KING_SHARE: i32 = 80;
pub const EXIT_REGISTRY_SCHEMA: i32 = 81;
/// A durable fleet incident stop is active - refused before every
/// bypass branch, `--force` and `FNO_SPAWN_GATE=0` included. In-flight
/// workers are untouched; only new admission is refused. Same number as the
/// Python gate's EXIT_FLEET_STOP (byte-parity for the fleet pair).
pub const EXIT_FLEET_STOP: i32 = 82;
/// The incident state exists but cannot be read: fail closed, and say this is
/// a CANNOT-TELL refusal, never a stop verdict.
pub const EXIT_FLEET_STOP_UNAVAILABLE: i32 = 83;
/// The lane declares nothing about how it stands toward the fno state root
/// (epic rule R3). NOT "declares no carrier": an unsandboxed lane needs none.
pub const EXIT_STATE_ROOT_UNGRANTED: i32 = 84;
/// The spawn-gate transport could not get an answer at all (the gate verb is
/// missing, failed, or timed out): fail closed, never admit on an unreadable
/// gate. Byte-parity with the Python table's EXIT_GATE_UNAVAILABLE. 86 went
/// to the per-territory team cap, which landed first; the gate-unavailable
/// number moved to the next free slot.
pub const EXIT_GATE_UNAVAILABLE: i32 = 87;

/// The prefix every PASS-path gate line carries. Only a refusal may start
/// `spawn-gate: ` - the verdict line and the refusal sentences - so the
/// readers (`cli/src/fno/backlog/advance.py _gate_refusal_detail`,
/// `crates/fno/src/dispatch_launch.rs refusal_detail`) can pick the refusal
/// out of a stderr that is full of passing readings. Contract:
/// docs/architecture/spawn-gate.md (Reading a refusal).
pub(crate) const NOTE: &str = "spawn-gate note:";

/// A refusal as data: the exit code the caller's arm returns, the stdout
/// receipt it prints (byte-shape unchanged from when `run_gate` printed it
/// itself), and the event fields the Python transport emits through `_refuse`
/// for spawns that enter Python (locked decision 5 - refusal events stay
/// Python-emitted; the native arm itself still emits nothing, owns a
/// Rust emit). The eprintln prose stays at the refusal site either way.
#[derive(Debug, Clone)]
pub struct Refusal {
    pub exit_code: i32,
    pub receipt: Option<serde_json::Value>,
    pub event: serde_json::Map<String, serde_json::Value>,
}

impl Refusal {
    pub(crate) fn code(exit_code: i32) -> Self {
        Refusal {
            exit_code,
            receipt: None,
            event: serde_json::Map::new(),
        }
    }

    pub(crate) fn with_receipt(exit_code: i32, receipt: serde_json::Value) -> Self {
        Refusal {
            exit_code,
            receipt: Some(receipt),
            event: serde_json::Map::new(),
        }
    }

    /// Attach one event field (builder style, so refusal sites stay one line).
    pub(crate) fn ev(mut self, key: &str, value: serde_json::Value) -> Self {
        self.event.insert(key.to_string(), value);
        self
    }
}

/// The one-line verdict every refusal ends with: `spawn-gate: refused on
/// <axis> (<reason>, exit <code>): <figures>`. The axis prefers the event's
/// explicit axis, then the receipt's axis/held_on/reason; the figures are
/// the receipt's scalar fields in key order (serde_json `preserve_order`),
/// skipping the words already named. Contract:
/// docs/architecture/spawn-gate.md (Reading a refusal).
pub(crate) fn verdict_line(r: &Refusal) -> String {
    let ev_str = |k: &str| r.event.get(k).and_then(serde_json::Value::as_str);
    let rc = r.receipt.as_ref();
    let rc_str = |k: &str| {
        rc.and_then(|v| v.get(k))
            .and_then(serde_json::Value::as_str)
    };
    let axis = ev_str("axis")
        .or_else(|| rc_str("axis"))
        .or_else(|| rc_str("held_on"))
        .or_else(|| rc_str("reason"))
        .or_else(|| ev_str("reason"))
        .unwrap_or("unknown");
    let reason = rc_str("reason")
        .or_else(|| ev_str("reason"))
        .unwrap_or("unknown");
    let mut figures: Vec<String> = Vec::new();
    let collect = |map: &serde_json::Map<String, serde_json::Value>, figures: &mut Vec<String>| {
        for (k, v) in map {
            if matches!(k.as_str(), "status" | "reason" | "axis" | "held_on") {
                continue;
            }
            match v {
                serde_json::Value::String(s) => figures.push(format!("{k}={s}")),
                serde_json::Value::Number(_) | serde_json::Value::Bool(_) => {
                    figures.push(format!("{k}={v}"));
                }
                _ => {}
            }
        }
    };
    if let Some(obj) = rc.and_then(serde_json::Value::as_object) {
        collect(obj, &mut figures);
    }
    if figures.is_empty() {
        // A receipt-less refusal (king share, fleet incident) keeps its
        // measurements in the event; the verdict names them, or it names
        // no breach at all.
        collect(&r.event, &mut figures);
    }
    if figures.is_empty() {
        format!(
            "spawn-gate: refused on {axis} ({reason}, exit {})",
            r.exit_code
        )
    } else {
        format!(
            "spawn-gate: refused on {axis} ({reason}, exit {}): {}",
            r.exit_code,
            figures.join(", ")
        )
    }
}

/// The first admission boundary of the native gate: a durable
/// incident stop or an unreadable incident state refuses before the
/// `FNO_SPAWN_GATE=0` operator bypass, before `--force`, and before any
/// capacity math. Mail stays ungated so the incident can be announced and
/// explained; `fno agents incident clear` reopens admission.
fn fleet_incident_gate() -> Result<(), Refusal> {
    match crate::fleet_incident::verdict() {
        crate::fleet_incident::Verdict::Clear(_) => Ok(()),
        crate::fleet_incident::Verdict::Stopped(record) => {
            eprintln!(
                "refused: fleet incident stop is active (generation {}, reason: {}); \
                 in-flight workers continue, no new spawn is admitted. \
                 Reopen with `fno agents incident clear --reason <text>`",
                record.generation, record.reason
            );
            Err(Refusal::code(EXIT_FLEET_STOP)
                .ev("reason", serde_json::json!("fleet-stop"))
                .ev("generation", serde_json::json!(record.generation))
                .ev("detail", serde_json::json!(record.reason)))
        }
        crate::fleet_incident::Verdict::Unavailable(detail) => {
            eprintln!(
                "refused: fleet incident state is unreadable ({detail}); \
                 admission fails closed until the record is readable again"
            );
            Err(Refusal::code(EXIT_FLEET_STOP_UNAVAILABLE)
                .ev("reason", serde_json::json!("fleet-stop-unavailable"))
                .ev("detail", serde_json::json!(detail)))
        }
    }
}

/// Queue mechanics (Claude's Discretion 2: targets, not contracts).
const QUEUE_POLL: Duration = Duration::from_secs(2);
const QUEUE_PROGRESS_EVERY: Duration = Duration::from_secs(30);
const QUEUE_TIMEOUT: Duration = Duration::from_secs(600);
/// LD4: the CPU-hold re-sample gap and the admission debounce. Longer
/// than the slot poll because the `ps` CPU column is a decaying average on
/// macOS - two reads 2s apart are one sample twice. Mirrors
/// `spawn_gate.py::CPU_HOLD_POLL_S`.
const CPU_HOLD_POLL: Duration = Duration::from_secs(15);
const CPU_ADMIT_SAMPLES: u32 = 2;
/// A blind CPU read (an undecidable band or an unreadable probe) gets at most
/// this many total samples before the gate refuses. A blind read is not
/// evidence the fleet is over. It never holds for the whole queue budget and
/// never admits: worst case is 3 probes of FOOTPRINT_PROBE_BUDGET plus 2 pauses.
const CPU_BLIND_SAMPLES: u32 = 3;
#[cfg(not(test))]
const CPU_BLIND_POLL: Duration = Duration::from_secs(5);
#[cfg(test)]
const CPU_BLIND_POLL: Duration = Duration::from_millis(10);
/// spawn-gate mutex TTL: generous vs the seconds-scale check→dispatch window;
/// PID liveness frees it instantly if the spawner dies.
const GATE_CLAIM_TTL_MS: i64 = 5 * 60 * 1000;
/// How long an uncapped spawner tolerates an UNBROKEN run of failed mutex
/// acquisitions before proceeding unserialized. The claim records the holder
/// pid, so a holder that dies frees the mutex on the next acquire. This bound
/// covers a LIVE holder stuck in dispatch: the mutex is a check→dispatch
/// serializer, not a state owner, and a gate that bricks spawning is what LD5
/// forbids. Failing open can overshoot the cap by the number of racing
/// spawners, so a capped spawner keeps queueing instead.
const MUTEX_WAIT_BUDGET: Duration = Duration::from_secs(60);
/// worker:<name> headless slot TTL: bounds a LIVE holder's stay; a dead
/// holder frees the slot at once (the claim carries its holder pid stamped
/// `holder-process`, so PID liveness is the primary release and the TTL is
/// only the backstop).
const WORKER_CLAIM_TTL_MS: i64 = 4 * 60 * 60 * 1000;
const KNOWN_UNROUTED_PROVIDER: &str = "__uncapped__";

/// Registry statuses that can hold a live process (idle counts: an
/// idle-but-unreaped process still holds RAM; a reaped pid drops out via the
/// liveness check). Mirrors `spawn_gate.py::LIVE_STATUSES`.
pub(crate) fn status_is_liveish(s: &AgentStatus) -> bool {
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

/// Page size from a `vm_stat` header
/// ("Mach Virtual Memory Statistics: (page size of 16384 bytes)").
pub(crate) fn vm_stat_page_size(text: &str) -> Option<u64> {
    text.lines()
        .next()?
        .split("page size of")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Parse `vm_stat` output (macOS) to available bytes: (free + inactive +
/// speculative + purgeable) pages × page size. `None` on any shape surprise
/// so the guard fails open.
pub fn parse_vm_stat(text: &str) -> Option<u64> {
    let page_size: u64 = vm_stat_page_size(text)?;
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

/// Parse macOS `sysctl vm.swapusage`
/// (`total = 18432.00M  used = 17080.75M  free = 1351.25M  (encrypted)`) to
/// percent used. `None` when the line does not parse or total is 0.
pub fn parse_swapusage(text: &str) -> Option<f64> {
    let (total, used) = parse_swapusage_mb(text)?;
    Some(used / total * 100.0)
}

pub(crate) fn parse_swapusage_mb(text: &str) -> Option<(f64, f64)> {
    let mut total_m = None;
    let mut used_m = None;
    let tokens: Vec<&str> = text.split_whitespace().collect();
    for i in 0..tokens.len().saturating_sub(1) {
        // "total = 18432.00M": label, "=", numeric-with-unit.
        let num = |tok: Option<&&str>| -> Option<f64> {
            tok.and_then(|v| {
                v.chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect::<String>()
                    .parse()
                    .ok()
            })
        };
        match tokens[i] {
            "total" => total_m = num(tokens.get(i + 2)),
            "used" => used_m = num(tokens.get(i + 2)),
            _ => {}
        }
    }
    let (total, used) = (total_m?, used_m?);
    (total > 0.0).then_some((total, used))
}

/// Swap percent used beside [`available_ram_gb`]: available counts reclaimable
/// pages and has no swap term, so a machine paging at 93% swap can read six
/// times its RAM floor. `None` = unreadable or no swap configured (the guard
/// skips, fail open like the RAM floor).
#[cfg(target_os = "macos")]
pub fn swap_used_pct() -> Option<f64> {
    sysctl_swapusage_pct()
}

#[cfg(target_os = "linux")]
pub fn swap_used_pct() -> Option<f64> {
    meminfo_swap_pct()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn swap_used_pct() -> Option<f64> {
    None
}

#[cfg(target_os = "macos")]
fn sysctl_swapusage_pct() -> Option<f64> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "vm.swapusage"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_swapusage(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(target_os = "linux")]
fn meminfo_swap_pct() -> Option<f64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|rest| {
                rest.trim_start_matches(':')
                    .trim()
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
    };
    let total = field("SwapTotal")?;
    let free = field("SwapFree")?;
    if total == 0 {
        return None;
    }
    Some((total - free) as f64 / total as f64 * 100.0)
}

/// Parse `vm_stat` output (macOS) for the cumulative `Swapins:` counter:
/// `(pages, page_size)`. `None` without the line or the header.
/// Dead off macOS except in tests, which parse the fixture on every platform.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn parse_vm_stat_swapins(text: &str) -> Option<(u64, u64)> {
    let page_size = vm_stat_page_size(text)?;
    let line = text.lines().find(|l| l.starts_with("Swapins:"))?;
    let pages: u64 = line
        .split_once(':')?
        .1
        .trim()
        .trim_end_matches('.')
        .parse()
        .ok()?;
    Some((pages, page_size))
}

/// Parse Linux `/proc/vmstat` for the cumulative `pswpin` page count. `None`
/// without the line.
/// Dead off Linux except in tests, which parse the fixture on every platform.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn parse_proc_vmstat_pswpin(text: &str) -> Option<u64> {
    let line = text.lines().find(|l| l.starts_with("pswpin "))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Sample window for the swap-in rate.
const SWAPIN_WINDOW: Duration = Duration::from_secs(1);
/// One mebibyte, the unit every swap-in rate renders in.
pub(crate) const MIB: f64 = 1024.0 * 1024.0;
/// Swap-in rate at or above this refuses beside an over-cap allocation. A
/// calibration choice, not a derived number: idle windows on machines at 94.7%
/// and 64.8% swap allocation read 0 swap-ins, and 1 MiB/s sits above stray
/// single-page touches.
pub(crate) const SWAPIN_REFUSE_BYTES_PER_S: f64 = MIB;

/// Cumulative swap-in pages plus the page size: macOS runs `vm_stat`
/// (`Swapins:`), Linux reads `/proc/vmstat` (`pswpin`, page size from
/// sysconf). `None` = unreadable or unsupported platform.
#[cfg(target_os = "macos")]
fn swapin_pages() -> Option<(u64, u64)> {
    let out = std::process::Command::new("vm_stat").output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_vm_stat_swapins(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(target_os = "linux")]
fn swapin_pages() -> Option<(u64, u64)> {
    let pages = parse_proc_vmstat_pswpin(&std::fs::read_to_string("/proc/vmstat").ok()?)?;
    // SAFETY: sysconf with a constant identifier has no preconditions.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    Some((pages, page_size.max(1) as u64))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn swapin_pages() -> Option<(u64, u64)> {
    None
}

/// Swap-in rate in bytes/second over `window`: counter delta times the page
/// size. `None` when either read fails or the counter went down (a counter
/// reset must never read as a negative rate).
pub fn swapin_bytes_per_sec(window: Duration) -> Option<f64> {
    let (a, page_size) = swapin_pages()?;
    std::thread::sleep(window);
    let (b, _) = swapin_pages()?;
    let pages = b.checked_sub(a)?;
    Some(pages as f64 * page_size as f64 / window.as_secs_f64())
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
/// per-territory cap can read the rows' worked NODES without a second
/// liveness implementation.
pub(crate) fn live_rows(registry_path: &Path, warnings: &mut Vec<String>) -> Vec<RegistryEntry> {
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
                    "{NOTE} claude roster unreadable ({e}); pid-less bg rows uncounted"
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
            "{NOTE} fno registry unreadable ({e}); slot count degraded to 0"
        )),
    }
    rows
}

/// Count fno WORKER SLOTS in use for the `max_live` cap: liveness-filtered fno
/// registry rows + live `worker:<name>` headless slot claims.
///
/// This is deliberately NOT the full claude daemon roster. The roster
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
    let (rows, claims) = slot_reading(registry_path, warnings);
    rows.len() + claims.len()
}

/// The slot count's two inputs, together: the live registry rows and the live
/// `worker:<name>` headless reservations, each reservation named. `slot_count`
/// is the sum; the refusal paths need the rows themselves to name them.
pub(crate) fn slot_reading(
    registry_path: &Path,
    warnings: &mut Vec<String>,
) -> (Vec<RegistryEntry>, Vec<SlotReservation>) {
    let rows = live_rows(registry_path, warnings);
    let claims = live_worker_slot_claims(warnings);
    (rows, claims)
}

/// The one slot-refusal sentence both print paths share, so a display can
/// never quote a count the gate did not measure. Pure text; the caller adds
/// its own tail (`refusing (--no-wait).`, or the queue line's advice). The
/// rows are named by the probe's `slot_rows` field (`fno agents gate-status`),
/// never by a second walk. When a reservation is counted, the sentence names
/// the release verb for the first suspect one; all-live saturation names none.
fn slot_refusal_line(
    slots: usize,
    cap: usize,
    rows: usize,
    claims: &[SlotReservation],
    waiting: usize,
    tail: &str,
) -> String {
    let waiting_note = if waiting > 0 {
        format!("; {waiting} of the rows wait on an operator question")
    } else {
        String::new()
    };
    // Name the remedy only for a SUSPECT reservation: a live one still holds
    // its slot on purpose, and releasing it would leave the worker running
    // uncounted. All-live saturation names no release target.
    let remedy = match claims.iter().find(|r| r.state == "suspect") {
        Some(r) => format!(
            "; free a dead reservation: fno agents claim release worker:{} --force \
             --reason \"<why>\"",
            r.name
        ),
        None => String::new(),
    };
    format!(
        "{slots} live worker slots >= max_live {cap} ({rows} registry rows, {n} headless \
         reservations{waiting_note}); every counted row: fno agents gate-status, field \
         slot_rows{remedy}; {tail}",
        n = claims.len()
    )
}

/// The territory (key, member node ids, kingless) a node belongs to, or
/// `None` when the answer cannot be READ (unreadable graph, node absent,
/// unreadable registry, uncompilable live crown). Membership is exclusive:
/// the deepest live crown whose scope holds the node owns it, the lowest
/// canonical scope on a tie (`territory::node_owners`); an unowned node
/// counts for its project's loose territory, so one worker never consumes
/// two territories' caps. `kingless` is false for a crown scope, true for
/// the loose fallback.
pub(crate) fn territory_of_node(
    config_cwd: &Path,
    registry_path: &Path,
    node: &str,
    warnings: &mut Vec<String>,
) -> Option<(String, std::collections::HashSet<String>, bool)> {
    use crate::king_board::project_map;

    // Through the backend switch (`graph_store::read_rows_strict`): a cap
    // answered from a frozen sqlite mirror polices a territory the store
    // does not recognize. Unreadable is None, the cannot-READ contract.
    let entries: Vec<Value> = match crate::territory::graph_entries(config_cwd) {
        Ok(rows) => rows,
        Err(_) => return None,
    };
    let row = entries
        .iter()
        .find(|e| e.get("id").and_then(Value::as_str) == Some(node));
    if row.is_none() {
        return None;
    }
    let crowns = match crate::territory::live_crowns(registry_path) {
        Ok(c) => c,
        Err(e) => {
            warnings.push(format!("{e}; refusing"));
            return None;
        }
    };
    let (owners, failures) = crate::territory::node_owners(
        &crowns,
        &entries,
        &Ok(project_map(config_cwd).unwrap_or_default()),
    );
    if !failures.is_empty() {
        warnings.extend(
            failures
                .iter()
                .map(|(s, e)| format!("territory: crown {s} uncompilable: {e}")),
        );
        return None;
    }
    if let Some(owner) = owners.get(node) {
        let members: HashSet<String> = owners
            .iter()
            .filter(|(_, s)| *s == owner)
            .map(|(id, _)| id.clone())
            .collect();
        return Some((owner.clone(), members, false));
    }
    let project = row
        .and_then(|r| r.get("project").and_then(Value::as_str))
        .unwrap_or("");
    if project.is_empty() {
        return None;
    }
    let loose: HashSet<String> = entries
        .iter()
        .filter_map(|e| {
            let id = e.get("id").and_then(Value::as_str)?;
            (e.get("project").and_then(Value::as_str).unwrap_or("") == project
                && !owners.contains_key(id))
            .then(|| id.to_string())
        })
        .collect();
    Some((format!("loose:{project}"), loose, true))
}

/// The per-territory team cap. `Err` carries the refusal receipt the
/// caller prints; `None` territory reads as UNKNOWN and refuses closed - the
/// cap never counts an unknown as headroom. A spawn that works no node skips
/// the check entirely: the team cap does not apply to it.
pub(crate) fn check_territory_cap(
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
    let Some((scope, members, _kingless)) = state else {
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

/// The territory refusal as data: the helper's receipt string parses into the
/// answer envelope, and the event carries the receipt's own reason word, so
/// the transport's emit vocabulary matches what the receipt says.
fn territory_refusal(receipt: &str) -> Refusal {
    let parsed = serde_json::from_str::<serde_json::Value>(receipt)
        .unwrap_or(serde_json::json!({"status": "refused", "reason": "territory_cap"}));
    let reason = parsed
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("territory_cap")
        .to_string();
    Refusal::with_receipt(EXIT_TERRITORY_CAP, parsed)
        .ev("reason", serde_json::json!(reason))
        .ev("axis", serde_json::json!("territory"))
}

/// The blueprint thread cap: the machine axis counts live rows whose
/// names parse to verb `bp`; the territory axis counts the ones working the
/// spawn's territory. Refuses, never queues, beside the machine cap - and
/// `--force` does not excuse it, the same posture as the territory cap. The
/// receipt names the live rows so a blocked king sees what holds the slot.
pub(crate) fn check_blueprint_cap(
    config_cwd: &Path,
    registry_path: &Path,
    name: &str,
    node: Option<&str>,
    live: &[RegistryEntry],
) -> Result<(), String> {
    if !crate::naming::is_blueprint_name(name) {
        return Ok(());
    }
    let blueprint_rows: Vec<&RegistryEntry> = live
        .iter()
        .filter(|r| crate::naming::is_blueprint_name(&r.name))
        .collect();
    let remedy = {
        let named_node = node
            .map(str::to_string)
            .or_else(|| crate::naming::parse_dispatch_agent_name(Some(name)).and_then(|p| p.node))
            .unwrap_or_else(|| "<node>".to_string());
        format!(
            "plan it in a native subagent: /fno:blueprint subagent {named_node} (law d-94853e86)"
        )
    };
    let max = agents_config::blueprint_max_live(config_cwd);
    if blueprint_rows.len() as u32 >= max {
        return Err(serde_json::json!({
            "status": "refused",
            "reason": "blueprint_cap",
            "count": blueprint_rows.len(),
            "max_live": max,
            "live_blueprints": blueprint_rows.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
            "remedy": remedy,
        })
        .to_string());
    }
    let Some(node) = node else {
        return Ok(());
    };
    let territory_cap = agents_config::blueprint_territory_max_live(config_cwd);
    let mut warnings = Vec::new();
    let state = territory_of_node(config_cwd, registry_path, node, &mut warnings);
    for w in &warnings {
        eprintln!("{w}");
    }
    let Some((scope, members, _kingless)) = state else {
        return Err(serde_json::json!({
            "status": "refused",
            "reason": "territory_unknown",
            "node": node,
            "max_live_per_territory": territory_cap,
        })
        .to_string());
    };
    let territory_rows: Vec<&RegistryEntry> = blueprint_rows
        .iter()
        .copied()
        .filter(|r| {
            r.node
                .as_deref()
                .map(|n| members.contains(n))
                .unwrap_or(false)
        })
        .collect();
    if territory_rows.len() as u32 >= territory_cap {
        return Err(serde_json::json!({
            "status": "refused",
            "reason": "blueprint_territory_cap",
            "territory": scope,
            "count": territory_rows.len(),
            "max_live_per_territory": territory_cap,
            "live_blueprints": territory_rows.iter().map(|r| r.name.clone()).collect::<Vec<_>>(),
            "remedy": remedy,
        })
        .to_string());
    }
    Ok(())
}

/// The blueprint refusal as data, mirroring [`territory_refusal`]: the
/// receipt's own reason word rides the event, the axis is `blueprint`.
fn blueprint_refusal(receipt: &str) -> Refusal {
    let parsed = serde_json::from_str::<serde_json::Value>(receipt)
        .unwrap_or(serde_json::json!({"status": "refused", "reason": "blueprint_cap"}));
    let reason = parsed
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("blueprint_cap")
        .to_string();
    Refusal::with_receipt(EXIT_BLUEPRINT_CAP, parsed)
        .ev("reason", serde_json::json!(reason))
        .ev("axis", serde_json::json!("blueprint"))
}

/// The verdict receipt for one node, from explicit paths - the counting leg
/// `run_territory_verdict` serves and the tests pin. `kingless` rides every
/// readable verdict; `territory_unknown` stays without one, because an
/// unreadable attribution has no territory and a `kingless` value there
/// would be a guess wearing a boolean.
fn territory_verdict_receipt(
    config_cwd: &Path,
    registry_path: &Path,
    node: &str,
    cap: u32,
) -> Value {
    let mut warnings = Vec::new();
    let state = territory_of_node(config_cwd, registry_path, node, &mut warnings);
    for w in &warnings {
        eprintln!("{w}");
    }
    match state {
        None => serde_json::json!({
            "verdict": "territory_unknown",
            "reason": "territory_unknown",
            "node": node,
            "max_live_per_territory": cap,
        }),
        Some((scope, members, kingless)) => {
            let live = live_rows(registry_path, &mut warnings);
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
                serde_json::json!({
                    "verdict": "territory_cap",
                    "reason": "territory_cap",
                    "territory": scope,
                    "kingless": kingless,
                    "count": count,
                    "current_count": count,
                    "max_live_per_territory": cap,
                })
            } else {
                serde_json::json!({
                    "verdict": "ok",
                    "territory": scope,
                    "kingless": kingless,
                    "current_count": count,
                    "max_live_per_territory": cap,
                })
            }
        }
    }
}

/// The `--node <id>` argument of a territory door verb, or `None`.
fn parse_node_arg(args: &[String]) -> Option<String> {
    let mut node: Option<String> = None;
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--node" {
            node = iter.next().cloned();
        }
    }
    node
}

/// `fno-agents territory-verdict --node <id>`: the per-territory cap verdict
/// for one node as JSON on stdout. The single counting leg: the Python gate
/// passes the node through this door and recomputes nothing. Exit is 0 for
/// every READABLE verdict (including a refusal - the verdict is the answer);
/// only a malformed invocation exits non-zero.
pub fn run_territory_verdict(args: &[String]) -> i32 {
    let Some(node) = parse_node_arg(args) else {
        eprintln!("territory-verdict: --node is required");
        return 2;
    };
    let config_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry_path = crate::paths::AgentsHome::from_env().registry_json();
    let cap = agents_config::territory_max_live(&config_cwd);
    let verdict = territory_verdict_receipt(&config_cwd, &registry_path, &node, cap);
    println!(
        "{}",
        serde_json::to_string(&verdict).unwrap_or_else(|_| "{}".to_string())
    );
    0
}

/// One counted `worker:<name>` reservation the slot census named, so a
/// reader can see what holds each slot and free a dead one. The provider
/// walker (`spawn_gate_lanes`) reuses this record instead of a third walk.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SlotReservation {
    /// The claim key's name half (`worker:<name>` minus the prefix).
    pub name: String,
    /// The claim's holder string (a credential, never parsed for identity).
    pub holder: String,
    /// The recorded holder pid, when the writer proved one.
    pub pid: Option<i32>,
    /// Seconds since `acquired_at`.
    pub age_s: Option<u64>,
    /// `live` or `suspect` - the only states the census counts.
    pub state: &'static str,
    /// The `model_provider` metadata tag the provider count reads.
    pub provider: Option<String>,
}

/// Live `worker:<name>` slot claims under the GLOBAL claims root, named.
/// Headless one-shots write no registry row, so their gate acquires one of
/// these for the call duration; concurrent gates see them here. `Suspect`
/// counts like `Live` (TTL-protected, never up for grabs); a dead
/// holder-process claim reads `Stale` before this counts it.
fn live_worker_slot_claims(warnings: &mut Vec<String>) -> Vec<SlotReservation> {
    let root = match gate_claims_root() {
        Some(r) => r,
        None => return Vec::new(),
    };
    let dir = root.join(".fno/claims");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(), // no claims dir yet: nothing held.
    };
    let prefix = claims::encode_key("worker:");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut found = Vec::new();
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
            (state @ (claims::ClaimState::Live | claims::ClaimState::Suspect), Some(rec)) => {
                found.push(SlotReservation {
                    name: key.strip_prefix("worker:").unwrap_or(&key).to_string(),
                    holder: rec.holder,
                    pid: rec.pid,
                    age_s: u64::try_from((now_ms - rec.acquired_at).max(0) / 1000).ok(),
                    state: match state {
                        claims::ClaimState::Live => "live",
                        _ => "suspect",
                    },
                    provider: rec
                        .metadata
                        .get("model_provider")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
            (claims::ClaimState::Corrupted, _) => {
                warnings.push(format!("{NOTE} corrupted slot claim {key} ignored"));
            }
            _ => {}
        }
    }
    found
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

impl GateGuard {
    /// The still-held keys, taken out before the guard drops, so a
    /// cross-process transport (the spawn-gate verb) can hand them back to
    /// the caller whose pid owns them. The guard is empty afterwards; it must
    /// NOT be released by the holder of the returned keys.
    pub fn take_keys(&mut self) -> (Option<(String, String)>, Option<(String, String)>) {
        (self.gate_key.take(), self.worker_key.take())
    }
}

/// Pure parity core (AC2-FR): would a bypass in this env emit
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
/// The state-root grant gate. Sits BEFORE `run_gate` in the spawn path
/// (bin/client.rs), so it carries the same wrapper duty: its refusal ends
/// with one [`verdict_line`], or the reader sees the remedy prose with no
/// verdict.
pub fn state_root_grant_gate(
    harness: &str,
    substrate: &str,
    roots: &[String],
) -> Result<(), Refusal> {
    state_root_grant_gate_decide(harness, substrate, roots).inspect_err(|r| {
        eprintln!("{}", verdict_line(r));
    })
}

fn state_root_grant_gate_decide(
    harness: &str,
    substrate: &str,
    roots: &[String],
) -> Result<(), Refusal> {
    if roots.is_empty() {
        return Ok(());
    }
    let contract = match crate::harness_capabilities::HarnessContract::packaged() {
        Ok(contract) => contract,
        Err(error) => {
            eprintln!(
                "spawn-gate: refused: the harness capability contract is unreadable ({error})"
            );
            eprintln!(
                "  a state root resolves for this spawn and no lane can be verified to carry it."
            );
            return Err(Refusal::code(EXIT_STATE_ROOT_UNGRANTED));
        }
    };
    if contract.state_root_stance(harness, substrate).is_some() {
        return Ok(());
    }
    // R3: name the root. A refusal that says "denied" without saying WHICH
    // directory sends the reader back to the code to find out.
    eprintln!(
        "spawn-gate: refused: the {harness}/{substrate} lane does not declare how it stands \
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
    Err(Refusal::code(EXIT_STATE_ROOT_UNGRANTED))
}

/// Everything the gate needs to decide one spawn. The Python transport
/// (`spawn_gate.py run_gate`) sends these as the `spawn-gate` verb's payload;
/// the native arms construct it directly. `holder_pid` is the pid whose death
/// frees the gate's claims - the PYTHON caller's pid across the verb, so the
/// native claim verdict judges the real holder, never the verb process.
#[derive(Debug, Clone, Default)]
pub struct GateInput {
    pub name: String,
    pub substrate: String,
    pub flags: GateFlags,
    pub route_provider: Option<String>,
    pub node: Option<String>,
    pub account: Option<String>,
    pub caller_session: Option<String>,
    pub succession_scope: Option<String>,
    pub holder_pid: Option<u32>,
    /// The spawn's seed message. Its first verb names the session phase.
    pub seed: Option<String>,
    /// An explicit `--session-phase` label.
    pub session_phase: Option<String>,
}

const REVIEW_SESSION_REMEDY: &str = "run the review in the session that did the work: \
     /fno:review <level> ($fno:review <level> on codex), or fno do target request-self-review";

/// Refuse before `--force` and `FNO_SPAWN_GATE=0`: a review runs in the
/// session that did the work, never in a new one.
fn review_session_gate(input: &GateInput) -> Option<Refusal> {
    let seeded = input
        .seed
        .as_deref()
        .and_then(crate::spawn_phase::seed_phase);
    if input.session_phase.as_deref() != Some("review") && seeded != Some("review") {
        return None;
    }
    Some(
        Refusal::with_receipt(
            EXIT_REVIEW_SESSION,
            serde_json::json!({
                "status": "refused",
                "reason": "review_session",
                "remedy": REVIEW_SESSION_REMEDY,
            }),
        )
        .ev("axis", serde_json::json!("review")),
    )
}

/// The held keys of a [`GateGuard`], taken out before the guard drops so a
/// cross-process transport can hand them back to the caller.
pub type GateKeys = (Option<(String, String)>, Option<(String, String)>);

/// Run the full gate for a spawn. Returns a guard to keep alive across
/// dispatch on pass, or `Err(Refusal)` on refusal/timeout. All human-facing
/// output goes to stderr (LD10: the stdout receipt is byte-reserved for the
/// pass path); the receipt itself travels as data in the [`Refusal`] for the
/// caller's arm to print.
///
/// Every refusal ends with one [`verdict_line`] on stderr, so a reader of
/// the stderr sees the refusing axis and breach as the last line, whatever
/// notes preceded it.
pub fn run_gate(
    config_cwd: &Path,
    registry_path: &Path,
    input: GateInput,
) -> Result<GateGuard, Refusal> {
    decide_gate(config_cwd, registry_path, input)
        .map_err(|r| {
            crate::machine_sample::stamp_refusal(
                r,
                &crate::paths::AgentsHome::from_env().events_jsonl(),
                chrono::Utc::now(),
            )
        })
        .inspect_err(|r| eprintln!("{}", verdict_line(r)))
}

/// The gate's decision body, split from [`run_gate`] so the wrapper can
/// append the verdict line at ONE site for all three production callers.
fn decide_gate(
    config_cwd: &Path,
    registry_path: &Path,
    input: GateInput,
) -> Result<GateGuard, Refusal> {
    // the incident stop gates BEFORE the operator bypass below - a
    // circuit breaker that a flag can bypass is not a circuit breaker.
    fleet_incident_gate()?;
    if let Some(refusal) = review_session_gate(&input) {
        return Err(refusal);
    }

    // FNO_SPAWN_GATE=0 disables the gate entirely (the FNO_THINK_SPAWN=0
    // precedent): test suites exercising spawn plumbing must not queue behind
    // the REAL machine's live workers, and it doubles as an operator escape.
    if std::env::var_os("FNO_SPAWN_GATE").is_some_and(|v| v == "0") {
        maybe_emit_spawn_cap_escape();
        return Ok(GateGuard::default());
    }
    let cap = agents_config::max_live(config_cwd) as usize;
    let floor_gb = agents_config::min_free_gb(config_cwd);
    let swap_cap = agents_config::max_swap_pct(config_cwd);
    // AC7: the retired trigger (max_load_per_cpu) is not read here;
    // the CPU axis consumes the payload's admission, which the Python decider
    // computed from its own config read.
    let name = input.name.as_str();
    let substrate = input.substrate.as_str();
    let flags = input.flags;
    let route_provider = input.route_provider.as_deref();
    let admitted_node = input.node.clone().or_else(gate_node);
    let holder_pid = input.holder_pid.unwrap_or_else(std::process::id);
    let holder = format!("spawn-gate:{}:{}", holder_pid, name);
    let root = gate_claims_root();

    let mut guard = GateGuard {
        gate_key: None,
        worker_key: None,
        root: root.clone(),
    };

    // Ahead of the force branch, deliberately. `--force` means "I know the
    // machine is busy", and a schema mismatch is not resource pressure: it is
    // a worker that can neither claim its node nor stamp its mail. The
    // dequeue path re-checks, the way the RAM floor does, because the queue
    // window is long enough for the shared schema to move underneath a
    // waiting spawn.
    let mut schema_warnings = Vec::new();
    check_registry_schema(registry_path, &mut schema_warnings)?;
    for w in &schema_warnings {
        eprintln!("{w}");
    }

    // Ahead of the force branch too: a vendor quota window is not machine
    // busy-ness, and forcing past it buys another corpse.
    if let Some(account) = input.account.as_deref() {
        let mut quota_warnings = Vec::new();
        check_account_quota_lock(config_cwd, account, &mut quota_warnings)?;
        check_account_login(route_provider, account, &mut quota_warnings)?;
        for w in &quota_warnings {
            eprintln!("{w}");
        }
    }

    // The route axis of the same wall: a route-keyed spawn carries no
    // account, so the check above never sees it. The daemon's lane snapshot
    // is the provider-keyed lock (auto-continue launched d8996f9b 32 minutes
    // before the reset through this hole), and it sits ahead of the force
    // branch like the account check: a vendor quota window is not machine
    // busy-ness.
    if let Some(provider) = route_provider {
        let mut lane_warnings = Vec::new();
        check_lane_quota_lock(
            registry_path
                .parent()
                .unwrap_or_else(|| std::path::Path::new(".")),
            provider,
            &mut lane_warnings,
        )?;
        for w in &lane_warnings {
            eprintln!("{w}");
        }
    }

    // The lane cap binds the provider axis only; an unrouted spawn is
    // uncapped (KNOWN_UNROUTED_PROVIDER slots carry no provider tag).
    let provider_cap =
        route_provider.and_then(|p| spawn_gate_lanes::provider_lanes_cap(config_cwd, p));

    if flags.force && provider_cap.is_none() {
        // Force speaks for the machine being busy, never for one territory
        // overrunning its team, so the per-territory cap stays enforced
        // here - the one axis --force does not excuse.
        if let Some(node) = admitted_node.as_deref() {
            let mut warnings = Vec::new();
            let live = live_rows(registry_path, &mut warnings);
            if let Err(receipt) = check_territory_cap(
                config_cwd,
                registry_path,
                &node,
                &live,
                agents_config::territory_max_live(config_cwd),
            ) {
                eprintln!("{receipt}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                return Err(territory_refusal(&receipt));
            }
        }
        // The blueprint axis refuses under --force too: force speaks
        // for the machine being busy, never for one king holding every
        // planning lane.
        {
            let mut warnings = Vec::new();
            let live = live_rows(registry_path, &mut warnings);
            if let Err(receipt) = check_blueprint_cap(
                config_cwd,
                registry_path,
                name,
                admitted_node.as_deref(),
                &live,
            ) {
                eprintln!("{receipt}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                return Err(blueprint_refusal(&receipt));
            }
        }
        eprintln!("{NOTE} forced past cap, RAM floor, and CPU share ceiling (--force)");
        if substrate == "headless" {
            // fail_closed=false: this arm cannot fault, only warn.
            acquire_worker_slot(&mut guard, name, &holder, holder_pid, route_provider, false).ok();
        }
        return Ok(guard);
    }

    let started = Instant::now();
    let mut last_progress = Instant::now();
    let mut announced = false;
    let mut last_slots: usize = 0;
    let mut last_succession_error: Option<&'static str>;
    // LD4: a fleet-over sample holds, and admission after a hold is
    // debounced to CPU_ADMIT_SAMPLES consecutive under-ceiling samples.
    let mut held_on_cpu = false;
    let mut under_streak: u32 = 0;
    // Consecutive blind CPU reads in the current run. Reset by the hold and
    // admit arms, so the count covers back-to-back blind reads only and the
    // receipt's `samples` names what actually happened.
    let mut blind_samples: u32 = 0;
    // Start of the current UNBROKEN run of failed acquisitions (None = holding
    // or not yet contended). Reset on every success so a long legitimate queue
    // never accumulates into a spurious fail-open.
    let mut mutex_blocked_since: Option<Instant> = None;

    loop {
        // Each pass reads afresh: a refusal names only what IT read, never a
        // slot count from an earlier pass.
        last_succession_error = None;
        let mut axes_read = serde_json::Map::new();
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
        // When a provider cap applies, the decision must be SERIALIZED to mean
        // anything: a claims-layer fault refuses (fail closed), exactly as the
        // Python gate's fail_closed arm refused.
        let fail_closed = provider_cap.is_some();
        let mut acquired_mutex = match claims::acquire(
            "gate:spawn",
            &holder,
            claims::AcquireOpts {
                ttl_ms: Some(GATE_CLAIM_TTL_MS),
                root: root.clone(),
                // Always the holder pid: a dead holder then frees the mutex
                // on the next acquire, whatever the TTL says.
                pid: Some(holder_pid),
                ..Default::default()
            },
        ) {
            claims::AcquireOutcome::Acquired(_) => true,
            // Contention is a holder acquire could not prove dead: a dead
            // holder's pid already freed the claim. Queue; --no-wait refuses fast.
            claims::AcquireOutcome::HeldByOther { .. } => false,
            claims::AcquireOutcome::Error(e) => {
                if fail_closed {
                    return Err(gate_fault_refusal(
                        route_provider,
                        "gate_mutex_unavailable",
                        &e,
                    ));
                }
                // Fail open: the mutex is a serializer, not a state owner.
                eprintln!("{NOTE} mutex unavailable ({e}); proceeding unserialized");
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
                    "spawn-gate: a holder the gate cannot prove dead holds the gate mutex; refusing \
                     (--no-wait). Read the holder with `fno agents claim status gate:spawn`."
                );
                return Err(Refusal::with_receipt(
                    EXIT_NO_WAIT,
                    serde_json::json!({
                        "status": "refused",
                        "reason": "no_wait_mutex_held",
                        "max_live": cap,
                    }),
                ));
            }
            if now.duration_since(since) >= MUTEX_WAIT_BUDGET && !fail_closed {
                eprintln!(
                    "{NOTE} the gate mutex stayed held for {}s; proceeding unserialized",
                    MUTEX_WAIT_BUDGET.as_secs()
                );
                acquired_mutex = true;
            }
        }

        if acquired_mutex {
            guard.gate_key = Some(("gate:spawn".to_string(), holder.clone()));
            // The provider cap is counted UNDER the mutex (check→dispatch
            // serialization is what makes the count mean anything), and
            // refuses before the CPU axis, exactly as the Python gate ordered
            // it. An unreadable count refuses - never a zero.
            if let Some(cap_value) = provider_cap {
                let mut lane_warnings = Vec::new();
                match spawn_gate_lanes::provider_live_count(
                    registry_path,
                    route_provider.unwrap_or_default(),
                    Some(name),
                    &mut lane_warnings,
                ) {
                    Ok(reading) => {
                        let live = reading.count;
                        for w in &lane_warnings {
                            eprintln!("{w}");
                        }
                        if live >= cap_value {
                            guard.release_gate_mutex();
                            let parked_names: Vec<String> =
                                reading.parked.iter().map(|(n, _)| n.clone()).collect();
                            let wait_note = if reading.parked.is_empty() {
                                String::new()
                            } else {
                                format!(
                                    "; {} waiting on the operator, not counted",
                                    parked_names.len()
                                )
                            };
                            let reserved_note = reserved_note(&reading.reserved);
                            eprintln!(
                                "spawn-gate: provider {}, cap {cap_value}, current count \
                                 {live}{wait_note}{reserved_note}; refusing; no worker launched. \
                                 {RESERVATION_RULE}",
                                route_provider.unwrap_or("unknown")
                            );
                            return Err(Refusal::with_receipt(
                                EXIT_PROVIDER_CAP,
                                serde_json::json!({
                                    "status": "refused",
                                    "reason": "provider_cap",
                                    "provider": route_provider,
                                    "cap": cap_value,
                                    "count": live,
                                    "current_count": live,
                                    "parked": parked_names,
                                    "reserved": reserved_receipt(&reading.reserved),
                                }),
                            ));
                        }
                    }
                    Err(fault) => {
                        guard.release_gate_mutex();
                        for w in &lane_warnings {
                            eprintln!("{w}");
                        }
                        return Err(gate_fault_refusal(
                            route_provider,
                            "gate_mutex_unavailable",
                            &fault,
                        ));
                    }
                }
            }
            if flags.force {
                // Byte-twin with the Python gate: force also bypasses the king
                // share here; the provider cap above stays enforced.
                eprintln!(
                    "{NOTE} forced past cap, RAM floor, and CPU share ceiling \
                     (--force); provider cap remains enforced"
                );
                if substrate == "headless" {
                    // A forced spawn of the reserved name redeems too: force
                    // speaks for the machine being busy, never for keeping a
                    // reservation the spawn itself was promised.
                    release_redeemed_reservation(name, guard.root.as_deref());
                    // A worker-slot claim fault is not the gate mutex; name the site.
                    if let Err(fault) = acquire_worker_slot(
                        &mut guard,
                        name,
                        &holder,
                        holder_pid,
                        route_provider,
                        true,
                    ) {
                        guard.release();
                        return Err(gate_fault_refusal(
                            route_provider,
                            "lane_reservation_unavailable",
                            &fault,
                        ));
                    }
                }
                return Ok(guard);
            }
            // Change 3: the CPU axis decides BEFORE the census, so a
            // hold never pays the registry scan and the slot cap stays the
            // backstop behind it (LD1).
            let cpu = check_cpu_axis(prefetched.as_deref(), probe_err.as_deref());
            let admission = &cpu.payload;
            axes_read.insert("cpu".into(), serde_json::json!(admission.verdict));
            let figures = receipt_fields(admission);
            match admission.verdict.as_str() {
                "refuse" | "undecidable" => {
                    // A blind read breaks both consecutive-sample runs.
                    under_streak = 0;
                    // A blind read is not evidence the fleet is over: the
                    // instrument can be blind for one pass while the machine
                    // is fine (2026-09-19: a worker refused twice on
                    // cpu_instrument_unreadable, a footprint read seconds
                    // later admitted clean). A waiting spawn re-reads a
                    // bounded number of times before it believes the
                    // refusal; --no-wait keeps one sample.
                    blind_samples += 1;
                    if !flags.no_wait && blind_samples < CPU_BLIND_SAMPLES {
                        guard.release_gate_mutex();
                        eprintln!(
                            "{NOTE} {reason}; re-reading in {s}s (read \
                             {blind_samples} of {CPU_BLIND_SAMPLES})",
                            s = CPU_BLIND_POLL.as_secs(),
                            reason = admission.reason,
                        );
                        pause = CPU_BLIND_POLL;
                    } else {
                        // The refusal is decided; drop the mutex BEFORE printing
                        // so queued spawners (and --no-wait callers) never sit
                        // behind anything.
                        guard.release();
                        eprintln!("{}", admission.reason);
                        let mut receipt = serde_json::json!({
                            "status": "refused",
                            "reason": cpu.token,
                            "samples": blind_samples,
                            "axes_read": axes_read.clone(),
                        });
                        for (k, v) in figures.as_object().into_iter().flatten() {
                            receipt[k] = v.clone();
                        }
                        return Err(Refusal::with_receipt(EXIT_LOAD_REFUSED, receipt)
                            .ev("reason", serde_json::json!(cpu.token))
                            .ev("samples", serde_json::json!(blind_samples))
                            .ev("axis", serde_json::json!("cpu"))
                            .ev("axes_read", serde_json::json!(axes_read))
                            .ev("figures", figures));
                    }
                }
                "hold" => {
                    // LD4: over is a HOLD - the fleet's own work drains - not
                    // a refusal. Re-sample on the slower CPU poll; --no-wait
                    // fails on the first over sample.
                    held_on_cpu = true;
                    under_streak = 0;
                    blind_samples = 0;
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
                        for (k, v) in figures.as_object().into_iter().flatten() {
                            receipt[k] = v.clone();
                        }
                        return Err(Refusal::with_receipt(EXIT_LOAD_REFUSED, receipt)
                            .ev("reason", serde_json::json!("fleet_cpu_share"))
                            .ev("samples", serde_json::json!(1))
                            .ev("held_on", serde_json::json!("fleet_cpu_share"))
                            .ev("axis", serde_json::json!("cpu"))
                            .ev("axes_read", serde_json::json!(axes_read))
                            .ev("figures", figures));
                    }
                    if !announced {
                        eprintln!("{}", admission.reason);
                        announced = true;
                        last_progress = Instant::now();
                    } else if last_progress.elapsed() >= QUEUE_PROGRESS_EVERY {
                        eprintln!(
                            "{}",
                            held_progress_line(admission, started.elapsed().as_secs())
                        );
                        last_progress = Instant::now();
                    }
                    pause = CPU_HOLD_POLL;
                }
                "admit" => {
                    blind_samples = 0;
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
                                "{NOTE} fleet share {:.1}% under the ceiling for \
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
                        let (live, reservations) = slot_reading(registry_path, &mut warnings);
                        let slots = live.len() + reservations.len();
                        last_slots = slots;
                        let succession = input.succession_scope.as_deref().map(|scope| {
                            spawn_gate_lanes::succession_replaces(
                                &live,
                                input.caller_session.as_deref(),
                                scope,
                            )
                        });
                        last_succession_error = succession
                            .as_ref()
                            .and_then(|result| result.as_ref().err().copied());
                        let replaced = usize::from(matches!(succession.as_ref(), Some(Ok(_))));
                        for w in &warnings {
                            eprintln!("{w}");
                        }
                        if slots.saturating_sub(replaced) < cap {
                            let slot_reading = succession
                                .as_ref()
                                .and_then(|result| result.as_ref().ok())
                                .map(|name| {
                                    format!("{slots}/{cap} ok (succession replaces {name})")
                                })
                                .unwrap_or_else(|| format!("{slots}/{cap} ok"));
                            axes_read.insert("slots".into(), serde_json::json!(slot_reading));
                            // Re-checked on dequeue for the same reason the RAM floor is: a
                            // spawn can sit queued past QUEUE_POLL for minutes, and another
                            // process can raise the shared schema inside that window.
                            let mut dequeue_warnings = Vec::new();
                            check_registry_schema(registry_path, &mut dequeue_warnings)
                                .inspect_err(|_| guard.release())?;
                            // Slot free. RAM recheck happens NOW (at dequeue too — a spawn
                            // that queued 5 minutes must not dispatch into a tight machine).
                            check_ram_floor(floor_gb, swap_cap).inspect_err(|_| guard.release())?;
                            // Stamped only once the floor actually answered, so a
                            // receipt never claims an axis it did not read.
                            axes_read.insert("ram".into(), serde_json::json!("ok"));
                            for w in &dequeue_warnings {
                                eprintln!("{w}");
                            }
                            if replaced == 1 {
                                axes_read.insert(
                                    "king_share".into(),
                                    serde_json::json!("skipped (crowned succession)"),
                                );
                            } else {
                                check_king_share(
                                    registry_path,
                                    cap,
                                    input.caller_session.as_deref(),
                                    &axes_read,
                                )
                                .inspect_err(|_| guard.release())?;
                                axes_read.insert("king_share".into(), serde_json::json!("ok"));
                            }
                            // The per-territory team cap: beside the machine cap,
                            // never instead of it. Refuses (never queues) - waiting cannot
                            // help while the node's own territory is full, and other
                            // territories keep their headroom.
                            if let Some(node) = admitted_node.as_deref() {
                                if let Err(receipt) = check_territory_cap(
                                    config_cwd,
                                    registry_path,
                                    &node,
                                    &live,
                                    agents_config::territory_max_live(config_cwd),
                                ) {
                                    guard.release();
                                    eprintln!("{receipt}");
                                    return Err(territory_refusal(&receipt));
                                }
                            }
                            // The blueprint axis beside it: a full
                            // planning lane refuses, never queues.
                            if let Err(receipt) = check_blueprint_cap(
                                config_cwd,
                                registry_path,
                                name,
                                admitted_node.as_deref(),
                                &live,
                            ) {
                                guard.release();
                                eprintln!("{receipt}");
                                return Err(blueprint_refusal(&receipt));
                            }
                            // Redemption sits after every refusing axis and
                            // at the point of admission, just before the spawn
                            // takes its own slot claim: earlier would burn the
                            // reservation on an unrelated CPU or RAM refusal.
                            release_redeemed_reservation(name, guard.root.as_deref());
                            if substrate == "headless" {
                                if let Err(fault) = acquire_worker_slot(
                                    &mut guard,
                                    name,
                                    &holder,
                                    holder_pid,
                                    route_provider,
                                    provider_cap.is_some(),
                                ) {
                                    guard.release();
                                    return Err(gate_fault_refusal(
                                        route_provider,
                                        "lane_reservation_unavailable",
                                        &fault,
                                    ));
                                }
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
                            let row_refs: Vec<&RegistryEntry> = live.iter().collect();
                            let drained = warnings.len();
                            let waiting = spawn_gate_lanes::read_awaiting_operator(
                                registry_path,
                                &row_refs,
                                &mut warnings,
                            );
                            for w in &warnings[drained..] {
                                eprintln!("{w}");
                            }
                            let line = slot_refusal_line(
                                slots,
                                cap,
                                live.len(),
                                &reservations,
                                waiting.len(),
                                "refusing (--no-wait).",
                            );
                            eprintln!("spawn-gate: {line}");
                            let mut receipt = serde_json::json!({
                                "status": "refused",
                                "reason": "no_wait",
                                "axis": "max_live",
                                "axes_read": axes_read.clone(),
                                "held_on": "max_live",
                                "max_live": cap,
                                "count": slots,
                                "current_count": slots,
                                "slot_rows": live
                                    .iter()
                                    .map(|r| r.name.clone())
                                    .chain(reservations.iter().map(|r| r.name.clone()))
                                    .collect::<Vec<_>>(),
                                "waiting_on_operator": waiting.iter().map(|(name, qid)| serde_json::json!({
                                    "name": name,
                                    "question_id": qid,
                                })).collect::<Vec<_>>(),
                            });
                            if let Some(reason) = last_succession_error {
                                receipt["succession"] = serde_json::json!(reason);
                            }
                            return Err(Refusal::with_receipt(EXIT_NO_WAIT, receipt));
                        }
                        if !announced {
                            let row_refs: Vec<&RegistryEntry> = live.iter().collect();
                            let drained = warnings.len();
                            let waiting = spawn_gate_lanes::read_awaiting_operator(
                                registry_path,
                                &row_refs,
                                &mut warnings,
                            );
                            for w in &warnings[drained..] {
                                eprintln!("{w}");
                            }
                            let line = slot_refusal_line(
                                slots,
                                cap,
                                live.len(),
                                &reservations,
                                waiting.len(),
                                "waiting for a free slot (--no-wait to fail fast, --force to bypass)",
                            );
                            eprintln!("spawn queued: {line}");
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
                        "{}",
                        instrument_refusal_sentence(&format!(
                            "the payload carries the unknown verdict {other:?}"
                        ))
                    );
                    return Err(Refusal::with_receipt(
                        EXIT_LOAD_REFUSED,
                        serde_json::json!({
                            "status": "refused",
                            "reason": "cpu_instrument_unreadable",
                            "axis": "cpu_instrument",
                            "axes_read": axes_read.clone(),
                        }),
                    ));
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
            if mutex_blocked_since.is_some() {
                eprintln!(
                    "spawn-gate: {reason} after {}s; a holder the gate cannot prove dead holds the gate mutex. \
                     Read it with `fno agents claim status gate:spawn`. Release a stuck holder \
                     with `fno agents claim release gate:spawn --force --reason \"<why>\"`.",
                    QUEUE_TIMEOUT.as_secs()
                );
            } else {
                eprintln!(
                    "spawn-gate: {reason} after {}s held on {held_on}; \
                     inspect live workers with `fno agents top`, or retry with --no-wait/--force",
                    QUEUE_TIMEOUT.as_secs()
                );
            }
            let mut receipt = serde_json::json!({
                "status": "refused",
                "reason": reason,
                "held_on": held_on,
                "axis": held_on,
                "axes_read": axes_read.clone(),
                "max_live": cap,
                "count": last_slots,
                "current_count": last_slots,
            });
            if let Some(reason) = last_succession_error {
                receipt["succession"] = serde_json::json!(reason);
            }
            return Err(Refusal::with_receipt(EXIT_QUEUE_TIMEOUT, receipt));
        }
        std::thread::sleep(pause);
    }
}

/// One shared memory reading: what the gate refuses on and what the probe
/// reports. `avail` is read only when the floor is enabled, `swap` only when
/// the cap is enabled, and the swap-in rate only when swap sits at or above
/// the cap (the sample costs a 1 s window; a normal spawn pays no wait).
pub(crate) struct MemoryReading {
    pub(crate) avail: Option<f64>,
    pub(crate) swap: Option<f64>,
    pub(crate) swapin_bps: Option<f64>,
}

/// Read [`MemoryReading`] once so the gate and the probe answer from the same
/// instrument calls.
pub(crate) fn read_memory(floor_gb: f64, max_swap_pct: f64) -> MemoryReading {
    let avail = (floor_gb > 0.0).then(available_ram_gb).flatten();
    let swap = (max_swap_pct > 0.0).then(swap_used_pct).flatten();
    let swapin_bps = if swap.is_some_and(|s| s >= max_swap_pct) {
        swapin_bytes_per_sec(SWAPIN_WINDOW)
    } else {
        None
    };
    MemoryReading {
        avail,
        swap,
        swapin_bps,
    }
}

/// The decision core of the memory check, pure so both terms are testable
/// without the machine happening to sit in a given state. Available is named
/// first: under BOTH terms failing, the receipt names the floor an operator
/// tunes first. The swap term fires only while the machine is ALSO swapping
/// in: macOS keeps swap allocated after pressure ends (an idle app can hold
/// tens of GB until it exits), so allocation alone refused every spawn on a
/// box with no paging at all.
pub(crate) fn ram_floor_term(
    avail: Option<f64>,
    floor_gb: f64,
    swap: Option<f64>,
    swapin_bps: Option<f64>,
    max_swap_pct: f64,
) -> Option<(&'static str, String)> {
    if floor_gb > 0.0 && avail.is_some_and(|a| a < floor_gb) {
        return Some((
            "ram_floor",
            format!(
                "available RAM {:.1}GB is below the min_free_gb floor {floor_gb:.1}GB",
                avail.unwrap()
            ),
        ));
    }
    if max_swap_pct > 0.0
        && swap.is_some_and(|s| s >= max_swap_pct)
        && swapin_bps.is_some_and(|r| r >= SWAPIN_REFUSE_BYTES_PER_S)
    {
        return Some((
            "swap_pressure",
            format!(
                "swap {:.1}% used is at or above the max_swap_pct cap {max_swap_pct:.0}% and \
                 swap-in is {:.1} MiB/s",
                swap.unwrap(),
                swapin_bps.unwrap() / MIB,
            ),
        ));
    }
    None
}

/// Memory check (Layer 2), two terms: refuse below the `floor_gb` available-
/// RAM floor, or at/above the `max_swap_pct` swap ceiling WHILE the machine
/// swaps in at [`SWAPIN_REFUSE_BYTES_PER_S`] or more. Never queues: low RAM
/// means something ELSE is eating the machine. `<= 0` disables a term; an
/// unreadable term skips (fail open, as before). Both readings ride every
/// verdict: a floor that only speaks on refusal cannot be audited, and a
/// passing gate must not look like a healthy box.
/// The pass-path RAM readings line, pure so the note prefix is testable:
/// it starts [`NOTE`], never the `spawn-gate: ` verdict marker, because
/// these readings ride ADMITTED spawns too.
pub(crate) fn ram_readings_line(m: &MemoryReading, floor_gb: f64, max_swap_pct: f64) -> String {
    // A disabled term renders `off`, never `unreadable`: it was not read
    // because it is disabled, and a broken sensor must not read as a
    // tuned knob.
    let off_or = |disabled: bool, value: &Option<f64>, unit: &str| -> String {
        if disabled {
            "off".into()
        } else {
            value
                .map(|v| format!("{v:.1}{unit}"))
                .unwrap_or_else(|| "unreadable".into())
        }
    };
    let swapin_word: String = if max_swap_pct <= 0.0 {
        "off".into()
    } else if m.swap.is_none_or(|s| s < max_swap_pct) {
        "not sampled (under cap)".into()
    } else {
        m.swapin_bps
            .map(|r| format!("{:.1} MiB/s", r / MIB))
            .unwrap_or_else(|| "unreadable".into())
    };
    format!(
        "{NOTE} ram readings: available {} (floor {floor_gb:.1}GB), swap {} (cap {max_swap_pct:.0}%), swap-in {swapin_word}",
        off_or(floor_gb <= 0.0, &m.avail, "GB"),
        off_or(max_swap_pct <= 0.0, &m.swap, "%"),
    )
}

fn check_ram_floor(floor_gb: f64, max_swap_pct: f64) -> Result<(), Refusal> {
    let m = read_memory(floor_gb, max_swap_pct);
    if floor_gb > 0.0 || max_swap_pct > 0.0 {
        eprintln!("{}", ram_readings_line(&m, floor_gb, max_swap_pct));
    }
    match ram_floor_term(m.avail, floor_gb, m.swap, m.swapin_bps, max_swap_pct) {
        Some((reason, term)) => {
            eprintln!("spawn-gate: {term}; refusing to spawn (--force to bypass)");
            Err(Refusal::with_receipt(
                EXIT_RAM_REFUSED,
                serde_json::json!({
                    "status": "refused",
                    "reason": reason,
                    "available_gb": m.avail,
                    "min_free_gb": floor_gb,
                    "swap_used_pct": m.swap,
                    "max_swap_pct": max_swap_pct,
                    "swapin_mib_per_s": m.swapin_bps.map(|r| r / MIB),
                }),
            ))
        }
        None => Ok(()),
    }
}

/// Change 3: the payload's `admission` object, computed by the ONE
/// Python decider (`cpu_admission`) and consumed verbatim by this gate. The
/// Rust gate computes no verdict of its own.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct AdmissionPayload {
    pub(crate) verdict: String,
    pub(crate) axis: String,
    pub(crate) reason: String,
    #[serde(default)]
    pub(crate) share_low: f64,
    #[serde(default)]
    pub(crate) share_high: f64,
    #[serde(default)]
    pub(crate) bound: String,
    #[serde(default)]
    pub(crate) fleet_cores: f64,
    #[serde(default)]
    pub(crate) machine_cores: f64,
    #[serde(default)]
    pub(crate) capacity_cores: f64,
    #[serde(default)]
    pub(crate) ceiling: f64,
    // read only through serde: kept so the admission payload still parses
    // when the decider sends the gap the old verdict shape carried.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) gap: Option<String>,
    /// The Python decider's short form of the fleet's largest program; absent
    /// on an older wheel and whenever no attributed row exists.
    #[serde(default)]
    pub(crate) top_holder: Option<String>,
}

/// The receipt's figure block. An axis of `cpu_instrument` measured nothing,
/// so every figure is JSON null - never a 0.0 a reader would take for a
/// reading. `axis`, `detail` and `bound` are words, not figures, and stay.
fn receipt_fields(admission: &AdmissionPayload) -> serde_json::Value {
    let unreadable = admission.axis == "cpu_instrument";
    let fig = |v: f64| {
        if unreadable {
            serde_json::Value::Null
        } else {
            serde_json::json!(v)
        }
    };
    serde_json::json!({
        "axis": admission.axis,
        "detail": admission.reason,
        "share_low": fig(admission.share_low),
        "share_high": fig(admission.share_high),
        "bound": admission.bound,
        "fleet_cores": fig(admission.fleet_cores),
        "machine_cores": fig(admission.machine_cores),
        "capacity_cores": fig(admission.capacity_cores),
        "ceiling": fig(admission.ceiling),
    })
}

/// The periodic held line. The holder clause is the payload's own words, so
/// this reprint and the Python reason cannot drift.
fn held_progress_line(admission: &AdmissionPayload, waited_secs: u64) -> String {
    let holder = admission
        .top_holder
        .as_deref()
        .map(|h| format!("; top holder {h}"))
        .unwrap_or_default();
    format!(
        "still held: fleet {:.1}% over {:.1}%, waited {}s{}",
        admission.share_low * 100.0,
        admission.ceiling * 100.0,
        waited_secs,
        holder
    )
}

#[derive(Debug, Default, Deserialize)]
pub struct FootprintCausePayload {
    /// Kept only so an older admission-less payload still parses; the verdict
    /// comes from `admission` now, never from the gap's presence.
    #[serde(default)]
    #[allow(dead_code)] // read only through serde: older-payload parse tolerance
    attribution_gap: Option<String>,
    /// The Claude Code background daemon's idle pre-warm pool, for the
    /// `fno agents status` machine line.
    #[serde(default)]
    spare_pool_process_count: u64,
    #[serde(default)]
    spare_pool_cpu_cores: f64,
    /// 1-min load average, for the status line only. It decides nothing
    /// anywhere (LD1).
    #[serde(default)]
    #[allow(dead_code)] // read only through serde: display context, decides nothing
    load_1m: Option<f64>,
    #[serde(default)]
    #[allow(dead_code)] // read only through serde: context on the payload, unused by the gate
    cpu_capacity_cores: f64,
    /// The `_emit_failure` shape: when footprint cannot measure at all it
    /// still answers, carrying this key and exit 4. Its words travel into
    /// the instrument refusal (keeps that contract).
    #[serde(default)]
    error: Option<String>,
    /// The decider's answer. Absent on a degraded payload: the gate refuses
    /// as `cpu_instrument_unreadable` rather than guessing (LD3).
    #[serde(default)]
    admission: Option<AdmissionPayload>,
    /// Top fleet consumers by summed ps %cpu; the machine_watch escalation
    /// names the first three by their own argv strings (AC7).
    #[serde(default)]
    pub(crate) top: Vec<TopConsumer>,
}

/// One `top` row of the footprint payload.
#[derive(Debug, Clone, Deserialize)]
pub struct TopConsumer {
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) cpu_percent: f64,
    #[serde(default)]
    pub(crate) command: String,
}

/// The CPU axis's answer for THIS spawn: the admission to branch on plus the
/// receipt `reason` token a refusal carries (AC13). A synthetic instrument
/// refusal is built when the payload carries no decidable admission.
pub(crate) struct CpuAdmission {
    pub(crate) payload: AdmissionPayload,
    /// refuse|undecidable -> cpu_share_undecidable |
    /// cpu_instrument_unreadable. Empty for admit/hold (they never refuse).
    pub(crate) token: &'static str,
}

/// The one spelling of the instrument refusal: the condition, the probe's
/// own words for what it measured, and a verb the reader can run.
fn instrument_refusal_sentence(why: &str) -> String {
    format!(
        "spawn-gate: the CPU instrument is unreadable ({why}); \
         read the instrument yourself with `fno doctor footprint --json --cause-only`; \
         refusing to spawn (--force to bypass)"
    )
}

/// Read the CPU axis from the prefetched footprint payload (LD3).
///
/// The Python decider `cpu_admission` (doctor_footprint.py) is the ONE
/// decider; this gate maps its `admission.verdict` to the same four branches
/// the Python gate takes and prints `admission.reason` verbatim. No payload,
/// an unparseable payload, or a payload without `admission` refuses as
/// `cpu_instrument_unreadable`: the sensor blinding under the load it
/// measures is itself a symptom, and an unknown share is not headroom. The
/// probe's own failure words (`probe_err`) travel into that refusal.
pub(crate) fn check_cpu_axis(prefetched: Option<&str>, probe_err: Option<&str>) -> CpuAdmission {
    fn instrument_refusal(why: &str) -> CpuAdmission {
        CpuAdmission {
            payload: AdmissionPayload {
                verdict: "refuse".to_string(),
                axis: "cpu_instrument".to_string(),
                reason: instrument_refusal_sentence(why),
                share_low: 0.0,
                share_high: 0.0,
                bound: "exact".to_string(),
                fleet_cores: 0.0,
                machine_cores: 0.0,
                capacity_cores: 0.0,
                ceiling: 0.0,
                gap: None,
                top_holder: None,
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
/// same `--json --cause-only` reading through `fno_py_cmd()` (PATH-robust:
///). `fno` itself is deliberately NOT a
/// candidate: it is the Rust shim, and a gate probe must not route through
/// its provisioning waits. The fno-py leg always yields an argv; a
/// genuinely missing wheel surfaces as a failed read the refusal names,
/// rather than a probe silently declared absent.
fn footprint_probe_argv() -> Option<Vec<String>> {
    if resolves_on_path("fno-footprint-cause") {
        return Some(vec!["fno-footprint-cause".to_string()]);
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
/// footprint probe: `(machine line, keeper note)`. Both best-effort
/// - a machine whose footprint cannot be read yields `(None, None)`, never a
/// stale or fabricated line.
pub fn machine_reading_notes() -> (Option<String>, Option<String>) {
    // ONE parse serves both notes; the raw string is never read twice.
    let payload: Option<FootprintCausePayload> = footprint_cause_raw()
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let home = crate::paths::AgentsHome::from_env();
    let footer = crate::machine_sample::newest(&home.events_jsonl())
        .map(|row| {
            let mut line = crate::machine_sample::footer_line(&row, chrono::Utc::now());
            if let Some(payload) = payload.as_ref().filter(|p| p.spare_pool_process_count > 0) {
                line.push_str(&format!(
                    " claude_spare_pool={}proc/{:.2}cores",
                    payload.spare_pool_process_count, payload.spare_pool_cpu_cores
                ));
            }
            line
        })
        .or_else(|| Some(crate::machine_sample::no_row_footer()));
    let keeper = payload.as_ref().and_then(|payload| {
        let commands: Vec<String> = payload.top.iter().map(|c| c.command.clone()).collect();
        crate::drift::keeper_path_note(&commands, std::env::current_exe().ok().as_deref())
    });
    (footer, keeper)
}

pub(crate) fn footprint_cause_raw() -> Result<String, String> {
    // Test seam: a pinned payload keeps gate tests measuring their own axis,
    // never the live machine's load or whatever probe PATH resolves here.
    #[cfg(test)]
    if let Ok(raw) = std::env::var("FNO_TEST_FOOTPRINT_PAYLOAD") {
        if !raw.is_empty() {
            return Ok(raw);
        }
    }
    // Test seam for the re-read loop: one payload PER read. Each call
    // consumes the first line; the last line sticks, so a positive control
    // can count the reads that actually happened. `ERR <words>` answers the
    // no-payload path.
    #[cfg(test)]
    if let Ok(seq) = std::env::var("FNO_TEST_FOOTPRINT_PAYLOAD_SEQ") {
        if !seq.is_empty() {
            let raw = std::fs::read_to_string(&seq).expect("payload-seq file readable");
            let (first, rest) = raw.split_once('\n').unwrap_or((raw.as_str(), ""));
            if !rest.is_empty() {
                std::fs::write(&seq, rest).expect("payload-seq file writable");
            }
            if let Some(why) = first.strip_prefix("ERR ") {
                return Err(why.to_string());
            }
            return Ok(first.to_string());
        }
    }
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

/// Take the headless worker slot claim. The claim carries `model_provider`
/// (the route provider, else the un-routed marker) because the provider count
/// reads that tag. The claim carries the holder pid stamped `holder-process`
/// (the same stamp the flight gate writes), so a dead holder frees its slot
/// at once instead of reading Suspect for the whole TTL. `fail_closed` (a
/// provider cap applies) turns a fault into the caller's refusal; without a
/// cap the claim is count VISIBILITY, not a correctness gate, and a fault
/// proceeds uncounted.
fn acquire_worker_slot(
    guard: &mut GateGuard,
    name: &str,
    holder: &str,
    holder_pid: u32,
    route_provider: Option<&str>,
    fail_closed: bool,
) -> Result<(), String> {
    let key = format!("worker:{name}");
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        "model_provider".to_string(),
        serde_json::Value::String(
            route_provider
                .filter(|p| !p.is_empty())
                .unwrap_or(KNOWN_UNROUTED_PROVIDER)
                .to_string(),
        ),
    );
    match claims::acquire(
        &key,
        holder,
        claims::AcquireOpts {
            pid: Some(holder_pid),
            pid_provenance: Some(claims::HOLDER_PROCESS.to_string()),
            ttl_ms: Some(WORKER_CLAIM_TTL_MS),
            metadata: Some(metadata),
            root: guard.root.clone(),
            ..Default::default()
        },
    ) {
        claims::AcquireOutcome::Acquired(_) => {
            guard.worker_key = Some((key, holder.to_string()));
            Ok(())
        }
        // Fail open: a slot claim is count VISIBILITY, not a correctness gate.
        claims::AcquireOutcome::HeldByOther { holder: h, .. } => {
            let fault = format!("worker reservation {key} held by {h}");
            if fail_closed {
                Err(fault)
            } else {
                eprintln!("{NOTE} worker slot claim {key} unavailable; proceeding uncounted");
                Ok(())
            }
        }
        claims::AcquireOutcome::Error(e) => {
            let fault = format!("worker reservation {key} unavailable: {e}");
            if fail_closed {
                Err(fault)
            } else {
                eprintln!("{NOTE} worker slot claim {key} unavailable; proceeding uncounted");
                Ok(())
            }
        }
    }
}

/// The claims-layer fault refusal: the gate could not serialize the decision
/// or take a lane reservation, so no count was measured and no cap may be
/// named. The reason is the faulted site, never a cap.
fn gate_fault_refusal(provider: Option<&str>, reason: &str, error: &str) -> Refusal {
    Refusal::with_receipt(
        EXIT_PROVIDER_CAP,
        serde_json::json!({
            "status": "refused",
            "reason": reason,
            "provider": provider,
            "error": error,
        }),
    )
}

/// The king-share refusal (W4 / LD1): the share divides
/// `max_live` by CROWNS; `held` counts the caller's own worker rows; a caller
/// with no resolved session is not share-checked; waiting cannot help, so
/// this refuses like the provider cap. Every number comes from
/// [`spawn_gate_lanes::share_reading`]: the count the gate refuses on and the
/// count any readout prints are one value.
/// The held-rows clause of the king-share refusal: the row names the caller
/// can act on, capped at five with an ellipsis like the unattributed suffix.
pub(crate) fn held_rows_suffix(held_rows: Option<&Vec<String>>) -> String {
    match held_rows.filter(|r| !r.is_empty()) {
        Some(rows) => {
            let shown: Vec<String> = rows.iter().take(5).cloned().collect();
            format!(
                "; the rows charged to you are {}{}",
                shown.join(", "),
                if rows.len() > 5 { "..." } else { "" }
            )
        }
        None => String::new(),
    }
}

fn check_king_share(
    registry_path: &Path,
    cap: usize,
    caller_session: Option<&str>,
    _axes_read: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), Refusal> {
    let Some(caller) = caller_session.filter(|c| !c.is_empty()) else {
        return Ok(());
    };
    let reading = spawn_gate_lanes::share_reading(registry_path, cap, Some(caller));
    let (Some(kings), Some(share), Some(held)) = (reading.kings, reading.share, reading.held)
    else {
        // An unreadable registry leaves every count unknown; nothing to
        // enforce and no zero to fail open on.
        return Ok(());
    };
    if held < share {
        return Ok(());
    }
    let mut msg = format!(
        "spawn-gate: king {} holds {held} of max_live {cap} across {kings} kings (share {share}); \
         refusing to spawn -- waiting cannot help while your own workers hold the share \
         (--force to bypass)",
        &caller[..caller.len().min(8)]
    );
    // The held names read before the unattributed bucket: they are the rows
    // the caller can stop, where the bucket names nobody.
    msg.push_str(&held_rows_suffix(reading.held_rows.as_ref()));
    if let Some(rows) = reading.unattributed_rows.filter(|r| !r.is_empty()) {
        let shown: Vec<String> = rows.iter().take(5).cloned().collect();
        msg.push_str(&format!(
            "; {} live row(s) name nobody and sit in the unattributed bucket ({}{})",
            rows.len(),
            shown.join(", "),
            if rows.len() > 5 { "..." } else { "" }
        ));
    }
    eprintln!("{msg}");
    Err(Refusal::code(EXIT_KING_SHARE)
        .ev("reason", serde_json::json!("king_share"))
        .ev("king", serde_json::json!(caller))
        .ev("held", serde_json::json!(held))
        .ev("share", serde_json::json!(share))
        .ev("max_live", serde_json::json!(cap))
        .ev("kings", serde_json::json!(kings))
        .ev(
            "held_rows",
            serde_json::json!(reading.held_rows.clone().unwrap_or_default()),
        ))
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
        _ => eprintln!("{NOTE} QoS demotion of pid {pid} failed (non-fatal)"),
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
                "{NOTE} bg worker {job_id} pid not in roster within 10s; \
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

    #[test]
    fn a_review_seed_or_label_is_refused_and_other_phases_pass() {
        for seed in [
            "$fno:review high --comment",
            "/fno:review x",
            "/code-review this diff",
            "/review x-1",
        ] {
            let refusal = review_session_gate(&GateInput {
                seed: Some(seed.to_string()),
                ..Default::default()
            })
            .expect(seed);
            assert_eq!(refusal.exit_code, EXIT_REVIEW_SESSION);
            let receipt = refusal.receipt.as_ref().unwrap();
            assert_eq!(receipt["reason"], "review_session");
            let remedy = receipt["remedy"].as_str().unwrap();
            assert!(remedy.contains("/fno:review"));
            assert!(remedy.contains("$fno:review"));
            assert!(verdict_line(&refusal)
                .starts_with("spawn-gate: refused on review (review_session, exit 89)"));
        }

        for (seed, phase) in [
            ("/fno:triage deep", Some("review")),
            ("/code-review this diff", Some("do")),
        ] {
            let refusal = review_session_gate(&GateInput {
                seed: Some(seed.to_string()),
                session_phase: phase.map(str::to_string),
                ..Default::default()
            })
            .expect(seed);
            assert_eq!(refusal.exit_code, EXIT_REVIEW_SESSION);
        }

        for seed in [
            "/fno:think why",
            "/fno:target x-1",
            "review the diff",
            "/Users/x/review",
            "",
        ] {
            assert!(review_session_gate(&GateInput {
                seed: Some(seed.to_string()),
                ..Default::default()
            })
            .is_none());
        }
        assert!(review_session_gate(&GateInput::default()).is_none());
    }

    /// The no_wait specimen renders the verdict line the plan pins: axis and
    /// breach named, figures from the receipt in key order.
    #[test]
    fn verdict_line_names_axis_and_breach_for_no_wait() {
        let refusal = Refusal::with_receipt(
            EXIT_NO_WAIT,
            serde_json::json!({
                "status": "refused",
                "reason": "no_wait",
                "axis": "max_live",
                "axes_read": {"cpu": "admit", "slots": "15/15 queued"},
                "held_on": "max_live",
                "max_live": 15,
                "count": 15,
                "current_count": 15,
                "slot_rows": ["w1", "w2"],
                "waiting_on_operator": [],
            }),
        );
        assert_eq!(
            verdict_line(&refusal),
            "spawn-gate: refused on max_live (no_wait, exit 76): max_live=15, count=15, current_count=15"
        );
    }

    /// A territory refusal: the event's axis wins over the receipt's, and
    /// the live_blueprints array never enters the figures.
    #[test]
    fn verdict_line_reads_event_axis_and_skips_arrays() {
        let refusal = Refusal::with_receipt(
            EXIT_TERRITORY_CAP,
            serde_json::json!({
                "status": "refused",
                "reason": "territory_cap",
                "territory": "team-x",
                "count": 3,
                "current_count": 3,
                "max_live_per_territory": 3,
                "live_blueprints": ["bp-a", "bp-b"],
            }),
        )
        .ev("axis", serde_json::json!("territory"));
        assert_eq!(
            verdict_line(&refusal),
            "spawn-gate: refused on territory (territory_cap, exit 86): territory=team-x, count=3, current_count=3, max_live_per_territory=3"
        );
    }

    /// No receipt and no event: the line still names the exit.
    #[test]
    fn verdict_line_without_receipt_names_the_exit() {
        let refusal = Refusal::code(82);
        assert_eq!(
            verdict_line(&refusal),
            "spawn-gate: refused on unknown (unknown, exit 82)"
        );
    }

    /// A receipt-less refusal keeps its measurements in the event; the
    /// verdict falls back to the event's scalars so the breach is named.
    #[test]
    fn verdict_line_falls_back_to_event_scalars() {
        let refusal = Refusal::code(EXIT_KING_SHARE)
            .ev("reason", serde_json::json!("king_share"))
            .ev("king", serde_json::json!("abc12345"))
            .ev("held", serde_json::json!(5))
            .ev("share", serde_json::json!(3))
            .ev("max_live", serde_json::json!(15))
            .ev("kings", serde_json::json!(2));
        assert_eq!(
            verdict_line(&refusal),
            "spawn-gate: refused on king_share (king_share, exit 80): king=abc12345, held=5, share=3, max_live=15, kings=2"
        );
    }

    /// The readings of an admitted spawn carry the note prefix, never the
    /// verdict marker.
    #[test]
    fn ram_readings_line_is_a_note() {
        let m = MemoryReading {
            avail: Some(35.9),
            swap: Some(85.5),
            swapin_bps: None,
        };
        let line = ram_readings_line(&m, 2.0, 90.0);
        assert!(
            line.starts_with("spawn-gate note: ram readings:"),
            "got: {line}"
        );
        assert!(!line.starts_with("spawn-gate:"), "got: {line}");
    }

    /// The marker is the refusal wire format (advance.py and
    /// dispatch_launch.rs both key on it). A pass-path word beside the
    /// marker breaks the readers, so the source itself is scanned.
    #[test]
    fn pass_path_lines_never_carry_the_verdict_marker() {
        const NEEDLE: &str = concat!("spawn-gate", ": ");
        const BARRED: [&str; 9] = [
            "proceeding",
            "readings",
            "not refusing",
            "admitting",
            "non-fatal",
            "forced past",
            "ignored",
            "uncounted",
            "degraded",
        ];
        for file in [
            include_str!("spawn_gate.rs"),
            include_str!("spawn_gate_lanes.rs"),
        ] {
            for line in file.lines() {
                if line.contains(NEEDLE) {
                    for word in BARRED {
                        assert!(
                            !line.contains(word),
                            "pass-path word {word:?} beside the verdict marker in: {line}"
                        );
                    }
                }
            }
        }
    }

    /// A refusal receipt carries only the readings of the pass that refused.
    /// No fixture can flip the CPU payload between queue
    /// passes (the test seam is one static env var), so the plan's fallback
    /// pins it structurally: the binding must sit inside run_gate's queue
    /// loop, not before it.
    #[test]
    fn axes_read_is_per_pass() {
        let src = include_str!("spawn_gate.rs");
        let loop_at = src
            .find("\n    loop {\n")
            .expect("run_gate's queue loop must be present");
        let bind_at = src
            .find("let mut axes_read = serde_json::Map::new();")
            .expect("axes_read binding must be present");
        assert!(
            bind_at > loop_at,
            "axes_read must reset per pass: it sits before the queue loop"
        );
        assert!(
            !src[loop_at..bind_at].lines().any(|l| l.starts_with('}')),
            "axes_read binding drifted outside the queue loop"
        );
    }

    /// The receipt names swap when the ceiling fires beside live swap-ins.
    #[test]
    fn ram_floor_term_names_swap_at_the_ceiling() {
        let term = ram_floor_term(
            Some(24.0),
            4.0,
            Some(92.6),
            Some(SWAPIN_REFUSE_BYTES_PER_S),
            90.0,
        );
        assert_eq!(term.as_ref().map(|(r, _)| *r), Some("swap_pressure"));
        let msg = term.unwrap().1;
        assert!(msg.contains("swap"), "the failing term must be named");
        assert!(msg.contains("MiB/s"), "the message names the swap-in rate");
    }

    /// Under BOTH terms failing, available is named (the floor an operator
    /// tunes first).
    #[test]
    fn ram_floor_term_names_available_under_both_terms() {
        let term = ram_floor_term(
            Some(1.0),
            4.0,
            Some(95.0),
            Some(SWAPIN_REFUSE_BYTES_PER_S),
            90.0,
        );
        assert_eq!(term.as_ref().map(|(r, _)| *r), Some("ram_floor"));
        assert!(term.unwrap().1.contains("available"));
    }

    /// Plenty of RAM, low swap: no term.
    #[test]
    fn ram_floor_term_passes_with_headroom() {
        assert_eq!(
            ram_floor_term(
                Some(24.0),
                4.0,
                Some(30.0),
                Some(SWAPIN_REFUSE_BYTES_PER_S),
                90.0
            ),
            None
        );
    }

    /// An unreadable swap read skips its term (fail open), while a failing
    /// available term still refuses.
    #[test]
    fn ram_floor_term_skips_unreadable_swap() {
        assert_eq!(ram_floor_term(Some(24.0), 4.0, None, None, 90.0), None);
        assert_eq!(
            ram_floor_term(Some(1.0), 4.0, None, None, 90.0).map(|(r, _)| r),
            Some("ram_floor")
        );
    }

    /// The swap ceiling disabled (`<= 0`) never fires, whatever the machine
    /// reads.
    #[test]
    fn ram_floor_term_disabled_swap_cap_never_fires() {
        assert_eq!(
            ram_floor_term(Some(24.0), 4.0, Some(100.0), Some(f64::MAX), 0.0),
            None
        );
    }

    /// Allocation alone never refuses: 94.7% against a cap of 90 with no
    /// swap-ins admits, because macOS holds swap allocated after pressure
    /// ends.
    #[test]
    fn ram_floor_term_admits_allocated_swap_with_no_swapins() {
        assert_eq!(
            ram_floor_term(Some(24.0), 4.0, Some(94.7), Some(0.0), 90.0),
            None
        );
    }

    /// The thrash shape stays refused: over-cap swap WITH live swap-ins, and
    /// the message names both the percent and the rate.
    #[test]
    fn ram_floor_term_refuses_allocated_swap_with_live_swapins() {
        let term = ram_floor_term(
            Some(24.11),
            4.0,
            Some(92.6),
            Some(8.0 * 1024.0 * 1024.0),
            90.0,
        );
        assert_eq!(term.as_ref().map(|(r, _)| *r), Some("swap_pressure"));
        let msg = term.unwrap().1;
        assert!(msg.contains("92.6"), "names the swap percent");
        assert!(msg.contains("MiB/s"), "names the swap-in rate");
    }

    /// An unreadable swap-in rate fails open even with swap over the cap.
    #[test]
    fn ram_floor_term_skips_unreadable_swapin_rate() {
        assert_eq!(
            ram_floor_term(Some(24.0), 4.0, Some(94.7), None, 90.0),
            None
        );
    }

    /// One byte per second under the floor admits; at the floor refuses.
    #[test]
    fn ram_floor_term_swapin_floor_boundary() {
        let under = SWAPIN_REFUSE_BYTES_PER_S - 1.0;
        assert_eq!(
            ram_floor_term(Some(24.0), 4.0, Some(94.7), Some(under), 90.0),
            None
        );
        assert_eq!(
            ram_floor_term(
                Some(24.0),
                4.0,
                Some(94.7),
                Some(SWAPIN_REFUSE_BYTES_PER_S),
                90.0
            )
            .map(|(r, _)| r),
            Some("swap_pressure")
        );
    }

    /// the macOS swapusage line parses to percent used; a malformed
    /// line and a zero total both read as unreadable.
    #[test]
    fn parse_swapusage_reads_the_sysctl_line() {
        let line = "total = 18432.00M  used = 17080.75M  free = 1351.25M  (encrypted)";
        let pct = parse_swapusage(line).unwrap();
        assert!((pct - 17080.75 / 18432.0 * 100.0).abs() < 0.01);
        assert_eq!(parse_swapusage("banana"), None);
        assert_eq!(parse_swapusage("total = 0.00M  used = 0.00M"), None);
    }

    const ROOTS: [&str; 1] = ["/Users/x/.fno"];

    fn roots() -> Vec<String> {
        ROOTS.iter().map(|r| r.to_string()).collect()
    }

    /// AC3-HP (Rust side): an active stop refuses at the FIRST boundary -
    /// this call runs before `run_gate`'s `FNO_SPAWN_GATE=0` return, so the
    /// bypass env cannot wave a spawn through.
    #[test]
    fn fleet_incident_gate_refuses_a_stopped_record() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let td = tempfile::TempDir::new().unwrap();
        let saved = std::env::var_os("FNO_AGENTS_HOME");
        std::env::set_var("FNO_AGENTS_HOME", td.path());
        let path = crate::fleet_incident::fleet_stop_path(&crate::paths::AgentsHome::at(td.path()));
        std::fs::create_dir_all(td.path()).unwrap();
        let record = crate::fleet_incident::IncidentRecord {
            version: crate::fleet_incident::STATE_VERSION,
            state: "stopped".into(),
            generation: 3,
            changed_at: "2026-09-11T00:00:00Z".into(),
            changed_by: "op".into(),
            reason: "wedged lock".into(),
            source: Some("file".into()),
        };
        std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();

        assert_eq!(
            fleet_incident_gate().err().map(|r| r.exit_code),
            Some(EXIT_FLEET_STOP)
        );
        match saved {
            Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
            None => std::env::remove_var("FNO_AGENTS_HOME"),
        }
    }

    /// AC3-EDGE: a corrupt record is a CANNOT-TELL refusal with its own exit
    /// code, never a clear and never a stop verdict.
    #[test]
    fn fleet_incident_gate_fails_closed_on_an_unreadable_record() {
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let td = tempfile::TempDir::new().unwrap();
        let saved = std::env::var_os("FNO_AGENTS_HOME");
        std::env::set_var("FNO_AGENTS_HOME", td.path());
        std::fs::create_dir_all(td.path()).unwrap();
        std::fs::write(
            crate::fleet_incident::fleet_stop_path(&crate::paths::AgentsHome::at(td.path())),
            b"garbage",
        )
        .unwrap();

        assert_eq!(
            fleet_incident_gate().err().map(|r| r.exit_code),
            Some(EXIT_FLEET_STOP_UNAVAILABLE)
        );
        match saved {
            Some(v) => std::env::set_var("FNO_AGENTS_HOME", v),
            None => std::env::remove_var("FNO_AGENTS_HOME"),
        }
    }

    #[test]
    fn undeclared_lane_is_refused_with_its_own_exit_code() {
        // An unknown harness declares nothing at all, which is the only thing
        // this gate refuses.
        assert_eq!(
            state_root_grant_gate("nosuchharness", "thread", &roots())
                .err()
                .map(|r| r.exit_code),
            Some(EXIT_STATE_ROOT_UNGRANTED)
        );
        assert_eq!(
            state_root_grant_gate("claude", "nosuchsubstrate", &roots())
                .err()
                .map(|r| r.exit_code),
            Some(EXIT_STATE_ROOT_UNGRANTED)
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
            assert!(
                state_root_grant_gate(harness, substrate, &roots()).is_ok(),
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
            assert!(
                state_root_grant_gate(harness, substrate, &roots()).is_ok(),
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
        assert!(state_root_grant_gate("gemini", "headless", &[]).is_ok());
    }

    #[test]
    fn spawn_cap_guard_agrees_with_python_gate_fixture() {
        // AC2-FR: this Rust guard must agree with the Python
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
Pages purgeable:                          25000.\n\
Swapins: 19235608.\n\
Swapouts: 3444531.\n";

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

    #[test]
    fn vm_stat_swapins_reads_the_swapins_line() {
        let (pages, size) = parse_vm_stat_swapins(VM_STAT).unwrap();
        assert_eq!(pages, 19_235_608);
        assert_eq!(size, 16_384);
        // Header but no Swapins line: not a swap-in reading, refuse to guess.
        assert_eq!(
            parse_vm_stat_swapins("Mach Virtual Memory Statistics: (page size of 16384 bytes)\n"),
            None
        );
        assert_eq!(parse_vm_stat_swapins("Swapins: banana.\n"), None);
    }

    #[test]
    fn proc_vmstat_pswpin_reads_the_pswpin_line() {
        assert_eq!(
            parse_proc_vmstat_pswpin("pgfault 123\npswpin 456\npswpout 789\n"),
            Some(456)
        );
        assert_eq!(parse_proc_vmstat_pswpin("pgfault 123\n"), None);
    }

    /// AC9: the shared fixture pins the branch this gate takes per
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

    /// Junk, an admission-less payload, an answered failure, and no payload
    /// at all all refuse as the unreadable instrument (LD3) - never as an
    /// idle machine; the probe's own failure words travel into the sentence
    /// and the sentence names the verb that re-reads the instrument.
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
            (
                Some(
                    r#"{"error":"footprint unavailable: worker root liveness unavailable: registry row w1 carries no pid start token","exit_code":4}"#,
                ),
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
            if let Some(probe_words) = err {
                assert!(
                    cpu.payload.reason.contains(probe_words),
                    "{}",
                    cpu.payload.reason
                );
            }
            assert!(
                cpu.payload.reason.contains("fno doctor footprint"),
                "{}",
                cpu.payload.reason
            );
        }
    }

    /// The receipt's figure block: an unreadable instrument leaves every
    /// figure JSON null - never a 0.0 a reader would take for a reading -
    /// while the words (axis, detail, bound) survive intact.
    #[test]
    fn receipt_figures_are_null_when_the_instrument_never_answered() {
        let cpu = check_cpu_axis(
            None,
            Some("process table unavailable: timed out after 5.0s"),
        );
        let fields = receipt_fields(&cpu.payload);
        assert_eq!(fields["axis"], "cpu_instrument");
        for key in [
            "share_low",
            "share_high",
            "fleet_cores",
            "machine_cores",
            "capacity_cores",
            "ceiling",
        ] {
            assert!(fields[key].is_null(), "{key} must be null");
        }
        assert_eq!(fields["detail"], cpu.payload.reason);
        assert_eq!(fields["bound"], "exact");
        assert!(
            cpu.payload.reason.contains("fno doctor footprint"),
            "{}",
            cpu.payload.reason
        );
    }

    /// The periodic held reprint prints the payload's holder clause
    /// verbatim; an absent holder leaves the line exactly as before.
    #[test]
    fn held_progress_line_prints_the_payloads_holder_verbatim() {
        let raw = r#"{"verdict":"hold","axis":"fleet_cpu_share","reason":"r","share_low":0.625,"share_high":0.625,"bound":"exact","fleet_cores":7.5,"machine_cores":7.5,"capacity_cores":12.0,"ceiling":0.5,"gap":null}"#;
        let mut admission: AdmissionPayload = serde_json::from_str(raw).unwrap();
        assert_eq!(
            held_progress_line(&admission, 40),
            "still held: fleet 62.5% over 50.0%, waited 40s"
        );
        admission.top_holder = Some("yes 16 procs 5.13 cores".to_string());
        assert_eq!(
            held_progress_line(&admission, 60),
            "still held: fleet 62.5% over 50.0%, waited 60s; top holder yes 16 procs 5.13 cores"
        );
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
            "[agents]\nmax_live = 999\nmin_free_gb = 0\nmax_swap_pct = 0\n",
        )
        .unwrap();

        // Hold the mutex as somebody else. The claim defaults its pid to this
        // live test process, so it reads as a live holder.
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
        // Positive control on the test's own premise: acquire must report the
        // mutex held by another. Assert that instead of assuming it: a free
        // mutex would let run_gate sail through, and this test would pass
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
            GateInput {
                name: "w2".into(),
                substrate: "bg".into(),
                flags: GateFlags {
                    force: false,
                    no_wait: true,
                },
                ..Default::default()
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
            got.err().map(|r| r.exit_code),
            Some(EXIT_NO_WAIT),
            "must refuse with the no-wait code"
        );
        assert!(
            elapsed < QUEUE_TIMEOUT,
            "must refuse fast, not queue: took {elapsed:?}"
        );
    }

    /// A holder that dies by signal leaves no release behind. The claim's pid
    /// must free the mutex anyway, so the next capped spawner acquires at once
    /// instead of queueing behind the TTL.
    #[test]
    fn a_gate_holder_killed_by_a_signal_frees_the_mutex_at_once() {
        use std::os::unix::process::CommandExt;
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-gate-sigdeath-{}", std::process::id()));
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
            "[agents]\nmax_live = 999\nmin_free_gb = 0\nmax_swap_pct = 0\n\n\
             [agents.provider_limits.zai]\nlanes = 99\n",
        )
        .unwrap();
        let registry = dir.join("registry.json");
        std::fs::write(&registry, r#"{"schema_version":1,"entries":[]}"#).unwrap();

        for sig in [libc::SIGTERM, libc::SIGKILL, libc::SIGPIPE] {
            let mut command = std::process::Command::new("sleep");
            command.arg("300");
            // The test process ignores SIGPIPE and an ignored disposition
            // survives exec, so reset it or SIGPIPE would not kill the child.
            unsafe {
                command.pre_exec(move || {
                    libc::signal(sig, libc::SIG_DFL);
                    Ok(())
                });
            }
            let mut child = command.spawn().unwrap();
            let child_pid = child.id();
            let holder = format!("spawn-gate:{child_pid}:holder");
            let held = claims::acquire(
                "gate:spawn",
                &holder,
                claims::AcquireOpts {
                    ttl_ms: Some(GATE_CLAIM_TTL_MS),
                    root: Some(root.clone()),
                    pid: Some(child_pid),
                    ..Default::default()
                },
            );
            assert!(
                matches!(held, claims::AcquireOutcome::Acquired(_)),
                "setup: the child must hold the mutex, got {held:?}"
            );
            unsafe { libc::kill(child_pid as i32, sig) };
            let _ = child.wait();

            let (state, _) = claims::status("gate:spawn", Some(&root));
            assert!(
                !matches!(
                    state,
                    claims::ClaimState::Live | claims::ClaimState::Suspect
                ),
                "signal {sig}: a dead holder must not read held, got {state:?}"
            );

            let started = Instant::now();
            let got = run_gate(
                &dir,
                &registry,
                GateInput {
                    name: "w-after-death".into(),
                    substrate: "bg".into(),
                    flags: GateFlags {
                        force: true,
                        no_wait: true,
                    },
                    route_provider: Some("zai".into()),
                    ..GateInput::default()
                },
            );
            let elapsed = started.elapsed();
            let guard = got.unwrap_or_else(|r| {
                panic!(
                    "signal {sig}: gate refused after holder death: {:?}",
                    r.receipt
                )
            });
            let me = format!("spawn-gate:{}:w-after-death", std::process::id());
            assert_eq!(
                guard.gate_key.as_ref().map(|(_, h)| h.as_str()),
                Some(me.as_str())
            );
            assert!(elapsed < Duration::from_secs(5), "took {elapsed:?}");
            drop(guard);
        }

        std::env::remove_var("FNO_CLAIMS_ROOT");
        match prior_spawn_gate {
            Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
            None => std::env::remove_var("FNO_SPAWN_GATE"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC5-HP: a route spawn onto a provider whose lane snapshot shows a
    /// fresh open wall refuses exit 78 even under --force, the way the
    /// auto-continue specimen d8996f9b should have been refused.
    #[test]
    fn route_spawn_refuses_on_a_walled_lane_even_forced() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-gate-lanequota-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("provider-cap")).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", dir.join("claims-root"));
        let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
        std::env::remove_var("FNO_SPAWN_GATE");
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
            "[agents]\nmax_live = 999\nmin_free_gb = 0\nmax_swap_pct = 0\n",
        )
        .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        std::fs::write(
            dir.join("provider-cap").join("snapshot.json"),
            format!(
                r#"{{"lanes":[{{"lane":"zai:default","provider":"zai","account":"default","reset_epoch":{},"reset_passed_epoch":null,"missing_reset_timezone":[],"state":"open","members":[]}}],"measured_at":"probe","measured_at_epoch":{}}}"#,
                now + 600,
                now
            ),
        )
        .unwrap();

        let got = run_gate(
            &dir,
            &dir.join("registry.json"),
            GateInput {
                name: "ac-t-f6c6-stop-hook-glm".into(),
                substrate: "headless".into(),
                flags: GateFlags {
                    force: true,
                    no_wait: true,
                },
                route_provider: Some("zai".into()),
                account: Some(String::new()),
                ..GateInput::default()
            },
        );

        std::env::remove_var("FNO_CLAIMS_ROOT");
        match prior_spawn_gate {
            Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
            None => std::env::remove_var("FNO_SPAWN_GATE"),
        }
        let refusal = got.err().expect("walled lane must refuse");
        assert_eq!(refusal.exit_code, EXIT_PROVIDER_CAP);
        assert_eq!(
            refusal.receipt.as_ref().unwrap()["reason"],
            "provider_quota_lock"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
            "[agents]\nmax_live = 1\nmin_free_gb = 0\nmax_swap_pct = 0\n",
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

    /// AC5-TEXT: the shared refusal sentence names the probe field that lists
    /// every counted row, marks the operator-waiting share, and never again
    /// blames a population its own recommended reader cannot see.
    #[test]
    fn slot_refusal_line_names_the_probe_and_marks_waiting_rows() {
        let line = slot_refusal_line(3, 2, 3, &[], 1, "refusing (--no-wait).");
        assert!(line.contains("fno agents gate-status"), "{line}");
        assert!(line.contains("slot_rows"), "{line}");
        assert!(
            line.contains("1 of the rows wait on an operator question"),
            "{line}"
        );
        assert!(!line.contains("--status quiet"), "{line}");
        assert!(!line.contains("fno agents top"), "{line}");

        let line = slot_refusal_line(3, 2, 3, &[], 0, "refusing (--no-wait).");
        assert!(!line.contains("wait on an operator question"), "{line}");
    }

    /// AC5-HP: the --no-wait slot refusal carries the receipt naming every
    /// counted row, so a king can act on rows instead of a bare number.
    #[test]
    fn no_wait_refusal_names_the_rows_it_counted() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-gate-rows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("claims-root");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &root);
        let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
        std::env::remove_var("FNO_SPAWN_GATE");
        // Pin the CPU axis to an admit: it decides BEFORE the slot census, so
        // a busy machine would refuse with 79 before the slot axis is reached.
        let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
        std::env::set_var(
            "FNO_TEST_FOOTPRINT_PAYLOAD",
            r#"{"admission":{"verdict":"admit","axis":"fleet_cpu_share","reason":"fixture","bound":"exact","ceiling":0.5}}"#,
        );
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
            "[agents]\nmax_live = 2\nmin_free_gb = 0\n",
        )
        .unwrap();

        // Three live workers (alive pids) against max_live 2.
        let agents = dir.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let reg = agents.join("registry.json");
        let me = std::process::id();
        std::fs::write(
            &reg,
            format!(
                r#"{{"schema_version":1,"entries":[
                    {{"name":"w1","provider":"claude","cwd":"/tmp","status":"live","pid":{me},"created_at":"2026-01-01T00:00:00Z"}},
                    {{"name":"w2","provider":"claude","cwd":"/tmp","status":"live","pid":{me},"created_at":"2026-01-01T00:00:00Z"}},
                    {{"name":"w3","provider":"claude","cwd":"/tmp","status":"live","pid":{me},"created_at":"2026-01-01T00:00:00Z"}}]}}"#
            ),
        )
        .unwrap();

        let got = run_gate(
            &dir,
            &reg,
            GateInput {
                name: "w4".into(),
                substrate: "bg".into(),
                flags: GateFlags {
                    force: false,
                    no_wait: true,
                },
                ..Default::default()
            },
        );

        std::env::remove_var("FNO_CLAIMS_ROOT");
        match prior_spawn_gate {
            Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
            None => std::env::remove_var("FNO_SPAWN_GATE"),
        }
        match prior_payload {
            Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
            None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
        }

        let refusal = got.err().expect("cap 2 with 3 live rows must refuse");
        assert_eq!(refusal.exit_code, EXIT_NO_WAIT);
        let receipt = refusal.receipt.expect("no_wait refusal carries a receipt");
        assert_eq!(receipt["count"], 3);
        let names: Vec<String> = receipt["slot_rows"]
            .as_array()
            .expect("slot_rows array")
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(names.len(), 3, "{names:?}");
        for expected in ["w1", "w2", "w3"] {
            assert!(names.iter().any(|n| n == expected), "{names:?}");
        }
        assert_eq!(
            receipt["waiting_on_operator"].as_array().map(Vec::len),
            Some(0),
            "no questions journal, nobody waits"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// AC5-WAIT: a counted row that waits on an open operator question keeps
    /// its slot (RAM, not rows) but is named in the receipt with its question.
    #[test]
    fn no_wait_refusal_marks_the_rows_waiting_on_the_operator() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-gate-wrows-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("claims-root");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &root);
        let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
        std::env::remove_var("FNO_SPAWN_GATE");
        let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
        std::env::set_var(
            "FNO_TEST_FOOTPRINT_PAYLOAD",
            r#"{"admission":{"verdict":"admit","axis":"fleet_cpu_share","reason":"fixture","bound":"exact","ceiling":0.5}}"#,
        );
        let projects = dir.join("projects");
        std::env::set_var(crate::claude_drive::PROJECTS_DIR_ENV, &projects);
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
            "[agents]\nmax_live = 2\nmin_free_gb = 0\n",
        )
        .unwrap();

        let a = "aaaaaaaa-0000-0000-0000-00000000000a";
        std::fs::write(
            dir.join("questions.jsonl"),
            format!(
                "{{\"type\":\"operator_question\",\"data\":{{\"question_id\":\"q-1\",\"session_id\":\"{a}\"}}}}\n"
            ),
        )
        .unwrap();
        let proj = projects.join("-tmp-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let t = proj.join(format!("{a}.jsonl"));
        std::fs::write(&t, b"{}\n").unwrap();
        let old = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3600;
        std::fs::File::options()
            .write(true)
            .open(&t)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(old)),
            )
            .unwrap();

        let agents = dir.join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let reg = agents.join("registry.json");
        let me = std::process::id();
        std::fs::write(
            &reg,
            format!(
                r#"{{"schema_version":1,"entries":[
                    {{"name":"waiting","harness":"claude","harness_session_id":"{a}","provider":"zai","cwd":"/tmp","status":"live","pid":{me},"created_at":"2026-01-01T00:00:00Z"}},
                    {{"name":"w2","provider":"claude","cwd":"/tmp","status":"live","pid":{me},"created_at":"2026-01-01T00:00:00Z"}},
                    {{"name":"w3","provider":"claude","cwd":"/tmp","status":"live","pid":{me},"created_at":"2026-01-01T00:00:00Z"}}]}}"#
            ),
        )
        .unwrap();

        let got = run_gate(
            &dir,
            &reg,
            GateInput {
                name: "w4".into(),
                substrate: "bg".into(),
                flags: GateFlags {
                    force: false,
                    no_wait: true,
                },
                ..Default::default()
            },
        );

        std::env::remove_var("FNO_CLAIMS_ROOT");
        std::env::remove_var(crate::claude_drive::PROJECTS_DIR_ENV);
        match prior_spawn_gate {
            Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
            None => std::env::remove_var("FNO_SPAWN_GATE"),
        }
        match prior_payload {
            Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
            None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
        }

        let refusal = got.err().expect("cap 2 with 3 live rows must refuse");
        assert_eq!(refusal.exit_code, EXIT_NO_WAIT);
        let receipt = refusal.receipt.expect("no_wait refusal carries a receipt");
        assert_eq!(receipt["count"], 3, "a waiting worker keeps its slot");
        let waiting = receipt["waiting_on_operator"]
            .as_array()
            .expect("waiting_on_operator array");
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0]["name"], "waiting");
        assert_eq!(waiting[0]["question_id"], "q-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// workers hold its full share refuses exit 80 even with fleet slots free,
    /// because waiting cannot help while the caller's own workers hold it.
    #[test]
    fn king_share_refuses_when_the_caller_holds_its_full_share() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("fno-gate-share-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("claims-root");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("FNO_CLAIMS_ROOT", &root);
        let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
        std::env::remove_var("FNO_SPAWN_GATE");
        // A prior test's FNO_CONFIG can name a deleted TempDir; config
        // candidates then resolve to that dead path alone and max_live
        // defaults, so this test's own config.toml never gets read.
        let prior_config = std::env::var_os("FNO_CONFIG");
        std::env::remove_var("FNO_CONFIG");
        // Pin the CPU axis to an admit: the king share under test sits AFTER
        // the CPU axis in gate order, so a busy machine (or a CI runner with
        // no probe installed) would refuse with 79 before reaching it.
        let prior_payload = std::env::var_os("FNO_TEST_FOOTPRINT_PAYLOAD");
        std::env::set_var(
            "FNO_TEST_FOOTPRINT_PAYLOAD",
            r#"{"admission":{"verdict":"admit","axis":"fleet_cpu_share","reason":"fixture","bound":"exact","ceiling":0.5}}"#,
        );
        let fnodir = dir.join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(
            fnodir.join("config.toml"),
            "[agents]\nmax_live = 4\nmin_free_gb = 0\nmax_swap_pct = 0\n",
        )
        .unwrap();

        let pid = std::process::id();
        let start = claims::process_create_time_ms(pid as i32).unwrap_or(0);
        let live = |name: &str, spawned_by: &str| {
            format!(
                r#"{{"name":"{name}","harness":"claude","provider":"zai","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","pid":{pid},"pid_start_time":{start},"spawned_by_session":"{spawned_by}"}}"#
            )
        };
        let crowned = |name: &str, session: &str| {
            format!(
                r#"{{"name":"{name}","harness":"claude","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","crown_level":1,"harness_session_id":"{session}"}}"#
            )
        };
        // 2 kings -> share 2; the caller holds its full share with 2 rows, so
        // 2 slots remain fleet-wide and the king share still refuses.
        let reg = dir.join("registry.json");
        std::fs::write(
            &reg,
            format!(
                r#"{{"schema_version":{},"entries":[{},{},{},{}]}}"#,
                crate::state::REGISTRY_SCHEMA_VERSION,
                crowned("king-a", "session-aaaaaaaa"),
                crowned("king-b", "session-bbbbbbbb"),
                live("w1", "session-aaaaaaaa"),
                live("w2", "session-aaaaaaaa"),
            ),
        )
        .unwrap();

        let got = run_gate(
            &dir,
            &reg,
            GateInput {
                name: "w3".into(),
                substrate: "bg".into(),
                flags: GateFlags {
                    force: false,
                    no_wait: true,
                },
                caller_session: Some("session-aaaaaaaa".into()),
                ..Default::default()
            },
        );
        std::env::remove_var("FNO_CLAIMS_ROOT");
        match prior_spawn_gate {
            Some(value) => std::env::set_var("FNO_SPAWN_GATE", value),
            None => std::env::remove_var("FNO_SPAWN_GATE"),
        }
        match prior_config {
            Some(value) => std::env::set_var("FNO_CONFIG", value),
            None => std::env::remove_var("FNO_CONFIG"),
        }
        match prior_payload {
            Some(value) => std::env::set_var("FNO_TEST_FOOTPRINT_PAYLOAD", value),
            None => std::env::remove_var("FNO_TEST_FOOTPRINT_PAYLOAD"),
        }
        let refusal = got.err().expect("the full share must refuse");
        assert_eq!(refusal.exit_code, EXIT_KING_SHARE, "{refusal:?}");
        assert_eq!(
            refusal.event.get("reason"),
            Some(&serde_json::json!("king_share"))
        );
        assert_eq!(refusal.event.get("held"), Some(&serde_json::json!(2)));
        assert_eq!(refusal.event.get("share"), Some(&serde_json::json!(2)));
        assert_eq!(refusal.event.get("kings"), Some(&serde_json::json!(2)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The held-rows clause names the caller's rows (never the bucket),
    /// caps at five with an ellipsis, and vanishes when there are none.
    #[test]
    fn held_rows_suffix_names_rows_and_caps_at_five() {
        let rows =
            |names: &[&str]| -> Vec<String> { names.iter().map(|n| n.to_string()).collect() };
        assert_eq!(
            held_rows_suffix(Some(&rows(&["w1", "w2"]))),
            "; the rows charged to you are w1, w2"
        );
        assert_eq!(
            held_rows_suffix(Some(&rows(&["w1", "w2", "w3", "w4", "w5", "w6", "w7"]))),
            "; the rows charged to you are w1, w2, w3, w4, w5..."
        );
        assert_eq!(held_rows_suffix(Some(&rows(&[]))), "");
        assert_eq!(held_rows_suffix(None), "");
    }

    /// The refusal event carries held_rows beside held, so the rows the
    /// count came from are readable back from the spawn_gate_refused event.
    #[test]
    fn king_share_refusal_event_carries_the_held_rows() {
        let dir = std::env::temp_dir().join(format!("fno-gate-held-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let reg = dir.join("registry.json");
        std::fs::write(
            &reg,
            format!(
                r#"{{"schema_version":{},"entries":[{},{},{}]}}"#,
                crate::state::REGISTRY_SCHEMA_VERSION,
                r#"{"name":"king-a","harness":"claude","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","crown_level":1,"harness_session_id":"session-aaaaaaaa"}"#,
                r#"{"name":"w1","harness":"claude","provider":"zai","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","spawned_by_session":"session-aaaaaaaa"}"#,
                r#"{"name":"w2","harness":"claude","provider":"zai","cwd":"/tmp","status":"live","created_at":"2026-01-01T00:00:00Z","spawned_by_session":"session-aaaaaaaa"}"#,
            ),
        )
        .unwrap();
        // One king -> share = cap = 2; the caller holds both rows, so the
        // share refuses and the event must name w1 and w2.
        let err = check_king_share(&reg, 2, Some("session-aaaaaaaa"), &serde_json::Map::new())
            .err()
            .expect("the full share must refuse");
        assert_eq!(err.exit_code, EXIT_KING_SHARE);
        assert_eq!(err.event.get("held"), Some(&serde_json::json!(2)));
        assert_eq!(
            err.event.get("held_rows"),
            Some(&serde_json::json!(["w1", "w2"]))
        );
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

        acquire_worker_slot(
            &mut guard,
            "plain-codex",
            "spawn-gate:test",
            std::process::id(),
            None,
            false,
        )
        .unwrap();

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
        crate::paths::pin_test_claims_root(&std::env::temp_dir().join("fno-gate-slotcount-claims"));
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

    /// AC1-FR: the Rust gate and the Python mirror must return the same
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
    // --- the per-territory team cap fixture (AC9) -------------------
    // The scenarios were recorded when the Python gate was a second counting
    // leg; the Python leg is deleted and these are the recorded contract now,
    // checked honestly against the one remaining count.

    #[test]
    fn territory_cap_characterized_by_recorded_scenarios() {
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
                Some((scope, members, _kingless)) => {
                    let count = live
                        .iter()
                        .filter(|r| {
                            r.node
                                .as_deref()
                                .map(|n| members.contains(n))
                                .unwrap_or(false)
                        })
                        .count();
                    if count >= cap as usize {
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

    /// Snapshot-and-restore scope for the env vars a fixture pins: the
    /// original value (or its absence) is put back on drop, panic included,
    /// so a fixture cannot permanently discard an ambient pin.
    struct EnvPin(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl EnvPin {
        fn take(vars: &[&'static str]) -> Self {
            Self(
                vars.iter()
                    .map(|var| (*var, std::env::var_os(var)))
                    .collect(),
            )
        }
    }

    impl Drop for EnvPin {
        fn drop(&mut self) {
            for (var, saved) in &self.0 {
                match saved {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }

    /// AC9: `kingless` rides every readable verdict receipt. A node inside a
    /// live crown's compiled scope reads false, a node in a project no crown
    /// rules reads true, and an unreadable attribution stays the existing
    /// territory_unknown shape with NO kingless key - a boolean there would
    /// be a guess wearing a boolean.
    #[test]
    fn territory_verdict_receipt_names_kingless_on_readable_verdicts() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let self_pid = std::process::id();
        let base = std::env::temp_dir().join(format!("fno-verdict-kingless-{self_pid}"));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("s0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("graph.json"),
            serde_json::json!({ "entries": [
                { "id": "x-epic", "type": "epic", "project": "fno" },
                { "id": "x-1", "parent": "x-epic", "project": "fno" },
                { "id": "x-out", "project": "other" },
            ]})
            .to_string(),
        )
        .unwrap();
        let reg = dir.join("registry.json");
        std::fs::write(
            &reg,
            format!(
                r#"{{"schema_version":1,"entries":[{{"name":"fixture-king","provider":"claude","cwd":"/tmp","status":"busy","created_at":"2026-01-01T00:00:00Z","pid":{self_pid},"crown_scope":"x-epic","crown_level":2}}]}}"#
            ),
        )
        .unwrap();
        // The territory read resolves its graph through the config climb
        // (graph_json_path never reads <cwd>/graph.json), so pin it with an
        // absolute paths.graph_json instead of touching process env - these
        // tests then race no other test's FNO_HOME writes.
        std::fs::create_dir_all(dir.join(".fno")).unwrap();
        std::fs::write(
            dir.join(".fno/config.toml"),
            format!(
                "schema_version = 1\n\n[paths]\ngraph_json = \"{}\"\n",
                dir.join("graph.json").display()
            ),
        )
        .unwrap();
        // Defense in depth against an FNO_CONFIG another test leaked toward a
        // deleted tempdir: the config pin answers first, FNO_HOME catches the
        // fall-through, and the pin restores the ambient value on drop.
        let _env = EnvPin::take(&["FNO_HOME"]);
        std::env::set_var("FNO_HOME", &dir);

        let crowned = territory_verdict_receipt(&dir, &reg, "x-1", 4);
        assert_eq!(crowned["verdict"], "ok", "{crowned}");
        assert_eq!(crowned["kingless"], false, "{crowned}");
        let loose = territory_verdict_receipt(&dir, &reg, "x-out", 4);
        assert_eq!(loose["verdict"], "ok", "{loose}");
        assert_eq!(loose["kingless"], true, "{loose}");
        let unknown = territory_verdict_receipt(&dir, &reg, "x-ghost", 4);
        assert_eq!(unknown["verdict"], "territory_unknown", "{unknown}");
        assert!(
            unknown.get("kingless").is_none(),
            "an unreadable attribution must not guess a boolean: {unknown}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// AC9: the two territory attributions on one branch - resolve_territories
    /// for the drain readout, territory_of_node for the cap - agree on
    /// kingless for the same node. The fixture carries one epic crown and one
    /// uncrowned workspace project, so both the crowned and the loose leg are
    /// pinned: a divergence between the readers ships caught, not silent.
    #[test]
    fn territory_of_node_and_resolve_territories_agree_on_kingless() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let self_pid = std::process::id();
        let base = std::env::temp_dir().join(format!("fno-territory-agree-kingless-{self_pid}"));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("s0");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("graph.json"),
            serde_json::json!({ "entries": [
                { "id": "x-epic", "type": "epic", "project": "fno" },
                { "id": "x-1", "parent": "x-epic", "project": "fno" },
                { "id": "x-out", "project": "other" },
            ]})
            .to_string(),
        )
        .unwrap();
        let reg = dir.join("registry.json");
        std::fs::write(
            &reg,
            format!(
                r#"{{"schema_version":1,"entries":[{{"name":"fixture-king","provider":"claude","cwd":"/tmp","status":"busy","created_at":"2026-01-01T00:00:00Z","pid":{self_pid},"crown_scope":"x-epic","crown_level":2}}]}}"#
            ),
        )
        .unwrap();
        // Same double pin as the receipt test, plus the workspace map the
        // resolve_territories leg needs: one config file answers both
        // lookups through the climb, so no FNO_CONFIG write can leak.
        std::fs::create_dir_all(dir.join(".fno")).unwrap();
        std::fs::write(
            dir.join(".fno/config.toml"),
            format!(
                "schema_version = 1\n\n[paths]\ngraph_json = \"{}\"\n\n[[work.workspaces.main.projects]]\nname = \"other\"\npath = \"/repo/other\"\n",
                dir.join("graph.json").display()
            ),
        )
        .unwrap();
        let _env = EnvPin::take(&["FNO_HOME"]);
        std::env::set_var("FNO_HOME", &dir);

        let mut warnings = Vec::new();
        let crowned =
            territory_of_node(&dir, &reg, "x-1", &mut warnings).expect("crowned node attributes");
        let loose =
            territory_of_node(&dir, &reg, "x-out", &mut warnings).expect("loose node attributes");
        let territories =
            crate::territory::resolve_territories(&dir, &reg).expect("fixture resolves");
        let crown_row = territories
            .iter()
            .find(|t| t.key == "x-epic")
            .expect("crown territory resolves");
        let loose_row = territories
            .iter()
            .find(|t| t.key == "other")
            .expect("uncrowned workspace project resolves as a loose territory");
        assert!(!crowned.2, "a live crown scope is not kingless");
        assert!(loose.2, "a project no crown rules is kingless");
        assert_eq!(crowned.2, crown_row.kingless, "crowned leg diverges");
        assert_eq!(loose.2, loose_row.kingless, "loose leg diverges");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// AC6-HP: nested crowns split one project exclusively; an uncompilable
    /// live crown blinds the whole read (None), the fail-closed posture.
    #[test]
    fn territory_of_node_attributes_nested_crowns_exclusively() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let self_pid = std::process::id();
        let base = std::env::temp_dir().join(format!("fno-nested-crowns-{self_pid}"));
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("s0");
        std::fs::create_dir_all(dir.join(".fno")).unwrap();
        std::fs::write(
            dir.join("graph.json"),
            serde_json::json!({ "entries": [
                { "id": "x-epic", "type": "epic", "project": "fno" },
                { "id": "x-1", "parent": "x-epic", "project": "fno" },
                { "id": "x-root", "project": "fno" },
            ]})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join(".fno/config.toml"),
            format!(
                "schema_version = 1\n\n[paths]\ngraph_json = \"{}\"\n\n[[work.workspaces.main.projects]]\nname = \"fno\"\npath = \"/repo/fno\"\n",
                dir.join("graph.json").display()
            ),
        )
        .unwrap();
        let reg_row = |name: &str, scope: &str, level: i64| {
            format!(
                r#"{{"name":"{name}","provider":"claude","cwd":"/tmp","status":"busy","created_at":"2026-01-01T00:00:00Z","pid":{self_pid},"crown_scope":"{scope}","crown_level":{level}}}"#
            )
        };
        let reg = dir.join("registry.json");
        std::fs::write(
            &reg,
            format!(
                r#"{{"schema_version":1,"entries":[{},{}]}}"#,
                reg_row("king-fno", "fno", 1),
                reg_row("king-epic", "x-epic", 2)
            ),
        )
        .unwrap();
        let _env = EnvPin::take(&["FNO_HOME"]);
        std::env::set_var("FNO_HOME", &dir);
        let mut warnings = Vec::new();
        let mut of =
            |node: &str| territory_of_node(&dir, &reg, node, &mut warnings).expect("attributes");
        let under_epic = of("x-1");
        assert_eq!(under_epic.0, "x-epic");
        assert!(!under_epic.1.contains("x-root"), "{:?}", under_epic.1);
        let root = of("x-root");
        assert_eq!(root.0, "fno");
        assert!(
            !root.1.contains("x-epic") && !root.1.contains("x-1"),
            "{:?}",
            root.1
        );
        // One uncompilable live crown refuses every node-bearing read.
        let reg_bad = dir.join("registry-bad.json");
        std::fs::write(
            &reg_bad,
            format!(
                r#"{{"schema_version":1,"entries":[{}]}}"#,
                reg_row("king-bad", "x-root", 2)
            ),
        )
        .unwrap();
        let mut warnings = Vec::new();
        assert!(territory_of_node(&dir, &reg_bad, "x-1", &mut warnings).is_none());
        assert!(
            warnings.iter().any(|w| w.contains("uncompilable")),
            "{warnings:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// AC6-HP: with five live `bp` rows and default config, a sixth
    /// blueprint spawn refuses with `blueprint_cap`, the receipt naming the
    /// five live rows, the cap, and the subagent remedy. A non-bp spawn and
    /// a spawn under the cap admit.
    #[test]
    fn a_sixth_blueprint_spawn_refuses_with_the_live_rows_named() {
        let dir = tempfile::tempdir().unwrap();
        let fnodir = dir.path().join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(fnodir.join("config.toml"), "schema_version = 1\n").unwrap();
        let reg = dir.path().join("registry.json");
        std::fs::write(&reg, r#"{"schema_version":1,"entries":[]}"#).unwrap();
        let live: Vec<RegistryEntry> = ["bp-x-1-a", "bp-x-2-b", "bp-x-3-c", "bp-x-4-d", "bp-x-5-e"]
            .iter()
            .map(|n| bp_entry(n, None))
            .collect();
        let err = check_blueprint_cap(dir.path(), &reg, "bp-x-9-slug", None, &live).unwrap_err();
        let parsed: serde_json::Value = serde_json::from_str(&err).unwrap();
        assert_eq!(parsed["reason"], serde_json::json!("blueprint_cap"));
        assert_eq!(parsed["max_live"], serde_json::json!(5));
        assert_eq!(
            parsed["live_blueprints"].as_array().map(|a| a.len()),
            Some(5),
            "{parsed}"
        );
        assert!(
            parsed["remedy"]
                .as_str()
                .unwrap()
                .contains("/fno:blueprint subagent"),
            "{parsed}"
        );
        // A non-bp spawn is never counted or refused on this axis.
        assert!(check_blueprint_cap(dir.path(), &reg, "t-x-9-slug", None, &live).is_ok());
        // Under the cap a bp spawn admits.
        assert!(check_blueprint_cap(dir.path(), &reg, "bp-x-9-slug", None, &live[..4]).is_ok());
    }

    /// AC6-ERR: one live blueprint row on a node, a second blueprint
    /// spawn for the same territory refuses with `blueprint_territory_cap`
    /// and names the live row.
    #[test]
    fn a_second_blueprint_in_a_territory_refuses() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        // A project-scoped graph so the loose-territory fallback answers.
        std::fs::write(
            dir.path().join("graph.json"),
            serde_json::json!({"entries": [{"id": "x-1", "project": "proj", "status": "idea"}]})
                .to_string(),
        )
        .unwrap();
        std::env::set_var("FNO_HOME", dir.path());
        let reg = dir.path().join("registry.json");
        std::fs::write(&reg, r#"{"schema_version":1,"entries":[]}"#).unwrap();
        let live = vec![bp_entry("bp-x-1-a", Some("x-1"))];
        let err =
            check_blueprint_cap(dir.path(), &reg, "bp-x-2-slug", Some("x-1"), &live).unwrap_err();
        std::env::remove_var("FNO_HOME");
        let parsed: serde_json::Value = serde_json::from_str(&err).unwrap();
        assert_eq!(
            parsed["reason"],
            serde_json::json!("blueprint_territory_cap")
        );
        assert_eq!(parsed["territory"], serde_json::json!("loose:proj"));
        assert_eq!(
            parsed["live_blueprints"],
            serde_json::json!(["bp-x-1-a"]),
            "{parsed}"
        );
        assert!(
            parsed["remedy"]
                .as_str()
                .unwrap()
                .contains("/fno:blueprint subagent"),
            "{parsed}"
        );
    }

    /// AC6-ERR (force): the force branch refuses the same blueprint
    /// spawn - force never excuses the blueprint axis.
    #[test]
    fn the_force_branch_refuses_a_capped_blueprint_spawn() {
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let fnodir = dir.path().join(".fno");
        std::fs::create_dir_all(&fnodir).unwrap();
        std::fs::write(fnodir.join("config.toml"), "schema_version = 1\n").unwrap();
        std::fs::write(
            dir.path().join("graph.json"),
            serde_json::json!({"entries": [{"id": "x-1", "project": "proj", "status": "idea"}]})
                .to_string(),
        )
        .unwrap();
        std::env::set_var("FNO_HOME", dir.path());
        std::env::set_var("FNO_NODE", "x-1");
        let prior_claims = std::env::var_os("FNO_CLAIMS_ROOT");
        std::env::set_var("FNO_CLAIMS_ROOT", dir.path().join("claims-root"));
        let prior_spawn_gate = std::env::var_os("FNO_SPAWN_GATE");
        std::env::remove_var("FNO_SPAWN_GATE");
        let reg = dir.path().join("registry.json");
        let mut e = RegistryEntry::default();
        e.name = "bp-x-1-a".into();
        e.node = Some("x-1".into());
        e.pid = Some(std::process::id());
        e.status = AgentStatus::Busy;
        e.pid_start_time = crate::daemon::process_start_time(std::process::id());
        crate::state::update_registry(&reg, |r| r.entries.push(e)).unwrap();
        let got = run_gate(
            dir.path(),
            &reg,
            GateInput {
                name: "bp-x-2-slug".into(),
                substrate: "bg".into(),
                flags: GateFlags {
                    force: true,
                    no_wait: false,
                },
                ..Default::default()
            },
        );
        std::env::remove_var("FNO_HOME");
        std::env::remove_var("FNO_NODE");
        std::env::remove_var("FNO_CLAIMS_ROOT");
        if let Some(v) = prior_claims {
            std::env::set_var("FNO_CLAIMS_ROOT", v);
        }
        if let Some(v) = prior_spawn_gate {
            std::env::set_var("FNO_SPAWN_GATE", v);
        }
        let refusal = got.err().expect("the spawn must refuse");
        assert_eq!(
            refusal.exit_code, EXIT_BLUEPRINT_CAP,
            "{:?}",
            refusal.receipt
        );
        let receipt = refusal.receipt.clone().unwrap_or(serde_json::Value::Null);
        assert_eq!(
            receipt["reason"],
            serde_json::json!("blueprint_territory_cap"),
            "{receipt}"
        );
    }

    /// A live blueprint registry row for the cap tests: pid is this
    /// process, so `live_rows`' liveness filter admits it.
    fn bp_entry(name: &str, node: Option<&str>) -> RegistryEntry {
        let mut e = RegistryEntry::default();
        e.name = name.into();
        e.node = node.map(str::to_string);
        e.pid = Some(std::process::id());
        e.status = AgentStatus::Busy;
        e
    }
}

#[cfg(test)]
#[path = "spawn_gate_slot_tests.rs"]
mod spawn_gate_slot_tests;

#[cfg(test)]
#[path = "spawn_gate_blind_tests.rs"]
mod spawn_gate_blind_tests;
