//! `fno-agents` substrate crate (Phase 6, ab-a09e1eaf).
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

pub mod active_backlog;
pub mod agents_config;
pub mod agy_ask;
pub mod claims;
pub mod claude_adopt;
pub mod claude_ask;
pub mod claude_attach;
pub mod claude_drive;
pub mod claude_roster;
pub mod client;
pub mod client_verbs;
pub mod codex_ask;
pub mod codex_inject;
pub mod daemon;
pub mod digest;
pub mod dispatch_posture;
pub mod drift;
pub mod envelope;
pub mod events;
pub mod finalize;
pub mod gc;
pub mod gemini_ask;
pub mod kill_criteria;
pub mod logs;
pub mod logs_client;
pub mod loop_dispatch;
pub mod loop_megawalk;
pub mod loop_runtime;
pub mod loop_target;
pub mod loopcheck;
pub mod mail_inject;
pub mod manifest;
pub mod nudge;
pub mod opencode_ask;
pub mod osc;
pub mod paths;
pub mod protocol;
pub mod provider;
pub mod readiness;
pub mod scrape;
pub mod screen;
pub mod spawn_gate;
pub mod state;
pub mod stream_worker;
pub mod subprocess_ask;
pub mod subscribe;
pub mod supervisor;
pub mod terminal_stop;
pub mod verify_evidence;
pub mod version;
pub mod wait;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// PTY spawned, not yet confirmed ready for input.
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

#[cfg(test)]
mod tests {
    use super::*;

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
        const TEST_ONLY_EMIT_KINDS: &[&str] = &["tick", "heartbeat", "foo", "x"];
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

    fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.is_empty() || haystack.len() < needle.len() {
            return None;
        }
        haystack.windows(needle.len()).position(|w| w == needle)
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
    // Agent lifecycle (daemon-emitted)
    "agent_spawned",
    "agent_stopped",
    "agent_exited",
    "agent_removed",
    "agent_inconsistent",
    "agent_ask_done",
    "agent_create_no_session",
    "agent_orphan_reaped",
    "agent_orphan_state_archived",
    // Dead-row GC (daemon/reap-verb-emitted, x-b1aa): a terminal, past-grace,
    // clean agent-view row was removed from the registry by the GC sweep or
    // `fno agents reap`. Distinct from `agent_orphan_reaped` (which flips a
    // live-but-unowned PID to exited); this REMOVES the row entirely.
    "agent_row_reaped",
    // Terminal-stop sweep (daemon-emitted, x-fcbf): a fire-and-forget
    // `claude --bg` worker that finalize marked terminal was `claude stop`ped so
    // its slot frees instead of parking at an idle prompt forever.
    "bg_worker_terminal_stopped",
    "agent_spawn_failed",
    "agent_stop_error",
    "agent_spawn_cwd_fallback",
    // Claude stream-json adoption front door (daemon-emitted, ab-734fcd6c):
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
    "daemon_shutting_down",
    "daemon_state",
    "daemon_recovery_error",
    // Binary-version drift (daemon-emitted, plan ab-1891cdff): advisory note that
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
    // Startup reconcile sweep (daemon-emitted, plan ab-70faa65b Architecture B)
    "startup_reconcile_done",
    "startup_reconcile_failed",
    // Deliver (daemon-emitted, Task 2.2 US4)
    "agent_deliver_injected",
    "agent_deliver_demoted",
    // Active-backlog drain supervisor (daemon-emitted, node x-c070): the drain
    // tick panicked and the supervisor is restarting it with backoff. The drain
    // decision events (active_backlog_dispatched / _yield / _parked / _skip) are
    // loop-stream events via Journal::append, NOT daemon emits, so they are
    // exempt from this registry by design.
    "active_backlog_task_crashed",
    // Harness-aware dispatch guard (walker-emitted, x-3e70): the shared node
    // chokepoint deferred a node to a foreign harness that owns / is working it
    // (a foreign-tagged claim, a codex/gemini branch, or a foreign worktree)
    // instead of default-spawning a claude worker. Unlike the journal-based
    // active_backlog decision events above, this is an EventEmitter emit, so it
    // is a first-class registered kind.
    "dispatch_deferred",
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
    // Screen-manifest fallback rung (daemon-emitted, scrape sweep): a scraped
    // verdict was stored/refreshed/cleared on a hook-less mux row, or a
    // provider's manifest failed to load.
    "screen_state_change",
    // NOTE: the a2a status-breakpoint kinds (task_started/task_done/blocked/
    // run_summary, x-dbaf) are NOT registered here. They are Python-defined in
    // cli/src/fno/events/schema.yaml; the parity gate partitions names (a kind
    // in both the Python schema and this Rust registry is a COLLISION). finalize
    // emits run_summary via a custom envelope writer (not the registered
    // `.emit()` path), so the production-emit-kind guard does not require it.
];

/// Build the unified (x-2901) events.jsonl envelope JSON Schema and the
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
///   "envelope": { <Branch B schema> },
///   "status": { <status-v1 schema> },
///   "event_kinds": ["agent_spawned", ...]
/// }
/// ```
pub fn emit_schema_json() -> serde_json::Value {
    use serde_json::json;
    json!({
        "envelope": {
            "$comment": "Unified events.jsonl envelope (x-2901). Emitted by crates/fno-agents/src/events.rs; structurally equal to schemas/events-v3.json after doc-key stripping (the parity gate diffs them).",
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
                        { "enum": ["abi-loop", "active-backlog", "backlog", "daemon", "hook", "megatron", "megawalk", "migration", "observer", "skill_diff", "subagent", "target", "test"] },
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
        "event_kinds": KNOWN_EVENT_KINDS
    })
}
