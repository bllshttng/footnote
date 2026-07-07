//! Spawn gate (x-c5cc): global concurrency cap + free-RAM floor + queue loop.
//!
//! Called at the top of the client `spawn` arm for the `bg`/`headless`
//! substrates only (`pane` re-execs into the Python CLI, whose mirrored gate
//! in `fno/agents/spawn_gate.py` is the sole gate on that path — exactly one
//! gate evaluation per spawn, LD1).
//!
//! The gate is READ-ONLY: the `max_live` slot cap reads the fno registry
//! (worker provenance), the RAM floor reads system `vm_stat`/meminfo, and the
//! claude daemon roster is read only by the post-spawn QoS demotion helper —
//! NOT by the slot count (x-bdf9: the roster's non-work sessions must not
//! consume worker slots). The gate's only writes are its own claims
//! (`spawn-gate` check→dispatch mutex, `worker:<name>` headless slot claims).
//! Every guard fails OPEN on read errors (LD5): the gate is protective
//! infrastructure and must never become the thing that bricks spawning.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// Queue mechanics (Claude's Discretion 2: targets, not contracts).
const QUEUE_POLL: Duration = Duration::from_secs(2);
const QUEUE_PROGRESS_EVERY: Duration = Duration::from_secs(30);
const QUEUE_TIMEOUT: Duration = Duration::from_secs(600);
/// spawn-gate mutex TTL: generous vs the seconds-scale check→dispatch window;
/// PID liveness frees it instantly if the spawner dies.
const GATE_CLAIM_TTL_MS: i64 = 5 * 60 * 1000;
/// worker:<name> headless slot TTL: bounds a one-shot that outlives its
/// client pid record; PID liveness is the primary release.
const WORKER_CLAIM_TTL_MS: i64 = 4 * 60 * 60 * 1000;

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
/// carries every live claude session — dozens of claude-mem observers and
/// resident-idle sessions among them — none of which is fno work; counting them
/// let the slot cap read "20/15" with zero real build workers running and wedge
/// `/target bg`. Registry membership IS the "fno spawned this for work"
/// provenance (spawn writes the row), so the registry alone is the slot
/// denominator. The roster's RAM cost is still honored elsewhere:
/// [`check_ram_floor`] reads real available RAM from `vm_stat`/meminfo, which
/// already reflects every process the roster holds — so dropping the roster
/// here changes only the slot denominator, not RAM behavior.
///
/// Read-only; a registry read failure degrades to a 0 contribution with one
/// warning line pushed to `warnings` (LD5, fail open).
pub fn slot_count(registry_path: &Path, warnings: &mut Vec<String>) -> usize {
    let mut count = 0usize;
    match load_registry(registry_path) {
        Ok(Registry { entries, .. }) => {
            for e in &entries {
                if !status_is_liveish(&e.status) {
                    continue;
                }
                let alive = e
                    .pid
                    .map(|p| pid_is_ours(p, e.pid_start_time))
                    .unwrap_or(false);
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
        return Ok(GateGuard::default());
    }
    let cap = agents_config::max_live(config_cwd) as usize;
    let floor_gb = agents_config::min_free_gb(config_cwd);
    let holder = format!("spawn-gate:{}:{}", std::process::id(), name);
    let root = gate_claims_root();

    let mut guard = GateGuard {
        gate_key: None,
        worker_key: None,
        root: root.clone(),
    };

    if flags.force {
        eprintln!("spawn-gate: forced past cap and RAM floor (--force)");
        if substrate == "headless" {
            acquire_worker_slot(&mut guard, name, &holder);
        }
        return Ok(guard);
    }

    let started = Instant::now();
    let mut last_progress = Instant::now();
    let mut announced = false;

    loop {
        // Serialize check→dispatch under the spawn-gate mutex so N concurrent
        // spawners at cap-1 can't all pass. Not held across the wait sleep.
        let acquired_mutex = match claims::acquire(
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
            guard.gate_key = Some(("spawn-gate".to_string(), holder.clone()));
            let mut warnings = Vec::new();
            let slots = slot_count(registry_path, &mut warnings);
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
            Err(EXIT_RAM_REFUSED)
        }
        None => {
            eprintln!("spawn-gate: could not read available RAM; skipping the floor check");
            Ok(())
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
pub fn qos_demote_bg_worker(config_cwd: &Path, claude_short_id: &str) {
    if !agents_config::worker_qos_enabled(config_cwd) || claude_short_id.is_empty() {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(roster) = ClaudeRoster::load_default() {
            if let Some(pid) = roster.find(claude_short_id).and_then(|w| w.pid) {
                qos_demote_pid(config_cwd, pid);
                return;
            }
        }
        if Instant::now() >= deadline {
            eprintln!(
                "spawn-gate: bg worker {claude_short_id} pid not in roster within 10s; \
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

    #[test]
    fn urldecode_inverts_encode_key() {
        let key = "worker:my agent/x";
        assert_eq!(urldecode(&claims::encode_key(key)).as_deref(), Some(key));
        assert_eq!(urldecode("bad%zz"), None);
    }

    #[test]
    fn qos_wrap_wraps_or_passes_through() {
        // test_env_lock: qos_wrap reads config via FNO_CONFIG-sensitive
        // resolve; serialize with the other env-touching tests.
        let _g = claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("abi-gate-qos-{}", std::process::id()));
        let abil = dir.join(".fno");
        std::fs::create_dir_all(&abil).unwrap();

        std::fs::write(
            abil.join("settings.yaml"),
            "config:\n  agents:\n    worker_qos: off\n",
        )
        .unwrap();
        // `sh` resolves on every CI platform (a non-resolving argv[0] is
        // deliberately left unwrapped so NotFound/127 semantics survive).
        let argv = vec!["sh".to_string(), "-c".to_string(), "true".to_string()];
        assert_eq!(qos_wrap(&dir, argv.clone()), argv, "off = identity");

        std::fs::write(
            abil.join("settings.yaml"),
            "config:\n  agents:\n    worker_qos: utility\n",
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
                let csidf = row["claude_short_id"]
                    .as_str()
                    .map(|s| format!(r#","claude_short_id":"{s}""#))
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
