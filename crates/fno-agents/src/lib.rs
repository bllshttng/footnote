//! `fno-agents` substrate crate (Phase 6).
#![recursion_limit = "512"]
//!
//! This crate is the Rust substrate for PTY-managed agents (codex / gemini /
//! future OpenCode). It is split per the design's Locked Decisions:
//!
//! - shared types (this module): [`ShortId`], [`AgentStatus`], [`ParsedEvent`]
//!   (LD9, sealed enum), [`MonotonicTimestamp`] (count-during-sleep clock).
//! - [`pty`]: PTY spawn + bounded-ring output drainer (LD31).
//! - [`write_queue`]: bounded-backpressure stdin queue + [`write_queue::WriteMsg`].
//! - [`supervisor`]: [`supervisor::RestartPolicy`] state machine + hard ceiling (LD36).
//! - [`readiness`]: [`readiness::ReadinessDetector`] trait + `UnknownReadinessSignal`
//!   (Open Question #9: no generic byte-count fallback; per-CLI signal mandatory).
//!
//! ## Scope of Wave 1 (this PR)
//!
//! Wave 0's smoke prototype (`cli/scripts/smoke/pty-survival/`) refuted the
//! "direct daemon-owned PTY survives daemon restart" assertion: a child on a
//! PTY whose master the supervisor owns is SIGHUP'd and dies the instant the
//! master closes. The locked outcome (Outcome B) was a per-agent worker process
//! that owned the master and outlived the daemon. That daemon-owned PTY hosting
//! was retired at G4: the mux is now the agent-PTY substrate, and this crate
//! keeps the registry, inside-leg reports, and the claude stream-json adopt lane.
//!
//! Deliberately deferred (documented seams, not gaps):
//! - `alacritty_terminal` grid wiring + per-CLI [`readiness::ReadinessDetector`]
//!   impls -> Wave 2, alongside the smoke captures that define the grid patterns
//!   (the trait operates over [`readiness::ScreenView`] so Wave 2 only adds impls).
//! - `tokio` runtime integration -> Wave 3 (the daemon is its only consumer; the
//!   substrate stays runtime-agnostic and is driven from `spawn_blocking`).
//!
//! ## Scope of Wave 2 (this PR)
//!
//! Wave 2 fills the seams Wave 1 left:
//! - [`provider`]: [`provider::Provider`] + [`provider::ProviderWithPty`] traits
//!   (LD8) and the three impls ([`provider::ClaudeProvider`] shellout,
//!   [`provider::CodexProvider`] / [`provider::GeminiProvider`] PTY-managed).
//! - [`envelope`]: [`envelope::Envelope`] structural anti-injection wrapper (LD15).
//! - [`screen`]: the terminal-grid construction behind [`readiness::ScreenView`]
//!   (the per-CLI [`readiness::ReadinessDetector`] impls now live in
//!   [`readiness`]).

// daemon.rs's `agent.list` row is one json! literal with a key set pinned by
// schemas/agents-list-row.json; the crate-level recursion_limit above covers
// the macro expansion since `spawned_by_session` joined the contract.

pub mod acceptance_evidence;
pub mod active_backlog;
pub mod additional_prs;
mod agent_lock;
pub mod agents_config;
pub mod agy_ask;
pub mod agy_hooks;
pub mod agy_launch;
pub mod announce;
pub mod arm_repair;
pub mod arm_watch;
pub mod attach;
pub mod attention;
pub mod attention_arm;
pub mod attention_file;
pub mod authorized_merge;
pub mod backlog;
pub mod backlog_ready;
pub mod bash_census;
#[cfg(test)]
#[path = "birth_guard_tests.rs"]
mod birth_guard_tests;
pub mod blueprint_judge;
mod bounded_cmd;
mod bounded_spawn;
mod cancel_sentinel;
pub mod canonical_check;
pub mod capability_leaves;
pub mod cargo_build_dirs;
pub mod census;
pub mod check_supersession;
pub mod claim_queue;
pub mod claim_store;
pub mod claim_verbs;
pub mod claims;
pub mod claims_root;
pub mod claude_adopt;
pub mod claude_ask;
pub mod claude_attach;
pub mod claude_drive;
pub mod claude_login;
pub mod claude_roster;
pub mod claude_sessions;
pub mod claude_stream_entry;
pub mod claude_supervisor;
pub mod cli_args;
pub mod client;
pub mod client_verbs;
pub mod codex_ask;
pub mod codex_daemon_readiness;
pub mod codex_daemon_upgrade;
/// Test support: a fake shared codex app-server daemon. Public because the
/// in-crate daemon tests and the integration tests both need one fake, and
/// only a library item reaches both.
#[doc(hidden)]
pub mod codex_fake_daemon;
pub mod codex_inject;
/// Public because `codex_resume` (pub, exercised by the parity test) names
/// [`CodexRoute`] in its signature.
pub mod codex_posture;
pub mod codex_route;
pub mod codex_store;
pub mod codex_thread;
mod codex_thread_entry;
pub mod compaction;
mod completion_output;
pub mod component_update;
pub mod context_run;
pub mod convert;
pub mod corrections_verify;
pub mod court_fold;
pub mod crown_alarm;
pub mod crown_reap;
pub mod crown_settle;
pub mod crown_split;
pub mod crown_widen;
pub mod cursor_agent;
pub mod daemon;
pub mod day;
pub mod decision_index;
pub mod delivery_completion;
pub mod digest;
pub mod disposition_gate;
pub mod distress;
pub mod drift;
pub mod duration;
pub mod envelope;
pub mod escalation;
pub mod eval_attempt;
pub mod evals_arm;
pub mod evals_macro;
pub mod evals_trend;
pub mod event_store;
pub mod events;
pub mod events_limits;
pub mod events_store;
pub mod evidence;
pub mod fallback_chain;
pub mod feed;
pub mod finalize;
pub mod finalize_run_summary;
pub mod fleet_incident;
pub mod fleet_load;
pub mod fleet_page;
pub mod fleet_task;
pub mod flight_gate;
pub mod gc;
pub mod gc_claude_stop;
pub mod gc_inventory;
pub mod gc_native;
pub mod gc_sweep;
pub mod gc_verify;
pub mod gemini_ask;
pub mod gh_budget;
#[cfg(test)]
mod git_test_helpers;
pub mod graph_get;
pub mod graph_keeper;
pub mod graph_store;
pub mod grok_store;
pub mod harness_capabilities;
pub mod harness_daemon;
pub mod heal;
pub mod honesty_sweep;
pub mod hook;
mod identity;
pub mod install_verify;
pub mod intel;
pub mod intel_insights;
pub mod interrupt_classify;
pub mod json_output;
pub mod kill_criteria;
pub mod king_board;
pub mod king_checkin;
pub mod king_escalation;
pub mod king_history;
pub mod king_ledger;
pub mod king_term;
pub mod king_termination;
pub mod king_verdict_inputs;
pub mod lane_heal;
pub mod launch_workdir;
pub mod law_match;
mod lifecycle_child;
pub mod live_store_fence;
pub mod liveness_sweep;
pub mod logs;
pub mod logs_client;
pub mod loop_dispatch;
pub mod loop_king;
pub mod loop_reign;
pub mod loop_runtime;
pub mod loop_target;
pub mod loopcheck;
pub mod loops_pause;
pub mod machine_sample;
pub mod machine_watch;
pub mod mail_inject;
pub mod manifest;
pub mod manifest_lookup;
pub mod merge_close;
pub mod merge_gates;
pub mod merge_grant;
pub mod merge_hold;
pub mod merge_posture;
pub mod merge_reap;
#[cfg(test)]
#[path = "mint_guard_tests.rs"]
mod mint_guard_tests;
pub mod model_env_scrub;
pub mod naming;
pub mod needs;
pub mod node_origin;
pub mod node_reading;
pub mod node_route;
pub mod node_seed;
pub mod nudge;
pub mod occupancy_login;
pub mod opencode_ask;
pub mod opencode_install;
pub mod opencode_serve;
pub mod opencode_transcript;
pub mod operator_notice;
pub mod operator_turns;
pub mod operator_witness;
pub mod orphan_reap;
pub mod osc;
pub mod pane_keeper;
pub mod pane_relaunch;
pub mod pane_stop;
pub mod paths;
pub mod pending_session_row;
pub mod pi;
pub mod planning_lane;
pub mod plans_dirs;
pub mod plugin_install;
pub mod pr_body_check;
pub mod pr_nudge;
pub mod pr_park;
pub mod pr_push;
pub mod pr_rebase;
pub mod pr_status_facts;
pub mod protocol;
pub mod prove_it_verdicts;
pub mod provenance;
pub mod provider;
pub mod provider_cap;
pub mod provider_cap_verbs;
pub mod publish_review;
pub mod quarantine;
pub mod question_intake;
pub mod question_sweep;
pub mod quiet_retire;
pub mod readiness;
pub mod real_session;
pub mod reap_release;
pub mod reap_render;
pub mod receipt;
pub mod reclaim;
pub mod reentry;
pub mod refusal_rate;
pub mod registry_json;
pub mod rename;
pub mod restart_run;
pub mod resume_args;
pub mod resume_gate;
pub mod resume_pin;
pub mod resume_receipt;
pub mod resume_wake;
pub mod review_freshness;
pub mod review_summary;
pub mod rm_receipt;
pub mod roster_progress;
pub mod roster_reap;
pub mod route_capacity;
pub mod route_slot;
pub mod row_truth;
pub mod run_outcome;
pub mod run_state;
pub mod sandbox_probe;
pub mod scoreboard;
pub mod scrape;
pub mod scratch;
pub mod screen;
pub mod select_read;
pub(crate) mod served_liveness;
pub mod session_activity;
pub mod session_cost;
pub mod session_names_fold;
pub mod session_start_bytes;
pub mod single_flight;
pub mod source_pin;
pub mod spawn;
pub mod spawn_axes;
pub mod spawn_backends;
pub mod spawn_context;
pub mod spawn_contract;
pub mod spawn_edge;
pub mod spawn_gate;
pub mod spawn_gate_lanes;
pub mod spawn_gate_reservations;
pub mod spawn_gate_verb;
pub mod spawn_lineage;
pub mod spawn_overlay;
pub mod spawn_payload;
pub mod spawn_transaction;
pub mod state;
pub mod state_path;
pub mod store_exec;
pub mod stream_worker;
pub mod stuck_work;
pub mod subprocess_ask;
pub mod subscribe;
pub mod supervisor;
pub mod surface_check;
pub mod sync_canonical;
pub mod task_context;
pub mod terminal_stop;
pub mod territory;
pub mod test_run;
pub mod tick_ledger;
pub mod transcript_activity;
pub mod truth_probe;
pub mod usage;
pub mod verify_evidence;
pub mod version;
pub mod wait;
pub mod worktree_reapable;
pub mod write_queue;

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A short, opaque agent identifier (e.g. `wkA`). Stored in the registry and
/// used to name per-agent state directories. Validation is intentionally light
/// at this layer; dispatch-layer validation (US1 invariant) owns argv rules.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShortIdError {
    #[error("short id must be non-empty")]
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShortId(pub(crate) String);

impl ShortId {
    /// Construct a short id. The field is crate-private and this is the only
    /// constructor, so a zero-length registry key (which would collapse
    /// per-agent state directory paths) cannot be built at any call site.
    /// Charset rules beyond non-empty remain the dispatch layer's
    /// responsibility (US1 argv validation).
    pub fn new(s: impl Into<String>) -> Result<Self, ShortIdError> {
        let s = s.into();
        if s.is_empty() {
            return Err(ShortIdError::Empty);
        }
        Ok(ShortId(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ShortId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Agent lifecycle status. `state.status` is canonical; `registry.status` is a
/// denormalized projection of it (LD10). Serialized snake_case for the JSON
/// state files and the cross-language schemas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// PTY spawned, not yet confirmed ready for input.
    #[default]
    Spawning,
    /// Confirmed ready for input (readiness_detector reported ready).
    Ready,
    /// Alive and waiting (equivalent to `Ready` for drive-eligibility, LD28).
    Idle,
    /// Mid-reply / actively processing.
    Busy,
    /// Live shorthand used by the registry projection.
    Live,
    /// Restart policy is backing off before re-spawn.
    Restarting,
    /// Reachability probe failed; needs reconcile or rm.
    Orphaned,
    /// Per-agent task panicked (provider parse panic, etc.); restart policy applies.
    Failed,
    /// Child exited; registry entry retained until rm.
    Exited,
    /// Restart hard ceiling hit (LD36); will not restart again.
    PermanentDead,
}

impl AgentStatus {
    /// Drive is accepted only for these statuses (LD28). `Idle`/`Live` are
    /// equivalent to `Ready` for drive purposes.
    pub fn is_drive_eligible(&self) -> bool {
        matches!(
            self,
            AgentStatus::Ready | AgentStatus::Idle | AgentStatus::Busy | AgentStatus::Live
        )
    }
}

/// Sealed event vocabulary every provider parses INTO (LD9). Variant additions
/// are a one-line crate-wide change; no per-provider enums. `#[serde(tag="kind")]`
/// matches the wire shape in the design's Architecture section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParsedEvent {
    SessionCreated {
        session_id: String,
    },
    OutputChunk {
        text: String,
    },
    ReplyComplete {
        text: String,
        duration_ms: u64,
    },
    ToolUse {
        name: String,
        args: Option<serde_json::Value>,
    },
    ProviderError {
        message: String,
    },
    /// A line the provider's parser did not recognize. Tee'd to timeline.jsonl
    /// as `unknown_stream_event` rather than dropped, so a provider version bump
    /// degrades gracefully (Silent-Failure-Hunter finding).
    Unknown {
        raw: String,
    },
}

/// A monotonic timestamp that **counts during system sleep**, used for
/// drive-window heartbeat math (LD17 + Domain Pitfall: macOS/Linux suspend
/// divergence).
///
/// Rust's `std::time::Instant` is inconsistent across platforms for the
/// sleep case: on macOS it uses `mach_continuous_time` (counts sleep), on
/// Linux it uses `CLOCK_MONOTONIC` (does NOT count sleep). A laptop-sleep
/// during a drive window must EXPIRE the window, so we standardize on the
/// count-during-sleep semantic on both:
///
/// - Linux: `clock_gettime(CLOCK_BOOTTIME)`.
/// - macOS: `mach_continuous_time()` converted to ns via `mach_timebase_info`.
///
/// Stored as nanoseconds since an unspecified epoch; only differences are
/// meaningful. Wall-clock `ts` for human audit lives in events.jsonl, tracked
/// independently (LD17).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MonotonicTimestamp(u64);

impl MonotonicTimestamp {
    /// Read the current count-during-sleep monotonic clock.
    pub fn now() -> Self {
        MonotonicTimestamp(raw_monotonic_nanos())
    }

    /// Nanoseconds elapsed since `earlier`. Saturates at 0 if `earlier` is in
    /// the future (clock readings are monotonic, so this only guards against a
    /// caller passing a later timestamp as `earlier`).
    pub fn duration_since(&self, earlier: MonotonicTimestamp) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }

    /// Convenience: elapsed since this timestamp until now.
    pub fn elapsed(&self) -> Duration {
        MonotonicTimestamp::now().duration_since(*self)
    }

    /// Raw nanoseconds, for persisting the heartbeat baseline to state.json.
    pub fn as_nanos(&self) -> u64 {
        self.0
    }

    /// Reconstruct from raw nanoseconds previously read via [`as_nanos`]. Used
    /// by the daemon (Wave 3) to restore a persisted heartbeat baseline. Only
    /// meaningful when paired with a `now()` from the same daemon incarnation's
    /// clock (the value is epoch-relative to the running clock).
    ///
    /// [`as_nanos`]: MonotonicTimestamp::as_nanos
    pub fn from_nanos(nanos: u64) -> Self {
        MonotonicTimestamp(nanos)
    }
}

#[cfg(target_os = "linux")]
fn raw_monotonic_nanos() -> u64 {
    // CLOCK_BOOTTIME includes time spent suspended (unlike CLOCK_MONOTONIC).
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, owned timespec; CLOCK_BOOTTIME is a valid clock
    // id on Linux >= 2.6.39.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) };
    if rc != 0 {
        // clock_gettime on a standard clock id effectively never fails on a
        // supported kernel, so treat it as a should-be-impossible fault and
        // make it LOUD rather than silent. Returning 0 is NOT a universal
        // fail-safe: if a *baseline* read failed, elapsed over-reports (window
        // expires early - safe); if a *current* read fails, elapsed under-
        // reports toward 0 (window could hang open - unsafe). We accept that
        // residual risk only because the failure cannot occur in practice, and
        // log so it never passes unnoticed.
        tracing::error!("clock_gettime(CLOCK_BOOTTIME) failed; monotonic reading degraded to 0");
        return 0;
    }
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec.max(0) as u64)
}

#[cfg(target_os = "macos")]
fn raw_monotonic_nanos() -> u64 {
    // mach_continuous_time() counts during sleep; convert mach ticks -> ns via
    // the timebase ratio (1/1 on current Apple hardware, but we must not assume
    // it). `libc` deprecated its mach timebase helpers and dropped
    // mach_continuous_time entirely (it lives in the `mach2` crate now), so we
    // declare the two libSystem symbols directly to avoid a macOS-only crate
    // dependency. Both are part of libSystem, linked by default on macOS.
    #[repr(C)]
    struct MachTimebaseInfo {
        numer: u32,
        denom: u32,
    }
    extern "C" {
        fn mach_continuous_time() -> u64;
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> libc::c_int;
    }
    use std::sync::OnceLock;
    static TIMEBASE: OnceLock<(u64, u64)> = OnceLock::new();
    let (numer, denom) = *TIMEBASE.get_or_init(|| {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: `info` is a valid, owned, repr(C) struct matching the C ABI;
        // mach_timebase_info fills it and returns a kern_return_t.
        let rc = unsafe { mach_timebase_info(&mut info) };
        if rc != 0 || info.denom == 0 {
            (1, 1)
        } else {
            (info.numer as u64, info.denom as u64)
        }
    });
    // SAFETY: no arguments; returns a monotonic tick count that counts sleep.
    let ticks = unsafe { mach_continuous_time() };
    // ns = ticks * numer / denom, computed in u128 to avoid overflow.
    ((ticks as u128 * numer as u128) / denom as u128) as u64
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn raw_monotonic_nanos() -> u64 {
    // Other POSIX targets are not shipped by Phase 6 (Windows is Phase 7+).
    // Fall back to CLOCK_MONOTONIC so the crate still compiles for dev on
    // such hosts; the suspend semantic is undefined there and not relied on.
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid owned timespec; CLOCK_MONOTONIC is POSIX-standard.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        tracing::error!("clock_gettime(CLOCK_MONOTONIC) failed; monotonic reading degraded to 0");
        return 0;
    }
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec.max(0) as u64)
}

/// Serializes every test that mutates the PROCESS `PATH` (or resolves a
/// subprocess through it): two tests racing `set_var` under the default
/// parallel test threads make the loser inherit the winner's stub dir - or a
/// deleted tempdir, which exits 127. Take `PATH_TEST_MUTEX` around both the
/// mutation and the PATH-dependent work. cfg(test) in the lib only.
#[cfg(test)]
pub static PATH_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold [`PATH_TEST_MUTEX`] for the rest of the scope, poisoning ignored: a
/// panicking test leaves the env restored by its own guard, so refusing the
/// lock afterwards would fail every later test instead of the broken one.
#[cfg(test)]
pub fn path_test_guard() -> std::sync::MutexGuard<'static, ()> {
    PATH_TEST_MUTEX
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The process `PATH` with `dir` in front. PREPEND, never replace: PATH is
/// process-global, so a test that replaces it takes the system tools away from
/// every concurrent test in the binary, and a stub only needs to win.
#[cfg(test)]
pub fn path_with(dir: &std::path::Path) -> std::ffi::OsString {
    let mut value = std::ffi::OsString::from(dir);
    if let Some(previous) = std::env::var_os("PATH") {
        value.push(":");
        value.push(previous);
    }
    value
}

/// Write `body` to `dir/name` as a 0755 executable and return the path.
///
/// A child /bin/sh writes the bytes, never this process. A write fd held
/// here is copied into the child of any sibling test thread that forks in
/// that window, and an exec of the stub then fails with ETXTBSY until that
/// child execs. A temp name plus rename does not help: the copied fd follows
/// the inode. The writer has exited before this returns.
#[cfg(test)]
pub(crate) fn write_exec_stub(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::io::Write;
    let path = dir.join(name);
    let mut child = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            r#"cat > "$1.tmp.$$" && chmod 755 "$1.tmp.$$" && mv -f "$1.tmp.$$" "$1""#,
            "sh",
        ])
        .arg(&path)
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn /bin/sh to write an exec stub");
    // A writer that fails early (a missing dir) closes its stdin first, so
    // this write can see a broken pipe. The exit status below names the
    // real failure, so the write error is not the one to report.
    let _ = child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(body.as_bytes());
    let status = child.wait().expect("wait for the stub writer");
    assert!(
        status.success(),
        "could not write exec stub {}: {status}",
        path.display()
    );
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    // AC2-HP: the host boot reading is a real past instant, and on macOS its
    // second count matches what `sysctl -n kern.boottime` prints.
    #[test]
    fn host_boot_epoch_ms_reads_a_past_boot() {
        let boot = host_boot_epoch_ms().expect("this host exposes a boot time");
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        assert!(boot > 0, "boot must be positive: {boot}");
        assert!(boot < now_ms, "boot must predate now: {boot} vs {now_ms}");
        let sysctl = std::process::Command::new("sysctl")
            .args(["-n", "kern.boottime"])
            .output();
        if let Ok(out) = sysctl {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(sec) = text
                .split("sec = ")
                .nth(1)
                .and_then(|rest| rest.split([',', ' ', '}']).next())
                .and_then(|v| v.parse::<i64>().ok())
            {
                let window_start = sec * 1000;
                assert!(
                    boot >= window_start && boot < window_start + 1000,
                    "boot ms {boot} outside boot second {sec}"
                );
            }
        }
    }

    // AC2-ERR: a non-positive boot second count is a failed reading.
    #[test]
    fn boot_ms_from_refuses_a_non_positive_second() {
        assert_eq!(boot_ms_from(0, 0), None);
        assert_eq!(boot_ms_from(-5, 0), None);
        assert_eq!(boot_ms_from(1, 0), Some(1000));
        assert_eq!(boot_ms_from(1789997638, 500_000), Some(1789997638500));
    }

    #[test]
    fn short_id_rejects_empty() {
        assert_eq!(ShortId::new(""), Err(ShortIdError::Empty));
        let ok = ShortId::new("wkA").unwrap();
        assert_eq!(ok.as_str(), "wkA");
    }

    #[test]
    fn agent_status_serde_roundtrip_is_snake_case() {
        let json = serde_json::to_string(&AgentStatus::PermanentDead).unwrap();
        assert_eq!(json, "\"permanent_dead\"");
        let back: AgentStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, AgentStatus::PermanentDead);
    }

    #[test]
    fn drive_eligibility_matches_ld28() {
        assert!(AgentStatus::Ready.is_drive_eligible());
        assert!(AgentStatus::Idle.is_drive_eligible());
        assert!(AgentStatus::Busy.is_drive_eligible());
        assert!(!AgentStatus::Restarting.is_drive_eligible());
        assert!(!AgentStatus::Exited.is_drive_eligible());
        assert!(!AgentStatus::PermanentDead.is_drive_eligible());
    }

    #[test]
    fn parsed_event_tagged_serde() {
        let ev = ParsedEvent::ReplyComplete {
            text: "hi".into(),
            duration_ms: 42,
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"kind\":\"reply_complete\""));
        let back: ParsedEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn parsed_event_unknown_preserves_raw() {
        let ev = ParsedEvent::Unknown {
            raw: "{\"new_event\":1}".into(),
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: ParsedEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn monotonic_clock_is_nondecreasing_and_measures_elapsed() {
        let t0 = MonotonicTimestamp::now();
        std::thread::sleep(Duration::from_millis(20));
        let t1 = MonotonicTimestamp::now();
        assert!(t1 >= t0, "monotonic clock went backwards");
        let elapsed = t1.duration_since(t0);
        assert!(
            elapsed >= Duration::from_millis(15),
            "elapsed too small: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "elapsed implausibly large: {elapsed:?}"
        );
    }

    #[test]
    fn duration_since_future_saturates_to_zero() {
        let t0 = MonotonicTimestamp::now();
        std::thread::sleep(Duration::from_millis(5));
        let t1 = MonotonicTimestamp::now();
        // Passing the later ts as `earlier` must not panic or underflow.
        assert_eq!(t0.duration_since(t1), Duration::ZERO);
    }

    // ── cv-114f75cc: production emit-kind completeness guard ──────────────
    // KNOWN_EVENT_KINDS is hand-maintained and feeds both the Branch B `kind`
    // schema enum and the cross-language parity gate, so a new `.emit("foo")`
    // whose kind was never added to the constant would silently drift those
    // surfaces. This test scans every production call site and fails on drift.

    // ── fire the registry check HERE, not only in CI ──────────────────────
    /// The reign events: a king journals these from the session, and
    /// `fno doctor event audit --type` resolves the name through this table.
    /// A kind dropped here makes the done-probe read "unknown type", which is
    /// the absence-lie in audit form.
    #[test]
    fn event_table_knows_reign() {
        for kind in [
            "reign_armed",
            "reign_checkin",
            "reign_dispatch_exception",
            "king_term",
        ] {
            assert!(
                KNOWN_EVENT_KINDS.contains(&kind),
                "{kind} missing from KNOWN_EVENT_KINDS"
            );
        }
    }

    // `KNOWN_EVENT_KINDS` and `schema.yaml` cannot be generated from each
    // other: the YAML entry carries description/sources/data/consumers the
    // const does not have, and the two sets are a deliberate partition. So
    // this one stays a check; what changes is WHERE it fires. Step 6 of
    // scripts/check-event-schema-parity.sh needs a built binary and WARNs
    // when it is absent, so a missing entry surfaced only after a push, which
    // cost a CI cycle. A unit test costs nothing and fires on the dev machine.
    #[test]
    fn known_event_kinds_documented_in_schema_yaml() {
        use std::collections::BTreeSet;

        let Some(root) = repo_root_for_test() else {
            return;
        };
        let schema_path = root.join("cli/src/fno/events/schema.yaml");
        if !schema_path.is_file() {
            // The crates.io tarball case, and only that case: no `cli/` tree
            // to read. A parse failure below is NOT swallowed the same way.
            return;
        }
        let text = std::fs::read_to_string(&schema_path).expect("read schema.yaml");
        let parsed: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&text).expect("schema.yaml must parse");
        let documented: BTreeSet<&str> = parsed["event_types"]
            .as_sequence()
            .expect("schema.yaml must carry an event_types sequence")
            .iter()
            .filter_map(|entry| entry["name"].as_str())
            .collect();

        let missing: Vec<&str> = KNOWN_EVENT_KINDS
            .iter()
            .copied()
            .filter(|kind| !documented.contains(kind))
            .collect();
        assert!(
            missing.is_empty(),
            "KNOWN_EVENT_KINDS entries missing from cli/src/fno/events/schema.yaml: {}\n\
             Add an event_types entry for each, or drop the kind from the registry.",
            missing.join(", ")
        );
    }

    /// Repo root for a test that reads across the tree, or `None` off a checkout.
    fn repo_root_for_test() -> Option<std::path::PathBuf> {
        let out = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let top = String::from_utf8(out.stdout).ok()?;
        let top = top.trim();
        if top.is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(top))
        }
    }

    #[test]
    fn every_production_emit_kind_is_registered() {
        use std::collections::BTreeSet;

        let known: BTreeSet<&str> = KNOWN_EVENT_KINDS.iter().copied().collect();
        let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

        let mut files = Vec::new();
        collect_rs_files(&src_root, &mut files);
        assert!(!files.is_empty(), "found no .rs files under {src_root:?}");

        let mut unregistered: Vec<String> = Vec::new();
        let mut production_kinds: BTreeSet<String> = BTreeSet::new();
        let mut scanned_calls = 0usize;
        for file in &files {
            let text = std::fs::read_to_string(file).expect("read source file");
            // Production code only: truncate at the first `#[cfg(test)]` marker.
            // Tests live at the bottom by Rust convention, so test fixtures like
            // `.emit("tick")` are excluded. (Verified: every production emit in
            // this crate precedes its file's first `#[cfg(test)]`.)
            let prod = match text.find("#[cfg(test)]") {
                Some(i) => &text[..i],
                None => &text[..],
            };
            let file_name = file.file_name().unwrap().to_string_lossy();
            for (kind, line) in scan_emit_kinds(prod) {
                scanned_calls += 1;
                production_kinds.insert(kind.clone());
                if !known.contains(kind.as_str()) {
                    unregistered.push(format!(
                        "{file_name}:{line}: .emit(\"{kind}\") not in KNOWN_EVENT_KINDS"
                    ));
                }
            }
        }

        assert!(
            scanned_calls > 0,
            "scanner found zero emit call sites - the scan pattern likely broke"
        );

        // cv-2801ed8a: enforce the truncation assumption rather than just
        // documenting it. The scan above trusts that every production emit
        // precedes its file's first `#[cfg(test)]`. Verify it: scan BELOW each
        // boundary too, and require every kind found there to be either also
        // emitted in production (so the registration guard above already saw
        // it) or a known test-only fixture. A production-looking kind that
        // lives only below a boundary would otherwise escape the guard
        // silently. `production_kinds` must be complete across ALL files before
        // this check (a kind can be production in one file and test-only in
        // another), so this is a second pass.
        //
        // `tick`/`heartbeat` are test fixture emits; `foo`/`x` are `.emit(...)`
        // examples inside doc comments in the test module that the byte-level
        // scanner picks up. (Escaped `.emit(\"...\")` in the scanner self-check
        // string is NOT matched: the char after `(` is a backslash, not `"`.)
        // `mux_pane_counters`/`operator_decision` are real kinds whose production
        // emitters live outside this crate (the mux server shells out to the
        // Python CLI; operator_decision is Python-only); the routing unit tests
        // in events.rs emit them below the test boundary.
        const TEST_ONLY_EMIT_KINDS: &[&str] = &[
            "tick",
            "heartbeat",
            "foo",
            "x",
            "mux_pane_counters",
            "operator_decision",
        ];
        let test_only: BTreeSet<&str> = TEST_ONLY_EMIT_KINDS.iter().copied().collect();

        let mut below_only: Vec<String> = Vec::new();
        for file in &files {
            let text = std::fs::read_to_string(file).expect("read source file");
            let boundary = match text.find("#[cfg(test)]") {
                Some(i) => i,
                None => continue,
            };
            // scan_emit_kinds reports lines relative to its input slice; add the
            // newline count before the boundary so the message points at the
            // real file line.
            let base_line = text[..boundary].bytes().filter(|&c| c == b'\n').count();
            let file_name = file.file_name().unwrap().to_string_lossy();
            for (kind, line) in scan_emit_kinds(&text[boundary..]) {
                if production_kinds.contains(&kind) || test_only.contains(kind.as_str()) {
                    continue;
                }
                below_only.push(format!(
                    "{file_name}:{}: .emit(\"{kind}\") appears only below #[cfg(test)] \
                     (not emitted in production, not a known test-only fixture)",
                    base_line + line
                ));
            }
        }

        assert!(
            below_only.is_empty(),
            "emit kinds found only below a #[cfg(test)] boundary - the truncation \
             assumption (all production emits precede the test module) may be \
             violated. If a kind below is a real production emit, register it in \
             KNOWN_EVENT_KINDS and move it above the test module; if it is \
             test-only, add it to TEST_ONLY_EMIT_KINDS:\n  {}",
            below_only.join("\n  ")
        );

        // Self-check: the scanner extracts a single-line `.emit(` kind, a
        // multi-line `.emit_fields(` kind, AND a whitespace-before-paren
        // `.emit (` kind (valid Rust), so a genuine unregistered kind cannot
        // slip past this guard silently. Also asserts the reported line number.
        let synthetic = "x.emit(\"agent_spawned\", &p);\n  y.emit_fields(\n    \"definitely_not_a_real_kind\", m);\n z.emit (\"another_fake_kind\");";
        let scanned = scan_emit_kinds(synthetic);
        assert!(
            scanned.iter().any(|(k, l)| k == "agent_spawned" && *l == 1),
            "scanner missed a single-line emit kind (or wrong line)"
        );
        assert!(
            scanned
                .iter()
                .any(|(k, _)| k == "definitely_not_a_real_kind"),
            "scanner missed a multi-line emit_fields kind"
        );
        assert!(
            scanned.iter().any(|(k, _)| k == "another_fake_kind"),
            "scanner missed a `.emit (` call with whitespace before the paren"
        );
        assert!(
            !known.contains("definitely_not_a_real_kind") && !known.contains("another_fake_kind"),
            "the synthetic drift kinds must not be real registered kinds"
        );
        assert!(
            unregistered.is_empty(),
            "production emit kinds missing from KNOWN_EVENT_KINDS:\n  {}",
            unregistered.join("\n  ")
        );
    }

    fn collect_rs_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    /// Extract `(kind, line)` for every `.emit` / `.emit_fields` call with a
    /// string-literal kind. Whitespace tolerant on both sides: `.emit ("x")`
    /// and a newline between `(` and the opening quote both parse (valid Rust).
    /// A call whose first argument is not a string literal is skipped - the
    /// kind is dynamic and not statically checkable. The line number (1-based)
    /// is reported so a drift failure points straight at the offending call.
    fn scan_emit_kinds(src: &str) -> Vec<(String, usize)> {
        // Comment lines are blanked (offsets preserved) first: a doc comment
        // may SHOW an emit shape (`.emit("...")` in prose) and the scan reads
        // bytes, not syntax.
        let bytes = src.as_bytes();
        let mut blanked = bytes.to_vec();
        let mut i = 0usize;
        while i < bytes.len() {
            let line_end = bytes[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map(|p| i + p)
                .unwrap_or(bytes.len());
            let first = bytes[i..line_end]
                .iter()
                .find(|&&c| c != b' ' && c != b'\t');
            if first == Some(&b'/') {
                for b in &mut blanked[i..line_end] {
                    *b = b' ';
                }
            }
            i = line_end + 1;
        }
        let src = std::str::from_utf8(&blanked).unwrap_or(src);
        let bytes = src.as_bytes();
        let mut kinds = Vec::new();
        for needle in [".emit", ".emit_fields"] {
            let nb = needle.as_bytes();
            let mut from = 0usize;
            while let Some(rel) = find_sub(&bytes[from..], nb) {
                let pos = from + rel;
                let mut j = pos + nb.len();
                // `.emit` must not match inside `.emit_fields` (next char `_`).
                if needle == ".emit" && j < bytes.len() && bytes[j] == b'_' {
                    from = j;
                    continue;
                }
                while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'(' {
                    j += 1;
                    while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b'"' {
                        let start = j + 1;
                        let mut k = start;
                        while k < bytes.len() && bytes[k] != b'"' {
                            k += 1;
                        }
                        if k < bytes.len() {
                            let kind = String::from_utf8_lossy(&bytes[start..k]).into_owned();
                            let line = src[..pos].bytes().filter(|&c| c == b'\n').count() + 1;
                            kinds.push((kind, line));
                        }
                    }
                }
                from = pos + nb.len();
            }
        }
        kinds
    }

    // The exec-stub guard: a lib test that writes an executable stub
    // in-process holds a write fd, and a sibling test thread's fork copies it
    // into a child; an exec of the stub then fails with ETXTBSY until that
    // child reaches its own exec. The one writer is `write_exec_stub`, whose
    // child /bin/sh exits before returning. ponytail: the regex matches only
    // the literal-mode idiom; a mode built in a variable not named `mode`
    // evades it.
    #[test]
    fn every_exec_stub_goes_through_write_exec_stub() {
        let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs_files(&src_root, &mut files);
        assert!(!files.is_empty(), "found no .rs files under {src_root:?}");

        // Byte-level scan, like the emit-kind guard: no regex crate here.
        // Matches the plan regex `(from_mode|set_mode|\.mode)\((0o7[0-7]{2}|mode)\)`
        // per line: after the needle, trimmed whitespace, either a 0o7xx octal
        // literal closed by `)`, or the variable form `mode)`.
        const NEEDLES: [&str; 3] = ["from_mode(", "set_mode(", ".mode("];
        let mut offenders: Vec<String> = Vec::new();
        for file in &files {
            let text = std::fs::read_to_string(file).expect("read source file");
            let lines: Vec<&str> = text.lines().collect();
            for (idx, line) in lines.iter().enumerate() {
                let mut hit = false;
                for needle in NEEDLES {
                    let mut from = 0;
                    while let Some(pos) = line[from..].find(needle) {
                        let rest = line[from + pos + needle.len()..].trim_start();
                        let octal = rest.as_bytes();
                        let matched = (octal.len() >= 6
                            && octal[..3] == *b"0o7"
                            && (b'0'..=b'7').contains(&octal[3])
                            && (b'0'..=b'7').contains(&octal[4])
                            && octal[5] == b')')
                            || rest.starts_with("mode)");
                        if matched {
                            hit = true;
                            break;
                        }
                        from += pos + needle.len();
                    }
                    if hit {
                        break;
                    }
                }
                if hit {
                    let name = file.strip_prefix(&src_root).unwrap_or(file);
                    offenders.push(format!("{}:{}", name.display(), idx + 1));
                }
            }
        }

        // The five allowed files: production binary repair (install_verify),
        // a production dir mode (paths), two dir-mode restores in tests
        // (claims, operator_turns), and the bin test target that cannot see a
        // cfg(test) lib fn (client_tests).
        const ALLOWED: &[(&str, usize)] = &[
            ("install_verify.rs", 1),
            ("paths.rs", 1),
            ("king_board/claims.rs", 1),
            ("operator_turns.rs", 1),
            ("client_tests.rs", 2),
        ];
        let allowed_counts: std::collections::HashMap<&str, usize> =
            ALLOWED.iter().copied().collect();
        let mut by_file: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for o in &offenders {
            let file = o.rsplit_once(':').map(|(f, _)| f.to_string()).unwrap();
            *by_file.entry(file).or_insert(0) += 1;
        }
        for (file, count) in &by_file {
            let allowed = allowed_counts.get(file.as_str()).copied().unwrap_or(0);
            assert!(
                *count <= allowed,
                "this test writes an executable stub from inside the test \
                 process, where a sibling test's fork can hold the write fd \
                 open and the exec fails with Text file busy. Write it with \
                 crate::write_exec_stub. Offenders: {offenders:?}"
            );
        }
        for (file, allowed) in ALLOWED {
            let actual = by_file.get(*file).copied().unwrap_or(0);
            assert!(
                actual <= *allowed,
                "{file} now matches {actual} times (allowance {allowed}); \
                 the extra site must go through crate::write_exec_stub. {offenders:?}"
            );
        }
    }

    fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || haystack.len() < needle.len() {
            return None;
        }
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    // AC1-HP / AC1-EDGE: the stub writer never holds the write fd in this
    // process, so execs of its output survive sibling threads forking in a
    // loop (darwin has no /bin/true, so the forking threads use
    // /usr/bin/true). Each stub prints its own name and the exec asserts it.
    #[test]
    fn write_exec_stub_survives_sibling_forks() {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = tempfile::tempdir().expect("tempdir");
        let stop = Arc::new(AtomicBool::new(false));
        let mut forkers = Vec::new();
        for _ in 0..4 {
            let stop = Arc::clone(&stop);
            forkers.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = std::process::Command::new("/usr/bin/true").status();
                }
            }));
        }
        let mut writers = Vec::new();
        for t in 0..4u32 {
            let dir = dir.path().to_path_buf();
            writers.push(std::thread::spawn(move || {
                for i in 0..25u32 {
                    let body = format!("#!/bin/sh\nprintf '%s' '{t}-{i}'\n");
                    let stub = crate::write_exec_stub(&dir, &format!("s{t}-{i}"), &body);
                    let out = std::process::Command::new(&stub)
                        .output()
                        .expect("exec stub");
                    assert!(
                        out.status.success(),
                        "exec of {} failed: {out:?}",
                        stub.display()
                    );
                    let mut text = String::new();
                    std::io::Cursor::new(&out.stdout)
                        .read_to_string(&mut text)
                        .expect("stdout is utf-8");
                    assert_eq!(text, format!("{t}-{i}"));
                }
            }));
        }
        for w in writers {
            w.join().expect("writer thread");
        }
        stop.store(true, Ordering::Relaxed);
        for f in forkers {
            f.join().expect("forker thread");
        }
        let mode = std::fs::metadata(dir.path().join("s0-0"))
            .expect("stub exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755, "stub mode must be 0755");
    }

    // AC1-ERR: a missing destination dir surfaces as the writer's exit
    // status, naming the path.
    #[test]
    #[should_panic(expected = "could not write exec stub")]
    fn write_exec_stub_refuses_a_missing_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _ = crate::write_exec_stub(&dir.path().join("absent"), "fno", "#!/bin/sh\n");
    }
}

// ---------------------------------------------------------------------------
// W7: Cross-language schema introspection
// ---------------------------------------------------------------------------

/// All real operator-facing event kinds emitted by the Rust supervisor.
/// Excludes test-only kinds (tick, heartbeat).
///
/// This const is the authoritative list for `--emit-schema` output and must
/// stay in sync with every `.emit(kind, ...)` / `.emit_fields(kind, ...)`
/// call site in the crate. The parity check script compares this list against
/// the Python side for global uniqueness.
///
/// **How to regenerate when adding a new event kind:**
/// ```text
/// grep -rn '\.emit\b\|\.emit_fields\b' crates/fno-agents/src/ \
///   | grep -v '//' \
///   | grep -oP '"[a-z_]+"' \
///   | sort | uniq
/// ```
/// Then cross-check the output against this list. Test-only kinds (tick,
/// heartbeat) and value fields (reason, backend, ...) will appear in the grep
/// output; only include kinds that appear as the first string argument to an
/// emit call in non-test production code.
pub const KNOWN_EVENT_KINDS: &[&str] = &[
    // The question intake's journal write (the ask port): the durable half
    // of `fno inbox outstanding ask`.
    "operator_question",
    // Agent lifecycle (daemon-emitted)
    "agent_spawned",
    // Spawn coordinator: the durable accepted record written BEFORE
    // any backend launch; not a birth, correlated to it by spawn_id.
    "agent_spawn_accepted",
    // The keeper's render trigger failed a pass (waves 8-9 store cutover);
    // carries the version and a stderr tail, and the backoff retries it.
    "graph_render_failed",
    // Pane-to-thread conversion: one per phase of the agent.convert
    // transaction (classified, claims-held, pane-stopped, hand-off,
    // resumed, flipped, rolled-back), carrying the name and strategy.
    "agent_convert_phase",
    "agent_stopped",
    // Stop/rm claims release: the receipt event for the claims a
    // stopped or removed worker held; one emit per stop/rm that ran one.
    "agent_stop_claims_released",
    // A stop the daemon REFUSED to claim: the interrupt never confirmed a
    // terminal turn, so the row stays live and the work is still running.
    "agent_stop_refused",
    "agent_exited",
    "agent_removed",
    // Served facts (daemon-emitted): the sweep is the only writer of the
    // registry's measured surfaces, so each of these announces a change that
    // a reader would otherwise learn from a snapshot field. `agent_renamed`
    // is the harness's title moving against the row's last-seen baseline
    // (the label is never rewritten); `agent_model_changed` /
    // `agent_effort_changed` are PostModelSwitch axis changes verified by
    // the report write; `session_aliases_merged` is the session-names
    // overlay folding legible aliases onto rows.
    "agent_renamed",
    "agent_model_changed",
    "agent_effort_changed",
    "session_aliases_merged",
    "merge_cleanup_requested",
    "merge_cleanup_skipped",
    "merge_cleanup_completed",
    "merge_cleanup_refused",
    // Merge reaper (daemon-emitted): a pending request was HELD (the
    // node reads open, the list is empty, or the graph would not read) and is
    // retried next pass; a request aged past its expiry window and is
    // tombstoned; a row's harness was stopped ahead of its registry removal.
    "merge_cleanup_held",
    "merge_cleanup_expired",
    "merge_reaper_stopped",
    "agent_inconsistent",
    "agent_ask_done",
    "agent_create_no_session",
    "agent_orphan_reaped",
    "agent_orphan_state_archived",
    // Orphaned-test-binary reap sweep (daemon-emitted): one event per pid
    // the footprint verb killed on the daemon's behalf.
    "orphan_test_binary_reaped",
    // Late bind (daemon-emitted, task 2): a pane-hosted codex row whose
    // spawn-time bind window expired got its `harness_session_id` resolved on
    // a later reconcile tick, from the pane-tree rollout probe. Makes "the row
    // bound 40 seconds after spawn" visible instead of inferred.
    "agent_late_bind",
    // Late-bind write failure (daemon-emitted, follow-up): the registry
    // write that would have bound `harness_session_id` failed, most often a
    // collision with a session id already claimed by another row. Distinct
    // from a probe miss (no event, just skipped) and from `agent_late_bind`
    // (the write succeeded).
    "agent_late_bind_failed",
    // Dead-row GC (daemon/reap-verb-emitted): a terminal, past-grace,
    // clean agent-view row was removed from the registry by the GC sweep or
    // `fno agents reap`. Distinct from `agent_orphan_reaped` (which flips a
    // live-but-unowned PID to exited); this REMOVES the row entirely.
    "agent_row_reaped",
    // One row per daemon retire tick that held rows: every held id
    // with its reason, detail and age, so the fleet reads one event stream.
    "retire_holds",
    // One bounded count summary for every configured state-retention pass.
    "state_reap",
    "graph_write_gate",
    // Choke-point removal accounting: ANY write path that drops a
    // registry row emits one of these, receipt staged first. Distinct from
    // `agent_row_reaped` (the GC door's own event); this fires for every
    // door, including ones nobody has enumerated yet.
    "registry_row_removed",
    // One lossy save, grouped: the writer, pid, verb, and every
    // lost id in one event, beside the per-row receipts above, so a save
    // that drops rows can never vanish without a door being named.
    "registry_rows_lost",
    // Orphan process sweep (daemon-emitted): the row GC beside it reaps
    // registry ROWS, this reaps the `fno-py` children that init inherited and
    // nobody was waiting on. Emitted on EVERY run including the ones that reap
    // nothing, because a reaper that speaks only when it kills cannot be told
    // apart from a reaper that never ran.
    "orphan_reap_sweep",
    // Worktree report sweep (daemon-emitted): one line per repo per 24h
    // saying what `fno agents workspace worktree cleanup --merged` WOULD archive. Report-only by
    // construction, because a timer tick is not proof that work landed; removal
    // stays on the merge-triggered path. Emitted even when the counts are zero,
    // so a quiet repo cannot be mistaken for a sweep that never ran.
    "worktree_sweep",
    // Stale-question reconcile (daemon-emitted): `fno agents stale-escalate`
    // ran on its 6h floor and reconciled the lane's fleet tasks (the former
    // [watchdog-stale:*] operator question) to the measured fleet through
    // the Rust fleet-task door. Report-only: a king reads open tasks on the
    // board's fleet_task queue.
    // Emitted even on outcome none/duplicate, so a quiet run cannot be
    // mistaken for a sweep that never ran.
    "stale_sweep",
    // Question sweep (daemon-emitted): `fno agents question-sweep` ran on
    // its interval floor and closed the node-closed questions it found,
    // appending one empty-answer row per question (a non-empty answer would
    // arm the unrecorded-decision gate against the asking session). Emitted
    // even on outcome none, so a quiet run cannot be mistaken for a sweep
    // that never ran.
    "question_sweep",
    // Park sweep (daemon-emitted): `fno-agents pr-park sweep` ran on its 6h
    // floor and un-parked open rows whose head moved or whose park passed
    // 24h, marking finished rows handled. Emitted even on a quiet or skipped
    // run, so a quiet run cannot be mistaken for a sweep that never ran.
    "park_sweep",
    // A parked PR resumed polling (pr-park-emitted): retries reset, by hand
    // (the king row's verb) or by the sweep.
    "pr_watch_unparked",
    // Dead-row GC also reconstructs the loop's canonical failure event when a
    // convention-named dispatch disappeared without a termination receipt.
    "node_failed",
    // The merge reaper emits the same kind the cleanup verb does when
    // it takes a merged node's tree, so one removal, one event, wherever the
    // caller lives.
    "worktree_removed",
    // Terminal-stop sweep (daemon-emitted): a fire-and-forget
    // `claude --bg` worker that finalize marked terminal was `claude stop`ped so
    // its slot frees instead of parking at an idle prompt forever.
    "bg_worker_terminal_stopped",
    // Nudge ladder (daemon-emitted): a session the retirement sweep keeps on
    // an open PR is mailed or resumed to drive the PR to merge; a stuck one
    // escalates to the operator; a recorded merge order pauses the ladder.
    "pr_nudge_sent",
    "pr_nudge_escalated",
    "pr_nudge_paused",
    "agent_spawn_failed",
    // A codex thread was auto-resumed with no reconstructible state-root grant
    //. The roots reach a spawn as an RPC param from the Python seam,
    // and daemon-side recovery has no such param, so that worker may be unable
    // to claim or mail. Emitted so the loss is readable instead of silent.
    "codex_thread_resumed_without_state_grant",
    "agent_stop_error",
    "agent_spawn_cwd_fallback",
    // Claude stream-json adoption front door (daemon-emitted):
    // advisory note that the single-writer claim substrate could not be consulted
    // before spawning, so the adopt proceeded fail-open (the registry one-host
    // re-check is the authoritative guard).
    "agent_stream_claim_unavailable",
    // Channel (daemon-emitted)
    "channel_registered",
    // Daemon lifecycle (daemon-emitted)
    "daemon_started",
    "daemon_exited",
    "daemon_idle_pending_exit",
    // Drift retirement (daemon-emitted): the daemon measured its own
    // build drifted and, at a quiet tick with no live worker, retired through
    // the graceful tail so the next lazy start runs the installed binary.
    "daemon_drift_pending_exit",
    "daemon_shutting_down",
    // The socket path stopped resolving to the inode this daemon bound, so
    // something else now owns it and this process is unreachable.
    // It retires rather than keep running as an invisible CPU burner.
    "daemon_socket_lost",
    "daemon_state",
    "daemon_recovery_error",
    "daemon_recovery_interrupted_temp",
    // Binary-version drift (daemon-emitted, plan): advisory note that
    // the daemon could not fingerprint its own executable at startup, so every
    // client drift check fails safe to Unknown.
    "daemon_exe_fingerprint_unavailable",
    // Drive (daemon-emitted)
    "drive_attached",
    "drive_detached",
    "drive_crashed",
    "drive_force_close_timeout",
    "drive_keystroke_stepped",
    "drive_refused_busy_elsewhere",
    "drive_takeover_after_stale",
    "drive_watch_input_rejected",
    // Reconcile (daemon-emitted)
    "reconcile_deferred",
    "reconcile_done",
    "reconcile_error",
    // Reign (king-emitted): the tenured-king skill journals these from
    // the reigning session; audit resolves the names through this table.
    "reign_armed",
    "reign_checkin",
    "reign_dispatch_exception",
    // A crown's term declared or extended (`fno agents king term <spec>
    // [--reason]`), before or after a Stop-hook gate observed it reached.
    // The receipt a reign's tenure bound leaves; `fno doctor event audit`
    // resolves it through this table exactly like the reign kinds above.
    "king_term",
    // A crown whose holder session is proven dead left its territory: the
    // dead-crown sweep journals the vacate with cause holder_dead, the
    // death evidence, and the inheritor (crown_reap.rs; the daemon retire
    // arm and `fno agents reap`). Python's attended `king done` emits the
    // same kind through the shared emitter.
    "agent_crown_vacated",
    // Startup reconcile sweep (daemon-emitted, plan Architecture B)
    "startup_reconcile_done",
    "startup_reconcile_failed",
    // Registry-side keeper sweep (daemon-emitted): the daemon-start
    // walk of the lane-B keeper thread sockets. Every dead or wedged verdict
    // carries its reason; the rebound/dead/wedged row events name the row.
    "keeper_sweep_done",
    "keeper_sweep_failed",
    "keeper_sweep_budget_exhausted",
    "keeper_socket_unlinked",
    "keeper_socket_silent_no_row",
    "keeper_socket_orphan",
    "keeper_row_dead",
    "keeper_row_wedged",
    "keeper_row_rebound",
    "keeper_row_superseded",
    "keeper_row_terminal_socket_live",
    // Store-keeper socket hygiene (daemon-start sweep): dead store sockets in
    // the state root and the hashed temp root are unlinked, as are orphaned
    // seat locks; live listeners are left as found.
    "store_socket_unlinked",
    "store_seat_lock_unlinked",
    // Deliver (daemon-emitted, Task 2.2 US4)
    "agent_deliver_injected",
    "agent_deliver_demoted",
    "agent_deliver_status_write_failed",
    // Unwrapped-injection audit (mail-inject binary + mux pane): an
    // agent_raw_inject records a payload delivered without the <fno_mail>
    // envelope, so the provenance marker survives in the ledger, not transcript.
    "agent_raw_inject",
    // Review invocation attempt/outcome join (daemon-emitted): the
    // canonical repo-local event records how a Codex review was fired and
    // whether its transport confirmed delivery.
    "review_invocation",
    // Active-backlog mission drain supervisor (daemon-emitted): the drain tick
    // panicked and the supervisor is restarting it with backoff. The drain
    // decision events (active_backlog_dispatched / _parked / _skip) are
    // loop-stream events via Journal::append, NOT daemon emits, so they are
    // exempt from this registry by design.
    "active_backlog_task_crashed",
    // A mission drain loop retired (its epic deactivated / all children done,
    // K2). An EventEmitter emit, so a first-class registered kind.
    "active_backlog_mission_retired",
    // Harness-aware dispatch guard (walker-emitted): the shared node
    // chokepoint deferred a node to a foreign harness that owns / is working it
    // (a foreign-tagged claim, a codex/gemini branch, or a foreign worktree)
    // instead of default-spawning a claude worker. Unlike the journal-based
    // active_backlog decision events above, this is an EventEmitter emit, so it
    // is a first-class registered kind.
    "dispatch_deferred",
    // Control-plane arms readout: the supervisor-level
    // active_backlog tick row is an EventEmitter emit (the mission-level rows
    // ride Journal::append and are exempt like the drain decision events).
    "control_plane_tick",
    // Evals demand (Python-emitted from the pr-watch tick's evals
    // leg): the scheduled regression-tier run's outcome, and the could-not-
    // fire row whose journal entries are the operator-notice rate bound.
    "evals_scheduled_run",
    "evals_stale",
    // Scratch-shape sweep (agents-emitted from the `scratch sweep`
    // stage of the daily eval-sweep ignition): one row per new (job, shape)
    // recurrence the jobs-dir walker found, and one row per node the sweep
    // filed, folded, or seeded for a shape. The journal is the sweep's own
    // dedupe index: a pair already observed in the window never re-emits.
    "scratch_shape_observed",
    "scratch_shape_filed",
    // Node-closed question sweep (daemon-emitted): the periodic walk that
    // auto-closes open operator questions whose node has closed; one emit
    // per pass names how many it closed and, when any, which.
    "question_sweep",
    // Meta (daemon/worker-emitted)
    "event_payload_too_large",
    // Inside-leg state push (daemon-emitted, inside-out E3.2): a per-turn hook
    // stored its latest {working|blocked|done} on the matching claude row, or the
    // daemon dropped a report (stale seq / unknown session) without storing it.
    "inside_leg_report",
    "inside_leg_report_dropped",
    // Ordered exit teardown (daemon-emitted, inside-out E3.3): a claude row with
    // an inside-leg report is going Exited; the completion is published before
    // the registry clears the report (AC-X2-4).
    "inside_leg_completed",
    // Buffer-on-early-push (daemon-emitted, inside-out E3.3): a report arrived
    // before its session's row existed and was held in the pending buffer, then
    // flushed onto the row at creation.
    "inside_leg_report_buffered",
    "inside_leg_buffer_flushed",
    // Driver-sourced thread-row status: the codex thread actor's
    // turn phases land on the row's inside_leg through the shared seq gate;
    // one event per accepted write.
    "codex_thread_inside_leg",
    // Screen-manifest fallback rung (daemon-emitted, scrape sweep): a scraped
    // verdict was stored/refreshed/cleared on a hook-less mux row, or a
    // provider's manifest failed to load.
    "screen_state_change",
    // CI heal drive loop (pr-heal verb): one row per --all --apply
    // invocation carrying the per-tick counts, so the arm is visible in the
    // journal even on a quiet cycle. Emitted even when every PR is skipped,
    // for the same reason worktree_sweep is: a quiet repo must not read as a
    // loop that never ran. The Python tick emits nothing for this family.
    "pr_heal_tick",
    // The heal loop's acted-on rows: one flake guard staged per flapping
    // (sha, run id, check), and one remedy row per PR the loop acted on.
    "pr_heal_flake",
    "pr_heal_pr",
    // Daemon startup scope declaration: sandbox home vs the operator's
    // shared home. Fleet gating and board routing key off it.
    "daemon_fleet_scope",
    // NOTE: the a2a status-breakpoint kinds (task_started/task_done/blocked/
    // run_summary) are NOT registered here. They are Python-defined in
    // cli/src/fno/events/schema.yaml; the parity gate partitions names (a kind
    // in both the Python schema and this Rust registry is a COLLISION). finalize
    // emits run_summary via a custom envelope writer (not the registered
    // `.emit()` path), so the production-emit-kind guard does not require it.
];

/// Build the unified events.jsonl envelope JSON Schema and the
/// `status-v1` AgentState schema as static JSON objects.
///
/// This mirrors `schemas/events-v3.json` (single envelope) and
/// `schemas/status-v1.json`. The hand-rolled approach is
/// chosen to avoid pulling in `schemars`; it MUST be accompanied by the
/// struct-drift unit test in `src/bin/client.rs` that asserts every
/// `AgentState` field key is present in the emitted status schema properties.
///
/// Returns a JSON object suitable for printing via `--emit-schema`:
/// ```json
/// {
///   "envelope": { <unified events-v3 schema> },
///   "status": { <status-v1 schema> },
///   "event_kinds": ["agent_spawned", ...]
/// }
/// ```
pub fn emit_schema_json() -> serde_json::Value {
    use serde_json::json;
    json!({
        "envelope": {
            "$comment": "Unified events.jsonl envelope. Emitted by crates/fno-agents/src/events.rs; structurally equal to schemas/events-v3.json after doc-key stripping (the parity gate diffs them).",
            "type": "object",
            "required": ["ts", "type", "source", "data"],
            "properties": {
                "ts": {
                    "type": "string",
                    "description": "UTC RFC3339 timestamp with millisecond precision and Z suffix"
                },
                "type": {
                    "type": "string",
                    "description": "Event type name; the daemon kinds live in KNOWN_EVENT_KINDS (see event_kinds below)"
                },
                "source": {
                    "type": "string",
                    "anyOf": [
                        { "enum": ["active-backlog", "agents", "approvals", "backlog", "bash", "cli", "config", "daemon", "fno-loop", "hook", "loop", "megatron", "megawalk", "migration", "observer", "pr-heal", "pr-park", "python", "skill_diff", "subagent", "target", "test"] },
                        { "pattern": "^(worker|stream-worker):.+$" }
                    ],
                    "description": "Producer identity: a fixed-string source or a per-agent worker (worker:<id> / stream-worker:<id>)"
                },
                "data": {
                    "type": "object",
                    "description": "Per-type payload object"
                }
            },
            "additionalProperties": true
        },
        "status": {
            "$comment": "AgentState schema v1. Derived from crates/fno-agents/src/state.rs AgentState struct.",
            "type": "object",
            "required": ["schema_version", "short_id", "status"],
            "properties": {
                "schema_version": {
                    "type": "integer",
                    "const": 1
                },
                "short_id": {
                    "type": "string"
                },
                "status": {
                    "type": "string",
                    "enum": [
                        "spawning", "ready", "idle", "busy", "live",
                        "restarting", "orphaned", "failed", "exited", "permanent_dead"
                    ]
                },
                "ready": {
                    "type": "boolean",
                    "default": false
                },
                "last_message_at": {
                    "type": ["string", "null"]
                },
                "last_reply": {
                    "type": ["string", "null"]
                },
                "restart_count": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0
                },
                "last_restart_at": {
                    "type": ["string", "null"]
                },
                "pty": {
                    "oneOf": [
                        { "type": "null" },
                        {
                            "type": "object",
                            "required": ["active", "drive_active"],
                            "properties": {
                                "active": { "type": "boolean" },
                                "drive_active": { "type": "boolean", "default": false },
                                "drive_session_id": { "type": ["string", "null"] },
                                "drive_mode": { "type": ["string", "null"] },
                                "last_heartbeat_at_monotonic_ns": { "type": ["integer", "null"] }
                            },
                            "additionalProperties": false,
                            "if": {
                                "properties": { "drive_active": { "const": true } },
                                "required": ["drive_active"]
                            },
                            "then": {
                                "required": ["drive_session_id", "drive_mode"],
                                "properties": {
                                    "drive_session_id": { "type": "string" },
                                    "drive_mode": { "type": "string" }
                                }
                            }
                        }
                    ]
                }
            },
            "additionalProperties": false
        },
        "event_kinds": KNOWN_EVENT_KINDS,
        // Generated from cli/src/fno/events/schema.yaml's limits block by
        // build.rs. Emitting it makes the value the compiled binary
        // actually carries observable, so a regenerated file the code never
        // reads cannot pass for a fix. The parity gate diffs only `envelope`,
        // `status` and `event_kinds`, so this sibling key perturbs nothing.
        "limits": {
            "max_data_bytes": events_limits::max_data_bytes(),
            "data_size_encoding": events_limits::data_size_encoding()
        }
    })
}

/// A file's tail, at most `cap` bytes, starting on a line boundary: a seek
/// into the middle of a line drops that partial line, so every admitted row
/// is whole. Empty on any read failure, never a guess. The one tail walk:
/// `tail_text` lossy-repairs it for observational readers, and a guard that
/// must fail closed takes [`tail_text_strict`] instead.
pub(crate) fn tail_bytes(path: &std::path::Path, cap: u64) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return Vec::new(),
    };
    let start = len.saturating_sub(cap);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    if start > 0 {
        match buf.iter().position(|&b| b == b'\n') {
            // No whole line inside the window: nothing to admit.
            Some(p) => buf.split_off(p + 1),
            None => Vec::new(),
        }
    } else {
        buf
    }
}

/// [`tail_bytes`] as lossy text: observational readers never fail on bytes.
pub(crate) fn tail_text(path: &std::path::Path, cap: u64) -> String {
    String::from_utf8_lossy(&tail_bytes(path, cap)).into_owned()
}

/// [`tail_bytes`] as text, None when the tail is not valid UTF-8: a guard
/// that answers with an allow reads corrupt evidence as unreadable, never
/// as a repaired guess.
pub(crate) fn tail_text_strict(path: &std::path::Path, cap: u64) -> Option<String> {
    String::from_utf8(tail_bytes(path, cap)).ok()
}

/// Host boot as epoch ms. Linux reuses `claims::linux_boot_time_s` (the
/// cached `/proc/stat` btime); macOS reads sysctl `kern.boottime`. None on
/// any failure, never "now": a caller that cannot know the boot must drop
/// the boot clause, not invent one.
#[cfg(target_os = "linux")]
pub fn host_boot_epoch_ms() -> Option<i64> {
    crate::claims::linux_boot_time_s().and_then(|s| boot_ms_from(s, 0))
}

/// The macOS leg: sysctl `kern.boottime` into a zeroed timeval, the same
/// call shape `census.rs` uses for its CTL_KERN probes. Cached like the
/// Linux btime: constant for the life of the host, read once per process.
#[cfg(target_os = "macos")]
pub fn host_boot_epoch_ms() -> Option<i64> {
    static BOOT: std::sync::OnceLock<Option<i64>> = std::sync::OnceLock::new();
    *BOOT.get_or_init(|| {
        let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
        let mut tv: libc::timeval = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<libc::timeval>();
        // SAFETY: sysctl fills a caller-owned zeroed buffer; mib and size live
        // in this frame and are read only during the call.
        let done = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                &mut tv as *mut _ as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if done != 0 {
            return None;
        }
        boot_ms_from(tv.tv_sec as i64, tv.tv_usec as i64)
    })
}

/// Platforms with neither reader read absent, like every other bound.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn host_boot_epoch_ms() -> Option<i64> {
    None
}

/// Boot epoch ms from a boot-time second count. A non-positive count is a
/// failed reading, not "the host booted at the epoch".
fn boot_ms_from(sec: i64, usec: i64) -> Option<i64> {
    if sec <= 0 {
        return None;
    }
    Some(sec * 1000 + usec / 1000)
}
