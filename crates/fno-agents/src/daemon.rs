//! The supervisor daemon (Wave 3, tasks 3.0 + 3.4).
//!
//! One long-running per-user process. Lazy-started by the client on first need;
//! lazy-exits after an idle window. Six observable states (each emits an event
//! on entry), a startup recovery procedure that must complete before the socket
//! serves requests, and a JSON-RPC serve loop routing `agent.*` / `channel.*`.
//!
//! Wave 3 lands the daemon skeleton, IPC transport, worker spawn/ask routing,
//! and the correctness-critical recovery procedure. The drive WebSocket surface
//! is Wave 4; the full lifecycle-verb polish is Wave 5; Python integration is
//! Wave 6. The handlers here are deliberately the minimum that makes the daemon
//! a working supervisor end-to-end.

use crate::events::EventEmitter;
use crate::paths::{self, AgentsHome};
use crate::protocol::{
    read_request, write_request, write_response, ErrorCode, Namespace, Request, Response,
};
use crate::state::{self, RegistryEntry};
use crate::AgentStatus;
use serde_json::{json, Map, Value};
use std::os::unix::process::CommandExt; // process_group on std::process::Command
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{UnixListener, UnixStream};

/// Six observable daemon states (design "Daemon lifecycle" table). Each entry
/// emits an event so events.jsonl reflects the lifecycle for an auditor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    ColdStart,
    Recovering,
    Serving,
    IdlePendingExit,
    ShuttingDown,
    Exited,
}

impl DaemonState {
    pub fn as_str(&self) -> &'static str {
        match self {
            DaemonState::ColdStart => "cold_start",
            DaemonState::Recovering => "recovering",
            DaemonState::Serving => "serving",
            DaemonState::IdlePendingExit => "idle_pending_exit",
            DaemonState::ShuttingDown => "shutting_down",
            DaemonState::Exited => "exited",
        }
    }
}

/// Daemon tunables. Defaults match the design (30 min idle exit).
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub idle_exit: Duration,
    /// Path to the `fno-agents-worker` binary. Resolved from the daemon's own
    /// executable directory by default; overridable via `FNO_AGENTS_WORKER_BIN`
    /// (tests point this at the cargo-built binary).
    pub worker_bin: PathBuf,
    /// Run one bounded reconcile sweep on daemon startup before serving any
    /// client (Architecture B, plan ab-70faa65b). Default `true`; the opt-out
    /// (env `FNO_AGENTS_NO_STARTUP_RECONCILE=1`, Claude's discretion #5) trades a
    /// truthful first `list` for the fastest possible cold start.
    pub reconcile_on_start: bool,
    /// Grace window before the dead-row GC reaps a finished agent-view row
    /// (x-b1aa). Default 1h; the daemon entrypoint overrides it from
    /// `config.agents.dead_row_grace` (via `agents_config::dead_row_grace_secs`).
    pub dead_row_grace: Duration,
    /// Fire an OS notification when a badge ENTERS `blocked` (x-dd84). Default
    /// ON; overridden from `config.mux.notify_on_blocked` at startup.
    pub notify_on_blocked: bool,
    /// Also notify on a terminal `done` hook transition. Default OFF; overridden
    /// from `config.mux.notify_on_done`.
    pub notify_on_done: bool,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        DaemonOptions {
            idle_exit: Duration::from_secs(1800),
            worker_bin: resolve_worker_bin(),
            reconcile_on_start: true,
            dead_row_grace: Duration::from_secs(crate::agents_config::DEFAULT_DEAD_ROW_GRACE_SECS),
            notify_on_blocked: true,
            notify_on_done: false,
        }
    }
}

fn resolve_worker_bin() -> PathBuf {
    if let Some(v) = std::env::var_os("FNO_AGENTS_WORKER_BIN") {
        return PathBuf::from(v);
    }
    // Side-by-side with the daemon binary.
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("fno-agents-worker")))
        .unwrap_or_else(|| PathBuf::from("fno-agents-worker"))
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("another daemon is already serving on {0}")]
    AlreadyRunning(PathBuf),
    #[error("socket permission invariant failed: {0}")]
    Permission(String),
    #[error("filesystem does not support advisory locking at {0}: {1}")]
    FlockUnsupported(PathBuf, String),
    #[error("state: {0}")]
    State(#[from] state::StateError),
}

/// Why a registry entry could not be reconciled against its `state.json` during
/// recovery. Typed so the report distinguishes the two cases a bare short_id
/// string elided (ab-3aea7437), mirroring `ReconcileOutcome`'s `(name, reason)`
/// inconsistency record. `as_str()` is the wire/event `reason` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InconsistencyReason {
    /// Registry row present, but no readable `state.json` (never spawned, or the
    /// file was removed out from under the daemon).
    MissingStateJson,
    /// `state.json` present but unreadable (I/O error or partial parse).
    UnreadableStateJson,
}

impl InconsistencyReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            InconsistencyReason::MissingStateJson => "missing_state_json",
            InconsistencyReason::UnreadableStateJson => "unreadable_state_json",
        }
    }
}

/// What recovery did, for the `daemon_started` event and tests.
#[derive(Debug, Default, PartialEq)]
pub struct RecoveryReport {
    /// `(short_id, reason)` per entry whose `state.json` could not be
    /// reconciled. The typed reason preserves *why* (missing vs unreadable),
    /// which a bare `Vec<String>` of short_ids discarded (ab-3aea7437).
    pub inconsistent: Vec<(String, InconsistencyReason)>,
    pub archived_orphans: Vec<String>,
    pub reaped_pids: Vec<u32>,
    pub recovered_drives: Vec<String>,
}

// ---------------------------------------------------------------------------
// Recovery procedure (sync, standalone-testable). Design steps 1-6; step 7
// (begin serving) is the caller's job once this returns.
// ---------------------------------------------------------------------------

/// Run the startup recovery procedure. Pure of any socket I/O so it can be
/// unit-tested against a hand-built `~/.fno/agents/` tree. The ordering
/// invariant (READ `drive_active` BEFORE clearing it, finding #12 Critical) is
/// enforced by [`crate::state::PtyState::take_active_drive`], which this calls.
pub fn recover(home: &AgentsHome, emitter: &EventEmitter) -> RecoveryReport {
    let mut report = RecoveryReport::default();
    let registry = state::load_registry(&home.registry_json()).unwrap_or_default();

    let registered: std::collections::BTreeSet<String> = registry
        .entries
        .iter()
        .map(|e| e.short_id.clone())
        .collect();

    // Steps 2-5: per registry entry, reconcile its state.json.
    for entry in &registry.entries {
        // Skip rows with no fno-managed per-agent state dir -- probing
        // `state_json` for one would emit a spurious `agent_inconsistent`
        // (Gemini medium, PR #364). Two shapes qualify:
        //   1. empty short_id: a codex/gemini shellout row (no worker key).
        //   2. a claude shellout (`ask`/`--bg`) or adopted row. Since v9 (x-1b1e)
        //      these carry the claude jobId in `short_id` (was `claude_short_id`),
        //      so the empty-short_id proxy no longer catches them; the only claude
        //      lane the daemon PTY-manages (and writes a state.json for) is the
        //      interactive stream-json worker, so a non-interactive claude row is
        //      a shellout/adopted row with no state dir.
        let is_claude_shellout = entry.provider == "claude"
            && entry.host_mode_or_default() != crate::state::HOST_MODE_INTERACTIVE;
        if entry.short_id.is_empty() || is_claude_shellout {
            continue;
        }
        let state_path = home.state_json(&entry.short_id);
        match state::load_state(&state_path) {
            Ok(Some(mut st)) => {
                // Step 3/4/5: stale drive window -> drive_crashed, then clear.
                let taken = st.pty.as_mut().and_then(|p| p.take_active_drive());
                if let Some(drive) = taken {
                    let mut fields = Map::new();
                    if let Some(sid) = &drive.session_id {
                        fields.insert("session_id".into(), Value::String(sid.clone()));
                    }
                    fields.insert("reason".into(), Value::String("daemon_restart".into()));
                    // Emit BEFORE persisting the cleared state (the read already
                    // happened inside take_active_drive; persistence is step 5).
                    let _ = emitter.emit_fields("drive_crashed", fields);
                    let _ = state::write_state_atomic(&state_path, &st);
                    report.recovered_drives.push(entry.short_id.clone());
                }
            }
            Ok(None) => {
                // Step 2: registry entry without a readable state.json. Mark
                // inconsistent; do NOT fabricate a state.json on its behalf.
                let reason = InconsistencyReason::MissingStateJson;
                let _ = emitter.emit_fields(
                    "agent_inconsistent",
                    json_obj(&[
                        ("short_id", Value::String(entry.short_id.clone())),
                        ("reason", Value::String(reason.as_str().into())),
                    ]),
                );
                report.inconsistent.push((entry.short_id.clone(), reason));
            }
            Err(_) => {
                // state.json present but unreadable. Emit the same event shape as
                // the missing case (it previously recorded nothing), so an
                // unreadable file is observable rather than silent.
                let reason = InconsistencyReason::UnreadableStateJson;
                let _ = emitter.emit_fields(
                    "agent_inconsistent",
                    json_obj(&[
                        ("short_id", Value::String(entry.short_id.clone())),
                        ("reason", Value::String(reason.as_str().into())),
                    ]),
                );
                report.inconsistent.push((entry.short_id.clone(), reason));
            }
        }
    }

    // Step 2 (other half): state.json dir without a registry entry -> archive.
    if let Ok(read) = std::fs::read_dir(home.root()) {
        for entry in read.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = match entry.file_name().into_string() {
                Ok(n) if !n.starts_with('.') => n,
                _ => continue,
            };
            if registered.contains(&name) {
                continue;
            }
            // Orphan dir (has a state.json but no registry row): archive it.
            if home.state_json(&name).exists() {
                let ts = now_compact();
                let dest = home.orphan_archive_dest(&name, &ts);
                let _ = std::fs::create_dir_all(home.orphaned_dir());
                if std::fs::rename(home.agent_dir(&name), &dest).is_ok() {
                    let _ = emitter.emit_fields(
                        "agent_orphan_state_archived",
                        json_obj(&[
                            ("short_id", Value::String(name.clone())),
                            (
                                "archived_to",
                                Value::String(dest.to_string_lossy().into_owned()),
                            ),
                        ]),
                    );
                    report.archived_orphans.push(name);
                }
            }
        }
    }

    // Step 6: orphan-PID sweep. An entry whose pid is set but is no longer OUR
    // worker is reaped (status -> exited). A live worker socket means the worker
    // (Outcome B) is still up; leave it. "No longer ours" = dead (ESRCH) OR a
    // recycled pid whose start time no longer matches what we recorded
    // (ab-d19e6458) — without the start-time check a reused pid belonging to an
    // unrelated process would keep a dead worker looking alive.
    let live_workers = home.scan_worker_sockets();
    let mut to_reap: Vec<(String, u32)> = Vec::new();
    for entry in &registry.entries {
        if live_workers.contains(&entry.short_id) {
            continue; // worker still alive; not an orphan
        }
        if let Some(pid) = entry.pid {
            if !pid_is_ours(pid, entry.pid_start_time) {
                to_reap.push((entry.short_id.clone(), pid));
            }
        }
    }
    if !to_reap.is_empty() {
        let reaped: std::collections::BTreeSet<String> =
            to_reap.iter().map(|(s, _)| s.clone()).collect();
        // Ordered exit teardown (E3.3, AC-X2-4): publish any inside-leg
        // completion before the reap write clears the report below.
        for e in &registry.entries {
            if reaped.contains(&e.short_id) {
                emit_inside_leg_completion(emitter, e);
            }
        }
        // Surface a reap-write failure rather than silently diverging the
        // event log (which says reaped) from the on-disk registry (Gemini high).
        if let Err(e) = state::update_registry(&home.registry_json(), |r| {
            for e in r.entries.iter_mut() {
                if reaped.contains(&e.short_id) {
                    e.status = AgentStatus::Exited;
                    // Clear the inside-leg authority on exit (E3.3 / AC-X2-4):
                    // a dead pane's last badge must not linger. Same for a
                    // scraped verdict.
                    e.inside_leg = None;
                    e.screen_state = None;
                }
            }
        }) {
            let _ = emitter.emit(
                "daemon_recovery_error",
                &json!({"op": "reap_orphans", "error": e.to_string()}),
            );
        }
        for (short_id, pid) in to_reap {
            let _ = emitter.emit_fields(
                "agent_orphan_reaped",
                json_obj(&[
                    ("short_id", Value::String(short_id)),
                    ("pid", Value::Number(pid.into())),
                ]),
            );
            report.reaped_pids.push(pid);
        }
    }

    report
}

/// A live process's start time, used to distinguish "our worker" from a recycled
/// PID (ab-d19e6458). `None` if the process is gone or the lookup is
/// unsupported/failed. The value is a per-host, per-boot quantity compared only
/// for equality against a value captured for the SAME pid, so the differing
/// units across platforms (Linux ticks vs macOS microseconds) do not matter.
#[cfg(target_os = "linux")]
pub fn process_start_time(pid: u32) -> Option<u64> {
    // /proc/<pid>/stat field 22 (1-based) is `starttime` in clock ticks since
    // boot. The comm field (2) can contain spaces and parens, so split on the
    // LAST ')' and index from there. After "comm)" the space-separated fields are
    // [state, ppid, ...], with starttime the 20th (0-based index 19).
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rsplit_once(')')?.1;
    after.split_whitespace().nth(19)?.parse::<u64>().ok()
}

/// macOS: `proc_pidinfo(PROC_PIDTBSDINFO)` fills a `proc_bsdinfo` whose
/// `pbi_start_tvsec`/`pbi_start_tvusec` is the process start time; fold to
/// microseconds. (`kinfo_proc` is not exposed by the libc crate.)
#[cfg(target_os = "macos")]
pub fn process_start_time(pid: u32) -> Option<u64> {
    use std::mem;
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let size = mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: buffer is a zeroed proc_bsdinfo of exactly `size` bytes.
    // proc_pidinfo returns the number of bytes written; anything other than a
    // full struct means the process is gone / not introspectable -> None.
    let written = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            size,
        )
    };
    if written != size {
        return None;
    }
    Some(info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_start_time(_pid: u32) -> Option<u64> {
    None
}

/// Outcome of one dead-row GC pass (x-b1aa), for the `fno agents reap` report
/// and tests. `reaped` lists the rows actually removed (by short_id, else name);
/// `kept_dirty` is `(id, worktree_path)` for each row kept because its worktree
/// has uncommitted changes (or the cleanliness probe failed), so the verb can
/// surface the path for the operator to clean up.
#[derive(Debug, Default, PartialEq)]
pub struct GcSummary {
    pub reaped: Vec<String>,
    pub kept_dirty: Vec<(String, String)>,
}

/// `git status --porcelain` cleanliness of a worktree-owning row's `cwd`.
/// `Some(true)` clean, `Some(false)` dirty (uncommitted changes), `None` the
/// probe could not determine it (git errored / not a repo) -> the caller fails
/// closed and keeps the row.
fn worktree_clean_probe(cwd: &str) -> Option<bool> {
    let out = std::process::Command::new("git")
        .current_dir(cwd)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout.iter().all(u8::is_ascii_whitespace))
}

/// Wall-clock epoch seconds, for GC grace math. Degrades to 0 (a pre-1970 clock
/// makes every stamped row look in-grace -> nothing reaped, the safe direction).
fn now_epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Dead-row garbage collection sweep (x-b1aa). Removes terminal, past-grace,
/// clean agent-view rows from the registry so finished rows stop accumulating
/// "like browser tabs." Shared by the daemon idle tick (the automatic path) and
/// `fno agents reap` (the manual escape hatch) -- ONE decision (`gc::gc_action`),
/// two triggers (Locked Decision #2). Idempotent and safe against a concurrent
/// sweep via the atomic reap-write: a row already gone is a no-op.
///
/// Liveness is RE-CHECKED here (AC1-FR): a row that re-registered live during the
/// grace window is never swept on a stale `exited`, and its stale `exited_at` is
/// cleared. A registry-write failure is surfaced as `daemon_recovery_error` and
/// reported as zero reaps, so the event log never claims a removal the disk did
/// not get (AC1-ERR).
pub fn gc_sweep(home: &AgentsHome, emitter: &EventEmitter, grace: Duration) -> GcSummary {
    let mut summary = GcSummary::default();
    let registry = state::load_registry(&home.registry_json()).unwrap_or_default();
    if registry.entries.is_empty() {
        return summary; // empty registry -> nothing to sweep (Boundary)
    }
    let live_workers = home.scan_worker_sockets();
    let now = now_epoch_secs();
    let grace_secs = grace.as_secs() as i64;

    // Keyed by row name -> the `created_at` we evaluated. Applied under the lock
    // ONLY when the row's current `created_at` still matches, so a same-name
    // session reaped-and-recreated (or resurrected) between this unlocked snapshot
    // + the slow git probes and the exclusive write is never clobbered by a
    // stale name-only decision (TOCTOU; gemini HIGH / codex P2 on PR #126).
    // `created_at` is the spawn-stamped identity discriminant: a replacement
    // session carries a fresh one.
    let mut to_reap: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut to_stamp: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();
    let mut to_clear: std::collections::BTreeMap<String, String> =
        std::collections::BTreeMap::new();

    for e in &registry.entries {
        let is_live = live_workers.contains(&e.short_id)
            || e.pid
                .map(|p| pid_is_ours(p, e.pid_start_time))
                .unwrap_or(false);
        let pid_confirmed_dead = e
            .pid
            .map(|p| !pid_is_ours(p, e.pid_start_time))
            .unwrap_or(false);
        let is_ask = e.is_one_shot_ask();
        let exited_at = e
            .exited_at
            .as_deref()
            .and_then(state::rfc3339_like_to_secs)
            .map(|s| s as i64);

        // Probe the worktree only for a row that could actually be reaped this
        // pass (dead + terminal + past grace + owns a worktree). Keeps git off the
        // hot path: steady state has no such rows, so no subprocess runs.
        let terminal_or_dead = matches!(e.status, AgentStatus::Exited | AgentStatus::PermanentDead)
            || pid_confirmed_dead;
        let past_grace = matches!(exited_at, Some(t) if now.saturating_sub(t) > grace_secs);
        let needs_probe = !is_live && terminal_or_dead && past_grace && !is_ask;
        let worktree_clean = if needs_probe {
            worktree_clean_probe(&e.cwd)
        } else {
            None
        };

        let row = crate::gc::GcRow {
            status: e.status,
            is_live,
            pid_confirmed_dead,
            is_ask,
            exited_at,
            worktree_clean,
        };
        let id = if e.short_id.is_empty() {
            e.name.clone()
        } else {
            e.short_id.clone()
        };
        match crate::gc::gc_action(&row, now, grace_secs) {
            crate::gc::GcAction::Reap => {
                to_reap.insert(e.name.clone(), e.created_at.clone());
            }
            crate::gc::GcAction::StampExit => {
                to_stamp.insert(e.name.clone(), e.created_at.clone());
            }
            crate::gc::GcAction::Keep => {
                if is_live && e.exited_at.is_some() {
                    // Resurrected: drop the stale exit stamp so a later death
                    // starts a fresh grace clock.
                    to_clear.insert(e.name.clone(), e.created_at.clone());
                } else if needs_probe && matches!(worktree_clean, Some(false) | None) {
                    // Past grace but held back by a dirty/undeterminable worktree.
                    summary.kept_dirty.push((id, e.cwd.clone()));
                }
            }
        }
    }

    if to_reap.is_empty() && to_stamp.is_empty() && to_clear.is_empty() {
        return summary;
    }

    let now_stamp = now_rfc3339_like();
    // Names actually removed under the lock (identity still matched), so the emit
    // + summary report only what really happened (AC1-ERR / no phantom reaps).
    let mut reaped_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let write = state::update_registry(&home.registry_json(), |r| {
        // `created_at` guard: apply each mutation only if the row under the lock is
        // still the SAME session we evaluated. A stale name whose row was
        // recreated with a fresh `created_at` is skipped (never clobbers the new
        // session); this preserves the liveness re-check guarantee across the
        // unlocked-snapshot window.
        for e in r.entries.iter_mut() {
            if to_stamp.get(&e.name) == Some(&e.created_at) {
                e.exited_at = Some(now_stamp.clone());
            }
            if to_clear.get(&e.name) == Some(&e.created_at) {
                e.exited_at = None;
            }
        }
        r.entries.retain(|e| {
            if to_reap.get(&e.name) == Some(&e.created_at) {
                reaped_names.insert(e.name.clone());
                false
            } else {
                true
            }
        });
    });
    match write {
        Ok(()) => {
            // Emit only AFTER a successful write so the event log never diverges
            // from disk (AC1-ERR), and only for rows actually removed under the
            // lock (a stale candidate whose identity changed is not a reap).
            for e in &registry.entries {
                if reaped_names.contains(&e.name) {
                    let _ = emitter.emit_fields(
                        "agent_row_reaped",
                        json_obj(&[
                            ("short_id", Value::String(e.short_id.clone())),
                            ("name", Value::String(e.name.clone())),
                        ]),
                    );
                    summary.reaped.push(if e.short_id.is_empty() {
                        e.name.clone()
                    } else {
                        e.short_id.clone()
                    });
                }
            }
        }
        Err(err) => {
            let _ = emitter.emit(
                "daemon_recovery_error",
                &json!({"op": "gc_sweep", "error": err.to_string()}),
            );
            // Nothing was removed; report no reaps (no event/disk divergence).
            summary.reaped.clear();
        }
    }
    summary
}

/// Terminal-stop sweep (x-fcbf): `claude stop` any fire-and-forget `claude --bg`
/// worker that `finalize` marked terminal. finalize (running as the worker's own
/// child) cannot self-exit it, so this daemon sweep — external to every worker —
/// runs the shipped stop on its behalf. A clean stop settles the session `(done)`
/// and is never Claude-daemon-respawned; roster-presence itself excludes owned-PTY
/// panes and operator terminals (never `claude --bg` daemon jobs), so a present +
/// marked job is exactly a done fire-and-forget bg worker.
///
/// Cheap in steady state: no markers -> one dir stat, no roster load. A stop
/// failure leaves the marker for the next tick (retry); a marker whose session is
/// already gone is dropped as stale.
async fn terminal_stop_sweep(home: &AgentsHome, emitter: &EventEmitter) {
    // read_markers (dir list + N file reads) and the roster load/parse are
    // blocking fs; run them off the async runtime so a slow disk or a large
    // marker dir never stalls a tokio worker thread. Returns the markers plus
    // the roster load result (an ERROR is kept distinct from a MISSING roster).
    let home_read = home.clone();
    let loaded = tokio::task::spawn_blocking(move || {
        let markers = crate::terminal_stop::read_markers(&home_read);
        if markers.is_empty() {
            return (markers, None);
        }
        let roster = crate::claude_roster::ClaudeRoster::load_default();
        (markers, Some(roster))
    })
    .await;
    let (markers, roster) = match loaded {
        Ok(v) => v,
        Err(e) => {
            eprintln!("daemon: terminal-stop sweep: read task failed: {e}");
            return;
        }
    };
    if markers.is_empty() {
        return;
    }
    // A load ERROR (e.g. a torn read while Claude rewrites roster.json, or a
    // future roster-format drift) must NOT be read as "session absent" — that
    // would delete every marker as stale and permanently leak the parked
    // workers this sweep exists to stop. Skip the tick and retry next time;
    // markers persist. A MISSING roster is a benign empty (Ok), correctly
    // yielding RemoveStale for a genuinely untracked session.
    let roster = match roster {
        Some(Ok(r)) => r,
        Some(Err(e)) => {
            eprintln!("daemon: terminal-stop sweep: roster load failed: {e} (retry next tick)");
            return;
        }
        None => return,
    };
    for marker in markers {
        let short = roster.find(&marker.uuid).map(|w| w.short_id().to_string());
        match crate::terminal_stop::stop_decision(short) {
            crate::terminal_stop::StopAction::Stop(short) => {
                // Bound the subprocess so a hung `claude` can never wedge the
                // sweep. A timeout leaves the marker for the next tick.
                // `kill_on_drop`: on timeout the `output()` future is dropped;
                // without this the hung child keeps running, and since the
                // marker is retried every tick that would leak a subprocess per
                // tick — the exact failure this feature exists to prevent.
                let stop = tokio::process::Command::new("claude")
                    .arg("stop")
                    .arg(&short)
                    .kill_on_drop(true)
                    .output();
                let stopped = tokio::time::timeout(Duration::from_secs(15), stop).await;
                match stopped {
                    Err(_) => eprintln!("daemon: claude stop {short} timed out (retry next tick)"),
                    Ok(Ok(o)) if o.status.success() => {
                        let _ = emitter.emit(
                            "bg_worker_terminal_stopped",
                            &json!({
                                "short_id": short,
                                "session_id": marker.uuid,
                                "reason": marker.reason,
                            }),
                        );
                        crate::terminal_stop::remove_marker(home, &marker.uuid);
                    }
                    // Non-fatal: leave the marker so the next tick retries.
                    Ok(Ok(o)) => eprintln!(
                        "daemon: claude stop {short} failed: {}",
                        String::from_utf8_lossy(&o.stderr).trim()
                    ),
                    Ok(Err(e)) => eprintln!("daemon: could not exec `claude stop`: {e}"),
                }
            }
            // The session already exited on its own (or a prior tick stopped it):
            // drop the stale marker so the dir does not grow without bound.
            crate::terminal_stop::StopAction::RemoveStale => {
                crate::terminal_stop::remove_marker(home, &marker.uuid);
            }
        }
    }
}

/// Is `pid` still OUR worker, not a recycled PID? True iff the process exists,
/// we may signal it, AND its current start time matches `recorded`
/// (ab-d19e6458). If a start time is unavailable on either side (`None` — lookup
/// unsupported/failed, or no start time was recorded for a legacy entry), fall
/// back to a bare existence check so behavior degrades to the pre-create_time
/// semantics rather than mis-deciding.
pub fn pid_is_ours(pid: u32, recorded: Option<u64>) -> bool {
    // Never treat pid 0 or 1 as ours (gemini security-high, PR #472). `kill(0, sig)`
    // signals the CALLER's whole process group and `kill(1, sig)` targets init;
    // worse, a corrupt status/registry pid of 0 would otherwise pass the probe
    // (kill(0,0)==0) and fall through to the `_ => true` arm, so a later
    // `send_sigterm(0)` would SIGTERM the client's own process group. A real
    // worker/daemon pid is never <= 1, so this only ever rejects a malformed pid.
    if pid <= 1 {
        return false;
    }
    // SAFETY: signal 0 is an existence/permission probe only. rc == 0 means the
    // process exists AND we may signal it; a non-zero rc is ESRCH (dead) or
    // EPERM (alive but owned by another user). Our worker is always the same user
    // as the daemon, so an unsignalable pid is never ours -- this also closes the
    // EPERM hole where a recycled foreign-user pid (no readable start time) would
    // otherwise fall through to "trust liveness" and be mistaken for our worker
    // (Gemini medium, PR #365).
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false;
    }
    match (recorded, process_start_time(pid)) {
        (Some(rec), Some(now)) => rec == now,
        // No basis to prove reuse -> trust existence (legacy / unsupported).
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Socket bind + perms + lazy-start race.
// ---------------------------------------------------------------------------

/// Bind the supervisor socket, resolving the lazy-start race and stale sockets.
///
/// - If a live daemon answers a connect to the existing socket, we are the race
///   loser: return [`DaemonError::AlreadyRunning`] so the caller exits cleanly.
/// - If the socket file exists but nothing answers (stale, from a crash), remove
///   and bind.
/// - Enforce dir 0700 / socket 0600 regardless of umask, fstat-verifying after
///   (finding #6 Critical).
pub async fn bind_supervisor_socket(home: &AgentsHome) -> Result<UnixListener, DaemonError> {
    home.ensure_root()?;
    flock_self_test(home)?;

    let sock = home.supervisor_sock();
    if sock.exists() {
        // Probe for a live daemon.
        if UnixStream::connect(&sock).await.is_ok() {
            return Err(DaemonError::AlreadyRunning(sock));
        }
        // Stale: remove and continue to bind.
        let _ = std::fs::remove_file(&sock);
    }

    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // A racing daemon bound between our probe and bind; it won.
            return Err(DaemonError::AlreadyRunning(sock));
        }
        Err(e) => return Err(e.into()),
    };

    paths::set_file_mode_0600(&sock)?;

    // fstat-verify the invariant; refuse to serve if either perm is wrong.
    #[cfg(unix)]
    {
        if !paths::is_dir_mode_0700(home.root()) {
            return Err(DaemonError::Permission(format!(
                "{} is not mode 0700",
                home.root().display()
            )));
        }
        if !paths::is_file_mode_0600(&sock) {
            return Err(DaemonError::Permission(format!(
                "{} is not mode 0600",
                sock.display()
            )));
        }
    }

    Ok(listener)
}

/// Prove the filesystem under `home` supports advisory locking before relying
/// on it for cross-language coordination. Network filesystems (NFS/FUSE) can
/// silently no-op flock; we refuse to start rather than corrupt shared state.
fn flock_self_test(home: &AgentsHome) -> Result<(), DaemonError> {
    let probe = home.root().join(".flock-probe");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&probe)
        .map_err(|e| DaemonError::FlockUnsupported(probe.clone(), e.to_string()))?;
    // Always clean up the probe file, even when the lock fails: an early `?`
    // here would otherwise leave a stray `.flock-probe` behind (ab-b396250f).
    let lock_res = file.lock();
    if lock_res.is_ok() {
        let _ = file.unlock();
    }
    let _ = std::fs::remove_file(&probe);
    lock_res.map_err(|e| DaemonError::FlockUnsupported(probe.clone(), e.to_string()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Serve loop.
// ---------------------------------------------------------------------------

/// Run the daemon to completion: cold_start -> recovering -> serving ->
/// (SIGTERM | idle) -> shutting_down -> exited. Returns when the process should
/// exit. The race-loser path returns `Ok(())` after logging, so the client that
/// lazy-forked it simply connects to the winner.
pub async fn run(home: AgentsHome, opts: DaemonOptions) -> Result<(), DaemonError> {
    let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

    // State: cold_start.
    let listener = match bind_supervisor_socket(&home).await {
        Ok(l) => l,
        Err(DaemonError::AlreadyRunning(_)) => {
            // Race loser: nothing to do; the winner serves.
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    // State: recovering. Recovery must complete before we accept a request.
    emit_state(&emitter, DaemonState::Recovering);
    let report = recover(&home, &emitter);

    // Architecture B (plan ab-70faa65b): ONE bounded reconcile sweep on startup,
    // as part of recovery and BEFORE the accept loop serves any client, so the
    // first `list` reads truthful process-liveness status instead of stale
    // creation-time values. Reuses the same bounded machinery as the `reconcile`
    // RPC (fairness order + 250ms/probe + 5s budget). Strictly non-fatal: a sweep
    // that returns an error (registry write failed -> registry unchanged) or even
    // panics degrades to serving last-recorded status -- we emit and continue,
    // never abort the daemon (AC1-FR). Completing before `accept` upholds the
    // Concurrency invariant that no client observes a half-applied sweep. Opt out
    // via FNO_AGENTS_NO_STARTUP_RECONCILE for the fastest cold start (discretion #5).
    if opts.reconcile_on_start {
        // Collapse a panic into an Err so the degradation has a single shape. The
        // FNO_AGENTS_FAIL_STARTUP_RECONCILE env is a test seam that forces the
        // failure path (proving the daemon keeps serving last-recorded status
        // instead of aborting -- AC1-FR); it is never set in production.
        let swept: Result<ReconcileSweepResult, String> =
            if std::env::var("FNO_AGENTS_FAIL_STARTUP_RECONCILE").is_ok() {
                Err(
                    "forced startup-reconcile failure (FNO_AGENTS_FAIL_STARTUP_RECONCILE)"
                        .to_string(),
                )
            } else {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_reconcile_sweep(&home, &emitter)
                }))
                .unwrap_or_else(|_| {
                    Err(
                        "startup reconcile sweep panicked; serving last-recorded status"
                            .to_string(),
                    )
                })
            };
        match swept {
            Ok(result) => {
                let _ = emitter.emit(
                    "startup_reconcile_done",
                    &json!({
                        "updated": result.outcome.updated.len(),
                        "deferred": result.outcome.deferred,
                    }),
                );
            }
            Err(msg) => {
                let _ = emitter.emit("startup_reconcile_failed", &json!({"error": msg}));
            }
        }
    }

    // State: serving. daemon_started is emitted AFTER recovery (step 7 ordering:
    // events.jsonl reflects reality from the first served request).
    let started_at = Instant::now();
    // Drift signal (ab-1891cdff): fingerprint the executable we are running so a
    // later client can tell whether the on-disk binary has been replaced since.
    // Also record our own pid start time so `restart` can pid-reuse-guard the
    // SIGTERM, reusing the same check the daemon already applies to workers.
    let exe_fingerprint = crate::drift::ExeFingerprint::current();
    if exe_fingerprint.is_none() {
        // Advisory only: a daemon that can't fingerprint itself just reports no
        // fingerprint, and every client drift check fails safe to Unknown.
        let _ = emitter.emit("daemon_exe_fingerprint_unavailable", &json!({}));
    }
    let pid_start_time = process_start_time(std::process::id());
    let _ = emitter.emit(
        "daemon_started",
        &json!({
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "recovered_drives": report.recovered_drives.len(),
        }),
    );
    emit_state(&emitter, DaemonState::Serving);

    // Shared across per-connection tasks (cheap Arc clone, no deep copy).
    let ctx = Arc::new(Ctx {
        home,
        emitter,
        opts,
        started_at,
        exe_fingerprint,
        pid_start_time,
        pending_inside_leg: std::sync::Mutex::new(std::collections::HashMap::new()),
    });

    // Active-backlog drain supervisor (node x-c070). Opt-in via
    // config.active_backlog; the supervisor resolves its own enabled targets and
    // stays dormant (live=false) when none, so this is byte-for-byte today's
    // behavior unless an operator turns it on. Started AFTER the Serving
    // transition (recovery is already complete here). `ab_live` keeps the daemon
    // out of idle-exit while >=1 project is enabled; `ab_shutdown` winds the task
    // down between ticks on daemon shutdown.
    let ab_live = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ab_shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ab_handle = {
        let abi_bin = std::env::var("FNO_BIN").unwrap_or_else(|_| "fno".to_string());
        let ab_emitter = EventEmitter::new(ctx.home.events_jsonl(), "active-backlog");
        let live = Arc::clone(&ab_live);
        let shutdown = Arc::clone(&ab_shutdown);
        tokio::spawn(crate::active_backlog::run_supervisor(
            abi_bin, ab_emitter, live, shutdown,
        ))
    };

    // SIGTERM -> graceful shutdown.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut idle_check = tokio::time::interval(Duration::from_secs(5));
    idle_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_activity = Instant::now();
    // Screen-manifest scrape gate: at most one sweep in flight (a slow mux
    // stalls its own sweep, never the loop or a pile-up of sweeps).
    let scrape_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Terminal-stop sweep gate (x-fcbf): same one-in-flight discipline. Each
    // `claude stop` is a subprocess; a large marker set must never serialize
    // inline in the select arm and starve accept()/SIGTERM.
    let terminal_stop_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    last_activity = Instant::now();
                    // Serve each connection in its own task so a slow or hung
                    // client cannot block the accept loop, SIGTERM, or other
                    // clients (Gemini high). Shared state is advisory-lock
                    // protected, so concurrent handling is safe.
                    let ctx = Arc::clone(&ctx);
                    tokio::spawn(async move {
                        serve_connection(ctx, stream).await;
                    });
                }
            }
            _ = sigterm.recv() => {
                emit_state(&ctx.emitter, DaemonState::ShuttingDown);
                let _ = ctx.emitter.emit("daemon_shutting_down", &json!({"reason": "sigterm"}));
                break;
            }
            _ = idle_check.tick() => {
                // Reap any worker that exited since the last tick so it never
                // lingers as a zombie under the long-lived daemon.
                reap_zombies();
                // Screen-manifest scrape sweep (the badge-lattice fallback
                // rung): subprocesses + file IO, so it runs off-loop under
                // spawn_blocking behind the one-in-flight gate.
                if !scrape_in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let flag = Arc::clone(&scrape_in_flight);
                    let home = ctx.home.clone();
                    let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
                    let notify_on_blocked = ctx.opts.notify_on_blocked;
                    tokio::task::spawn_blocking(move || {
                        crate::scrape::scrape_sweep(&home, &emitter, notify_on_blocked);
                        flag.store(false, std::sync::atomic::Ordering::SeqCst);
                    });
                }
                // Dead-row GC (x-b1aa): remove terminal, past-grace, clean
                // agent-view rows so finished rows self-clean without the merge
                // ritual. Cheap in steady state (no candidates -> no git, no
                // registry write); the grace window makes exact cadence
                // non-critical, so running it on the idle tick is fine.
                let _ = gc_sweep(&ctx.home, &ctx.emitter, ctx.opts.dead_row_grace);
                // Terminal-stop sweep (x-fcbf): exit fire-and-forget `claude --bg`
                // workers finalize marked terminal, so a shipped bg /target frees
                // its slot instead of parking at an idle prompt forever. Spawned
                // off the select arm behind a one-in-flight gate (mirrors the
                // scrape sweep) so N serialized `claude stop`s never starve
                // accept()/SIGTERM. Cheap when there are no markers.
                if !terminal_stop_in_flight.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    let flag = Arc::clone(&terminal_stop_in_flight);
                    let home = ctx.home.clone();
                    let emitter = EventEmitter::new(ctx.home.events_jsonl(), "daemon");
                    tokio::spawn(async move {
                        terminal_stop_sweep(&home, &emitter).await;
                        flag.store(false, std::sync::atomic::Ordering::SeqCst);
                    });
                }
                let empty = state::load_registry(&ctx.home.registry_json())
                    .map(|r| r.entries.is_empty())
                    .unwrap_or(true);
                // An enabled active-backlog project keeps the daemon resident even
                // when the board is drained (OQ1 Option A): idle-exit must never
                // kill a live drain supervisor.
                let ab_active = ab_live.load(std::sync::atomic::Ordering::SeqCst);
                if empty && !ab_active && last_activity.elapsed() >= ctx.opts.idle_exit {
                    emit_state(&ctx.emitter, DaemonState::IdlePendingExit);
                    let _ = ctx.emitter.emit("daemon_idle_pending_exit", &json!({}));
                    emit_state(&ctx.emitter, DaemonState::ShuttingDown);
                    let _ = ctx.emitter.emit(
                        "daemon_shutting_down",
                        &json!({"reason": "idle"}),
                    );
                    break;
                }
            }
        }
    }

    // Wind down the active-backlog supervisor: signal it to stop scheduling new
    // ticks, then abort its await. An in-flight tick's spawn_blocking thread is
    // not abortable, but that is safe by design - the dispatched worker owns its
    // node:<id> claim independently, and on the next daemon start the live-claims
    // filter excludes the still-in-flight node (no double-dispatch).
    ab_shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    ab_handle.abort();

    let _ = std::fs::remove_file(ctx.home.supervisor_sock());
    emit_state(&ctx.emitter, DaemonState::Exited);
    let _ = ctx.emitter.emit("daemon_exited", &json!({"clean": true}));
    Ok(())
}

/// Daemon-wide context passed to handlers.
struct Ctx {
    home: AgentsHome,
    emitter: EventEmitter,
    opts: DaemonOptions,
    started_at: Instant,
    /// Fingerprint of the executable this daemon is running (ab-1891cdff),
    /// captured once at startup. `None` if `current_exe()`/stat failed; the
    /// status payload then reports null and clients fail safe to `Unknown`.
    exe_fingerprint: Option<crate::drift::ExeFingerprint>,
    /// This daemon's own process start time, for the `restart` pid-reuse guard.
    /// `None` on platforms/paths where it is unavailable (the guard degrades to
    /// a bare existence check, like the worker path).
    pid_start_time: Option<u64>,
    /// Early-push buffer (inside-out E3.3, buffer-on-early-push): inside-leg
    /// reports keyed by session_id that arrived before their registry row
    /// existed (a per-turn hook can fire faster than the daemon registers the
    /// pane). Flushed onto the row at creation (`handle_spawn` /
    /// `spawn_claude_stream_lane`). Bounded by [`PENDING_INSIDE_LEG_CAP`] so a
    /// flood of pushes for sessions that never register cannot grow without
    /// limit. Highest seq wins per session.
    pending_inside_leg: std::sync::Mutex<std::collections::HashMap<String, state::InsideLegReport>>,
}

/// Cap on the early-push buffer (E3.3). A report for a NEW session is dropped
/// (logged `buffer_full`) once the buffer is at cap; an already-buffered
/// session's seq still advances (no new key). 64 covers any realistic burst of
/// panes registering at once while staying a hard ceiling.
const PENDING_INSIDE_LEG_CAP: usize = 64;

fn emit_state(emitter: &EventEmitter, state: DaemonState) {
    let _ = emitter.emit("daemon_state", &json!({"state": state.as_str()}));
}

/// Idle cap for the first read on a connection: a client that connects but
/// never sends a frame self-terminates rather than holding the task forever.
const CONN_READ_TIMEOUT: Duration = Duration::from_secs(30);

async fn serve_connection(ctx: Arc<Ctx>, mut stream: UnixStream) {
    // One request per accepted connection (clients open per RPC). A read fault
    // is mapped to a structured error response so callers get a deterministic
    // error code rather than a transport EOF (Codex P2): only a clean hangup
    // (UnexpectedEof) is silent. A silent client is bounded by the timeout.
    let req = match tokio::time::timeout(CONN_READ_TIMEOUT, read_request(&mut stream)).await {
        Err(_elapsed) => return, // client sent nothing within the window; drop
        Ok(Ok(r)) => r,
        Ok(Err(crate::protocol::ProtocolError::UnexpectedEof)) => return, // clean hangup
        Ok(Err(e)) => {
            // Malformed / oversized frame: we could not parse a request id, so
            // reply against id 0 with a structured MalformedFrame error.
            let resp = Response::err(0, ErrorCode::MalformedFrame, format!("{e}"));
            let _ = write_response(&mut stream, &resp).await;
            return;
        }
    };
    // `agent.logs` (with --follow) upgrades the same stream to a
    // WebSocket and streams appended log lines until the client detaches; it
    // does not fit the one-request/one-response shape.
    if req.method == "agent.logs" {
        crate::logs::handle_logs(&ctx.home, &req, stream).await;
        return;
    }
    let resp = dispatch(&ctx, &req).await;
    let _ = write_response(&mut stream, &resp).await;
}

/// Run a synchronous (flock + CPU, no socket I/O) handler on the blocking pool
/// so its advisory-lock wait never starves the async executor (Gemini high).
async fn run_blocking<F>(ctx: &Arc<Ctx>, req: &Request, f: F) -> Response
where
    F: FnOnce(&Ctx, &Request) -> Response + Send + 'static,
{
    let ctx = Arc::clone(ctx);
    let req = req.clone();
    let id = req.id;
    match tokio::task::spawn_blocking(move || f(&ctx, &req)).await {
        Ok(resp) => resp,
        Err(_) => Response::err(id, ErrorCode::Internal, "handler task panicked"),
    }
}

/// Offload the blocking flock + file read of `state::load_registry` to the
/// blocking pool so it never stalls an async handler's runtime thread
/// (ab-e86e326b). Mirrors the existing `handle_status` offload and the
/// `run_blocking` wrapper. A join failure or a read error both collapse to the
/// empty registry, matching the `.unwrap_or_default()` the inline callers used.
async fn load_registry_offloaded(path: PathBuf) -> state::Registry {
    tokio::task::spawn_blocking(move || state::load_registry(&path))
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default()
}

/// Offload the blocking read-modify-write of `state::update_registry` to the
/// blocking pool (ab-e86e326b). The closure runs on the blocking thread, so it
/// must be `Send + 'static` (callers move owned clones in). A join panic maps to
/// a `StateError::Io` so callers' existing error handling fires.
async fn update_registry_offloaded<F, T>(path: PathBuf, f: F) -> Result<T, state::StateError>
where
    F: FnOnce(&mut state::Registry) -> T + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(move || state::update_registry(&path, f)).await {
        Ok(result) => result,
        Err(e) => Err(state::StateError::Io(std::io::Error::other(format!(
            "update_registry task panicked: {e}"
        )))),
    }
}

async fn dispatch(ctx: &Arc<Ctx>, req: &Request) -> Response {
    match Namespace::of(&req.method) {
        Namespace::Agent => dispatch_agent(ctx, req).await,
        Namespace::Channel => dispatch_channel(ctx, req).await,
        Namespace::Unknown => Response::err(
            req.id,
            ErrorCode::UnknownMethod,
            format!("unknown namespace for method `{}`", req.method),
        ),
    }
}

async fn dispatch_agent(ctx: &Arc<Ctx>, req: &Request) -> Response {
    // Async handlers (spawn/ask/stop) interleave worker-socket I/O and stay on
    // the async runtime; pure-sync handlers go to the blocking pool.
    match Namespace::verb(&req.method) {
        Some("spawn") => handle_spawn(ctx, req).await,
        Some("ask") => handle_ask(ctx, req).await,
        Some("switchboard") => handle_switchboard(ctx, req).await,
        Some("stop") => handle_stop(ctx, req).await,
        Some("rm") => handle_rm(ctx, req).await,
        Some("list") => run_blocking(ctx, req, handle_list).await,
        // status reads the in-memory drive table for the active-drives count, so
        // it stays on the async runtime rather than the blocking pool.
        Some("status") => handle_status(ctx, req).await,
        Some("reconcile") => run_blocking(ctx, req, handle_reconcile).await,
        // Inside-leg state push (E3.2): a per-turn hook stores the latest
        // {working|blocked|done} on the matching claude row. Pure flock + CPU.
        Some("report") => run_blocking(ctx, req, handle_report).await,
        _ => Response::err(
            req.id,
            ErrorCode::UnknownMethod,
            format!("unknown agent verb in `{}`", req.method),
        ),
    }
}

/// Validate an agent name: 1..=64 chars from `[A-Za-z0-9_-]` (US1 dispatch rule).
fn valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Derive a short id from a name, made unique against the registry.
fn derive_short_id(name: &str, registry: &state::Registry) -> String {
    let base: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect();
    let base = if base.is_empty() {
        "agent".into()
    } else {
        base
    };
    if registry.entries.iter().all(|e| e.short_id != base) {
        return base;
    }
    for n in 1..10_000 {
        let cand = format!("{base}{n}");
        if registry.entries.iter().all(|e| e.short_id != cand) {
            return cand;
        }
    }
    format!("{base}-{}", now_compact())
}

/// Whether `e` records `uuid` as its resume target (any provider id field).
/// `pub` so `subscribe` can resolve a hook report's `session_id` back to a row
/// name using the daemon's own matching, never a forked lookup.
pub fn entry_holds_session(e: &RegistryEntry, uuid: &str) -> bool {
    e.codex_session_id.as_deref() == Some(uuid)
        || e.gemini_session_id.as_deref() == Some(uuid)
        || e.session_id.as_deref() == Some(uuid)
        // Interactive claude (E1) records its pinned session in claude_session_uuid;
        // the locked one-host re-check matches it here so a second writer on one
        // session id is refused even when the file claim is unavailable.
        || e.claude_session_uuid.as_deref() == Some(uuid)
}

/// Non-terminal == has (or expects) a live backend. Exited/PermanentDead are
/// the only terminal states.
fn is_non_terminal(s: AgentStatus) -> bool {
    !matches!(s, AgentStatus::Exited | AgentStatus::PermanentDead)
}

async fn handle_spawn(ctx: &Ctx, req: &Request) -> Response {
    let p = &req.params;
    let name = match p.get("name").and_then(|v| v.as_str()) {
        Some(n) if valid_agent_name(n) => n.to_string(),
        Some(_) => {
            return Response::err(
                req.id,
                ErrorCode::InvalidParams,
                "name must be 1-64 chars of [A-Za-z0-9_-]",
            )
        }
        None => return Response::err(req.id, ErrorCode::InvalidParams, "missing `name`"),
    };
    let provider = p
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("codex")
        .to_string();
    // A missing `cwd` means a misbehaving client: the daemon is a shared,
    // long-lived process, so fall back to a neutral temp dir and emit an event
    // so the /tmp launch is greppable rather than silently adopting the daemon's
    // own repo. A well-behaved client always forwards cwd.
    let cwd = match p.get("cwd").and_then(|v| v.as_str()) {
        Some(c) => PathBuf::from(c),
        None => {
            let fallback = std::env::temp_dir();
            let _ = ctx.emitter.emit(
                "agent_spawn_cwd_fallback",
                &json!({"name": name, "fallback": fallback.to_string_lossy()}),
            );
            fallback
        }
    };
    // Post-G4 (x-f54c): the daemon hosts no agent PTYs, so the only spawn it
    // still serves is the claude stream-json ADOPTION lane -- host_mode=interactive
    // + mode=stream_json resumes an idle session as a held stream thread
    // (`claude -p --resume <uuid>`) for chat/switchboard/ask to drive. Every
    // interactive PTY host (codex, gemini, claude) moved to the mux, and bg/
    // headless never reach the daemon, so any other spawn is a retired
    // PTY-hosting request and errors with a mux pointer.
    let host_mode = p
        .get("host_mode")
        .and_then(|v| v.as_str())
        .unwrap_or(crate::state::HOST_MODE_EXEC);
    let resume_id = p
        .get("resume_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if host_mode == crate::state::HOST_MODE_INTERACTIVE && provider == "claude" {
        let claude_mode = p
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or(crate::state::CLAUDE_MODE_STREAM_JSON);
        if claude_mode != crate::state::CLAUDE_MODE_INTERACTIVE {
            let explicit_argv = p.get("argv").and_then(|v| v.as_array()).map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<String>>()
            });
            return spawn_claude_stream_lane(
                ctx,
                req,
                &name,
                &cwd,
                resume_id.as_deref(),
                explicit_argv,
            )
            .await;
        }
    }
    let _ = ctx.emitter.emit(
        "agent_spawn_failed",
        &json!({"name": name, "reason": "daemon_pty_hosting_retired", "provider": provider}),
    );
    Response::err(
        req.id,
        ErrorCode::InvalidParams,
        "daemon PTY hosting was retired at G4 (x-f54c): spawn a mux-hosted agent pane with \
         `fno agents spawn --substrate pane`, or use `--substrate bg|headless`. The daemon \
         serves only claude stream-json adoption (host_mode=interactive, mode=stream_json).",
    )
}

// ---------------------------------------------------------------------------
// Claude stream-json host lane front door (Group 3, ab-734fcd6c).
// ---------------------------------------------------------------------------

/// The single-writer claim holder for an adopted claude stream thread, derived
/// from its short_id (stable + unique per thread). The worker releases the claim
/// by this EXACT string (passed via `--holder`), so the daemon's acquire and the
/// worker's RAII release must agree on it.
fn stream_claim_holder(short_id: &str) -> String {
    format!("stream:{short_id}")
}

/// Is this row a LIVE writer for the one-host guard? Narrower than
/// [`is_non_terminal`]: it EXCLUDES the dead-but-non-terminal states (`Orphaned`
/// = the child died and the worker released its claim; `Failed` = the task
/// panicked) so a session whose adopted thread has died is re-adoptable. AC1-FR
/// marks a dead thread `orphaned` and releases the claim, and AC1-EDGE refuses a
/// second adopt only for a session "currently held LIVE by another process" —
/// using `is_non_terminal` here would wrongly keep an orphaned UUID un-adoptable
/// until a reconcile/rm cleared the row.
fn is_live_writer(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Live
            | AgentStatus::Ready
            | AgentStatus::Idle
            | AgentStatus::Busy
            | AgentStatus::Spawning
            | AgentStatus::Restarting
    )
}

/// The worker argv for the claude stream-json lane (everything after the worker
/// BINARY path). `parse_stream_args` in bin/worker.rs accepts these flags in any
/// order before `--`; the child argv (normally
/// [`crate::provider::claude_stream_json_resume_argv`]) follows the separator.
/// Pure so the flag wiring is unit-testable without spawning a process.
fn claude_stream_worker_args(
    short_id: &str,
    home: &std::path::Path,
    cwd: &std::path::Path,
    uuid: &str,
    holder: &str,
    child_argv: &[String],
) -> Vec<String> {
    let mut args = vec![
        "--stream".into(),
        "--short-id".into(),
        short_id.into(),
        "--home".into(),
        home.to_string_lossy().into_owned(),
        "--cwd".into(),
        cwd.to_string_lossy().into_owned(),
        "--session-uuid".into(),
        uuid.into(),
        "--holder".into(),
        holder.into(),
        "--".into(),
    ];
    args.extend(child_argv.iter().cloned());
    args
}

/// Build the registry row for an adopted claude stream thread. `provider`=claude
/// + `host_mode`=interactive (so `is_interactive()` keeps reconcile from
/// settling it `exited` like a one-shot) + the FULL `claude_session_uuid` (the
/// resume key, finally populated here -- the field G1 added is set by the front
/// door). Pure so the row shape is asserted without a live spawn.
fn build_claude_stream_entry(
    name: &str,
    short_id: &str,
    cwd: &std::path::Path,
    uuid: &str,
    pid: u32,
    pid_start_time: Option<u64>,
    log_path: PathBuf,
) -> RegistryEntry {
    let cwd_s = cwd.to_string_lossy().into_owned();
    RegistryEntry {
        name: name.into(),
        short_id: short_id.into(),
        provider: "claude".into(),
        harness: Some("claude".into()),
        harness_session_id: Some(uuid.into()),
        cwd: cwd_s.clone(),
        project_root: cwd_s,
        session_id: None,
        legacy_claude_short_id: None,
        claude_session_uuid: Some(uuid.into()),
        messaging_socket_path: None,
        codex_session_id: None,
        gemini_session_id: None,
        mcp_channel_id: None,
        cc_session_id: None,
        host_mode: Some(crate::state::HOST_MODE_INTERACTIVE.into()),
        status: AgentStatus::Live,
        last_message_at: Some(now_rfc3339_like()),
        created_at: now_rfc3339_like(),
        pid: Some(pid),
        pid_start_time,
        log_path: Some(log_path.to_string_lossy().into_owned()),
        last_reconciled_at: None,
        inside_leg: None,
        exited_at: None,
        mux: None,
        screen_state: None,
    }
}

/// Outcome of the pre-spawn single-writer claim acquisition.
#[derive(Debug)]
enum ClaimOutcome {
    /// We hold `session:<uuid>` (fresh acquire or idempotent re-acquire).
    Acquired,
    /// Another live writer holds it; refuse to double-adopt (AC1-EDGE).
    HeldByOther(String),
    /// The claim substrate could not be consulted (no `fno` on PATH, exec error,
    /// unparseable output). Fail OPEN: the registry one-host re-check under the
    /// lock is the authoritative in-daemon guard; the file-claim is the
    /// cross-process coordination record, best-effort like the worker's release.
    Unavailable(String),
}

/// Acquire the `session:<uuid>` single-writer claim before spawning the stream
/// worker (Locked Decision 5; the worker's `SessionClaimGuard` RELEASES it on
/// orphan/exit, so the daemon only acquires). Native `crate::claims` call — no
/// subprocess, no Python cold start on the adopt path. The record is anchored
/// to the daemon's own (long-lived) pid, so the claim is live from birth: the
/// old acquire-to-reanchor stale window, where a concurrent adopter could
/// reclaim a claim pinned to an already-dead `fno` subprocess, is gone
/// structurally. The fail-open posture on an unconsultable substrate
/// (`Unavailable` -> registry one-host re-check remains authoritative) is
/// unchanged.
fn acquire_session_claim(uuid: &str, holder: &str) -> ClaimOutcome {
    match crate::claims::acquire(
        &format!("session:{uuid}"),
        holder,
        crate::claims::AcquireOpts::default(),
    ) {
        crate::claims::AcquireOutcome::Acquired(_) => ClaimOutcome::Acquired,
        crate::claims::AcquireOutcome::HeldByOther { holder, .. } => {
            ClaimOutcome::HeldByOther(holder)
        }
        crate::claims::AcquireOutcome::Error(e) => ClaimOutcome::Unavailable(e),
    }
}

/// RAII release for the daemon-held single-writer claim. Armed when the daemon
/// acquires `session:<uuid>` before spawn; on Drop it releases the claim UNLESS
/// disarmed (the worker has taken ownership of the claim once the row is
/// registered `live` and owns its own RAII release). This means every
/// early-return failure path releases exactly once with no manual call (gemini
/// review HIGH: prefer RAII over scattered manual releases). The release is a
/// native file operation (microseconds), so it no longer needs a detached
/// subprocess or the idle-tick reaper to stay off the async executor.
struct DaemonClaimGuard {
    session_uuid: String,
    holder: String,
    armed: bool,
}

impl DaemonClaimGuard {
    /// The worker now owns the claim (registered live); the daemon must not
    /// release it on drop. Consumes the guard so it cannot fire afterward.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for DaemonClaimGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Best-effort native release: an error is ignored (the claim's
        // PID-liveness + reconcile are the backstops). AC1-ERR: a failed adopt
        // must release any claim it acquired. Direct call — file io in a Drop
        // is microseconds, and there is no detached child for the idle-tick
        // reaper to sweep anymore.
        let _ = crate::claims::release(
            &format!("session:{}", self.session_uuid),
            &self.holder,
            None,
            None,
        );
    }
}

/// Does the stream worker at `sock` report its `claude -p --resume` child ALIVE?
/// A dead-on-arrival resume (bad/expired UUID, auth failure) exits immediately,
/// yet the worker still binds its socket and answers `stream.ping`; querying
/// `stream.status.child_alive` (backed by `try_wait`) distinguishes "worker up +
/// child live" from "worker up + child already exited", so a DOA adopt is
/// rejected instead of registered `live` (AC1-ERR; codex review P2). Bounded so a
/// wedged worker never hangs the daemon; a timeout reads as not-alive.
async fn stream_worker_reports_child_alive(sock: &std::path::Path) -> bool {
    let probe = async {
        let mut conn = UnixStream::connect(sock).await.ok()?;
        write_request(&mut conn, &Request::new(1, "stream.status", json!({})))
            .await
            .ok()?;
        let resp = crate::protocol::read_response(&mut conn).await.ok()?;
        Some(
            resp.result()
                .and_then(|r| r.get("child_alive"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(STREAM_PROBE_TIMEOUT_S), probe).await,
        Ok(Some(true))
    )
}

/// Spawn (adopt) a claude session as a held stream-json thread under the daemon
/// (Task 5.1). This is the claude analog of the codex/gemini PTY promote path in
/// `handle_spawn`: validate -> single-writer guard -> spawn the per-session
/// worker (Outcome B: own process group, detached) -> confirm it serves the
/// stream protocol -> register `live`. The worker resumes the FULL session UUID
/// (`claude -p --resume`); readiness is the worker answering `stream.ping`
/// (Locked Decision 9: a stream-json session emits nothing until the first turn,
/// so we never wait for a spontaneous `init` event).
async fn spawn_claude_stream_lane(
    ctx: &Ctx,
    req: &Request,
    name: &str,
    cwd: &std::path::Path,
    resume_id: Option<&str>,
    explicit_argv: Option<Vec<String>>,
) -> Response {
    // 1. Adoption requires a resume target. A fresh `host --provider claude`
    //    (no --from) has nothing to resume; point the user at the adopt verb.
    let uuid = match resume_id {
        Some(u) if !u.trim().is_empty() => u,
        _ => {
            let _ = ctx.emitter.emit(
                "agent_spawn_failed",
                &json!({"name": name, "reason": "claude_host_needs_from"}),
            );
            return Response::err(
                req.id,
                ErrorCode::InvalidParams,
                "claude has no fresh interactive host; adopt an idle session: `fno agents promote <name> --from <session-uuid> --provider claude`",
            );
        }
    };

    // 2. Lock-free pre-checks for clean messages (the authoritative re-checks run
    //    atomically under the registry lock at registration).
    let registry = load_registry_offloaded(ctx.home.registry_json()).await;
    if let Some(existing) = registry.find(name) {
        return Response::err(
            req.id,
            ErrorCode::AgentExists,
            format!(
                "agent {name} already exists (short_id={}); use `fno agents rm` first",
                existing.short_id
            ),
        );
    }
    // Single-writer one-host pre-check: refuse a second adopt of the same session
    // (AC1-EDGE). Matches a LIVE claude row already carrying this UUID; an
    // orphaned/exited row (dead child, claim released) is re-adoptable (AC1-FR).
    if let Some(h) = registry.entries.iter().rev().find(|e| {
        e.provider == "claude"
            && e.claude_session_uuid.as_deref() == Some(uuid)
            && is_live_writer(e.status)
    }) {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            format!(
                "session '{uuid}' is already hosted by live stream thread '{}'; one writer per session",
                h.name
            ),
        );
    }
    let short_id = derive_short_id(name, &registry);
    let holder = stream_claim_holder(&short_id);

    // 3. Acquire the single-writer claim BEFORE spawning (Locked Decision 5). A
    //    clear held-by-other refusal aborts; an unavailable substrate fails open
    //    (the registry one-host re-check below is the authoritative in-daemon
    //    guard). Run on the blocking pool: `fno` is a short-lived subprocess.
    let uuid_owned = uuid.to_string();
    let holder_for_acq = holder.clone();
    let claim_outcome =
        tokio::task::spawn_blocking(move || acquire_session_claim(&uuid_owned, &holder_for_acq))
            .await
            .unwrap_or_else(|e| ClaimOutcome::Unavailable(format!("claim task panicked: {e}")));
    // The guard releases the claim on EVERY early return below until it is
    // disarmed at successful registration (the worker then owns the claim).
    let claim_guard = match claim_outcome {
        ClaimOutcome::Acquired => DaemonClaimGuard {
            session_uuid: uuid.to_string(),
            holder: holder.clone(),
            armed: true,
        },
        ClaimOutcome::HeldByOther(who) => {
            let _ = ctx.emitter.emit(
                "agent_spawn_failed",
                &json!({"name": name, "reason": "session_claimed", "detail": who}),
            );
            return Response::err(
                req.id,
                ErrorCode::InvalidParams,
                format!(
                    "session '{uuid}' is held by another writer ({who}); refusing to double-adopt"
                ),
            );
        }
        ClaimOutcome::Unavailable(why) => {
            let _ = ctx.emitter.emit(
                "agent_stream_claim_unavailable",
                &json!({"name": name, "session_uuid": uuid, "detail": why}),
            );
            // Nothing to release (we never acquired); a disarmed guard keeps the
            // rest of the function uniform.
            DaemonClaimGuard {
                session_uuid: uuid.to_string(),
                holder: holder.clone(),
                armed: false,
            }
        }
    };

    // 4. Build the child argv and spawn the per-session stream worker in its own
    //    process group (Outcome B: survives a kill of the daemon's group). The
    //    explicit-argv escape hatch lets tests substitute a fake stream emitter so
    //    CI never spawns a real `claude -p` (Test discipline / Locked Decision 1).
    let child_argv =
        explicit_argv.unwrap_or_else(|| crate::provider::claude_stream_json_resume_argv(uuid));
    let worker_args =
        claude_stream_worker_args(&short_id, ctx.home.root(), cwd, uuid, &holder, &child_argv);
    let mut cmd = std::process::Command::new(&ctx.opts.worker_bin);
    cmd.args(&worker_args);
    cmd.process_group(0);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // claim_guard releases on return.
            let _ = ctx.emitter.emit(
                "agent_spawn_failed",
                &json!({"name": name, "reason": "binary_not_found", "detail": e.to_string()}),
            );
            return Response::err(
                req.id,
                ErrorCode::SpawnFailed,
                format!("could not launch stream worker: {e}"),
            );
        }
    };
    let worker_pid = child.id();
    let worker_pid_start_time = process_start_time(worker_pid);
    drop(child);

    // 5. Wait (bounded) for the worker socket to appear, proving the worker bound.
    let sock = ctx.home.worker_sock(&short_id);
    let start = Instant::now();
    while !sock.exists() && start.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    if !sock.exists() {
        // claim_guard releases on return.
        let _ = ctx.emitter.emit(
            "agent_create_no_session",
            &json!({"name": name, "short_id": short_id, "lane": "stream"}),
        );
        return Response::err(
            req.id,
            ErrorCode::SpawnFailed,
            "stream worker did not come up within 10s",
        );
    }

    // 6. Confirm the worker actually serves the stream protocol (a `stream.ping`
    //    answer). This is the readiness proof (LD9: drive-a-turn, not wait-for-init
    //    -- the ping is the cheapest drive that confirms the worker, without
    //    spending a real turn). A bound-but-wrong worker fails here, not `live`.
    if !is_live_stream_thread(&sock).await {
        best_effort_worker_shutdown(&sock).await;
        let _ = ctx.emitter.emit(
            "agent_create_no_session",
            &json!({"name": name, "short_id": short_id, "reason": "not_a_stream_thread"}),
        );
        return Response::err(
            req.id,
            ErrorCode::SpawnFailed,
            "stream worker came up but does not serve the stream protocol",
        );
    }

    // 6b. Confirm the resumed child is ALIVE before registering live (AC1-ERR;
    //     codex review P2). A dead-on-arrival `claude -p --resume` (bad/expired
    //     UUID, auth failure) exits immediately but the worker still binds its
    //     socket and answers `stream.ping`; `stream.status.child_alive` (try_wait)
    //     catches it so the adopt is rejected, not registered live then silently
    //     orphaned.
    if !stream_worker_reports_child_alive(&sock).await {
        best_effort_worker_shutdown(&sock).await;
        let _ = ctx.emitter.emit(
            "agent_create_no_session",
            &json!({"name": name, "short_id": short_id, "reason": "resume_child_exited"}),
        );
        return Response::err(
            req.id,
            ErrorCode::SpawnFailed,
            "claude --resume child exited before adoption (bad/expired session id, auth failure, or dead cwd)",
        );
    }

    // 7. Register under the exclusive registry lock. Two concurrent adopts can
    //    both pass the lock-free checks above; the locked re-check (name + the
    //    one-host UUID guard) means exactly one inserts. The loser shuts its
    //    just-started worker down (which releases the claim via the worker's RAII
    //    guard) so it is never leaked untracked.
    let entry = build_claude_stream_entry(
        name,
        &short_id,
        cwd,
        uuid,
        worker_pid,
        worker_pid_start_time,
        ctx.home.timeline_jsonl(&short_id),
    );
    let uuid_for_lock = uuid.to_string();
    let insert = update_registry_offloaded(ctx.home.registry_json(), move |r| {
        if r.entries.iter().any(|e| e.name == entry.name) {
            return false;
        }
        if r.entries.iter().any(|e| {
            e.provider == "claude"
                && e.claude_session_uuid.as_deref() == Some(&uuid_for_lock)
                && is_live_writer(e.status)
        }) {
            return false;
        }
        r.entries.push(entry);
        true
    })
    .await;
    match insert {
        // E3.3 buffer-on-early-push: drain any report buffered before this stream
        // row existed onto it now that it is registered (race-free post-insert).
        Ok(true) => flush_buffered_inside_leg(ctx, uuid, name),
        Ok(false) => {
            best_effort_worker_shutdown(&sock).await;
            let _ = ctx.emitter.emit(
                "agent_spawn_failed",
                &json!({"name": name, "short_id": short_id, "reason": "session_taken_concurrent"}),
            );
            return Response::err(
                req.id,
                ErrorCode::AgentExists,
                format!("session '{uuid}' was adopted by a concurrent call; this one refused"),
            );
        }
        Err(e) => {
            best_effort_worker_shutdown(&sock).await;
            let _ = ctx.emitter.emit(
                "agent_spawn_failed",
                &json!({"name": name, "short_id": short_id, "reason": "registry_write_failed"}),
            );
            return Response::err(req.id, ErrorCode::Internal, format!("registry write: {e}"));
        }
    }
    // Registered live: the worker now owns the claim (its own SessionClaimGuard
    // releases it on orphan/exit), so the daemon must not release on drop.
    claim_guard.disarm();
    let _ = ctx.emitter.emit(
        "agent_spawned",
        &json!({"name": name, "provider": "claude", "short_id": short_id, "lane": "stream", "session_uuid": uuid}),
    );

    Response::ok(
        req.id,
        json!({"short_id": short_id, "provider": "claude", "status": "live", "lane": "stream"}),
    )
}

/// Map a provider name string to a per-CLI readiness detector.
///
/// NOTE: This is a local match rather than routing through `Box<dyn Provider>`
/// because the provider trait impls live in `provider.rs` with no `from_str`
/// constructor. A full resolver is the right long-term home (LD8); for now the
/// match is the surgical minimum that unblocks Task 1.1 without touching
/// provider.rs.
fn provider_readiness_detector(provider: &str) -> Box<dyn crate::readiness::ReadinessDetector> {
    use crate::provider::ProviderWithPty as _;
    match provider {
        "codex" => crate::provider::CodexProvider.readiness_detector(),
        "gemini" => crate::provider::GeminiProvider.readiness_detector(),
        "agy" => crate::provider::AgyProvider.readiness_detector(),
        "opencode" => crate::provider::OpencodeProvider.readiness_detector(),
        // E1 (codex review P2): interactive claude rows need a real detector, else
        // `agent.ask` polls NoSignalDetector and times out with "no readiness
        // signal" despite ClaudeReadinessDetector existing. Same source of truth.
        "claude" => crate::provider::ClaudeInteractiveProvider.readiness_detector(),
        // Carry the real provider name so the UnknownReadinessSignal error and
        // provider_name() name the actual CLI (e.g. "opencode") rather than the
        // literal "unknown" (cv-789fdba0).
        other => Box::new(crate::readiness::NoSignalDetector {
            provider: other.to_string(),
        }),
    }
}

/// Poll the worker snapshot in a bounded loop until the per-provider readiness
/// detector reports the CLI is idle at a prompt, then return the settled screen
/// text. Returns `Err(String)` on timeout.
///
/// Each iteration feeds a FRESH `TerminalGrid` from the full snapshot string
/// (the snapshot is the whole current screen, not a delta) so the grid reflects
/// the current state without accumulated duplicates.
///
/// # Path choice (b) note
/// The worker's `worker.snapshot` RPC returns `text: String` (the lossy UTF-8
/// decoding of the PTY ring). Feeding `text.as_bytes()` back into a
/// `TerminalGrid` is slightly redundant for plain ASCII output but is correct
/// for all vt100-renderable content: the vt100 parser re-interprets the
/// decoded bytes. The alternative (adding a `raw_bytes_b64` field to the
/// snapshot RPC) was considered but would require a worker.rs protocol change;
/// given that the readiness detectors only examine prompt-glyph patterns on the
/// visible text, the lossy path is sufficient.
/// Failure modes of [`poll_until_ready`]. Distinguishes a CLI that never settled
/// within the budget from a worker whose snapshot read itself hung, so the daemon
/// (and anyone reading the ask error) can tell "slow CLI" from "stuck worker"
/// instead of two indistinguishable `String`s (cv-789fdba0). Display output is
/// byte-identical to the prior inline format strings.
#[derive(Debug, PartialEq, Eq)]
enum PollError {
    /// The readiness detector never reported ready before the deadline.
    Timeout { secs: u64 },
    /// A single worker-snapshot fetch did not return before the deadline.
    WorkerUnresponsive { secs: u64 },
}

impl std::fmt::Display for PollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PollError::Timeout { secs } => {
                write!(f, "ask timed out after {secs}s before reply settled")
            }
            PollError::WorkerUnresponsive { secs } => write!(
                f,
                "ask timed out after {secs}s before reply settled (worker snapshot read did not return)"
            ),
        }
    }
}

async fn poll_until_ready<F, Fut>(
    fetcher: F,
    detector: Box<dyn crate::readiness::ReadinessDetector>,
    poll_interval: Duration,
    timeout: Duration,
) -> Result<String, PollError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(PollError::Timeout {
                secs: timeout.as_secs(),
            });
        }
        // Bound the snapshot fetch by the remaining time to the deadline.
        // fetcher() performs socket I/O to the worker; without this cap a hung
        // or deadlocked worker would block the daemon indefinitely, since the
        // deadline check above only runs between iterations (gemini-code-assist
        // security-critical on PR #361). A per-fetch timeout converts a hung
        // read into the same bounded "ask timed out" error as a slow CLI.
        let remaining = deadline.saturating_duration_since(now);
        let fetched = match tokio::time::timeout(remaining, fetcher()).await {
            Ok(opt) => opt,
            Err(_) => {
                return Err(PollError::WorkerUnresponsive {
                    secs: timeout.as_secs(),
                })
            }
        };
        if let Some(text) = fetched {
            // Fresh grid each iteration: the snapshot is the full current screen.
            let mut grid = crate::screen::TerminalGrid::with_default_size();
            grid.feed(text.as_bytes());
            let owned = grid.snapshot();
            let view = owned.view();
            match detector.is_ready(&view) {
                Ok(true) => return Ok(owned.text),
                Ok(false) | Err(_) => {} // not ready yet; Err treated as not-ready (Open Question #9 discipline)
            }
        }
        tokio::time::sleep(poll_interval).await;
    }
}

async fn handle_ask(ctx: &Ctx, req: &Request) -> Response {
    let name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return Response::err(req.id, ErrorCode::InvalidParams, "missing `name`"),
    };
    let message = req
        .params
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let provider_param = req
        .params
        .get("provider")
        .and_then(|v| v.as_str())
        .map(String::from);
    let cwd_param = req
        .params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from);
    let from_name_param = req
        .params
        .get("from_name")
        .and_then(|v| v.as_str())
        .map(String::from);
    let yolo_param = req
        .params
        .get("yolo")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let registry = load_registry_offloaded(ctx.home.registry_json()).await;
    let entry = match registry.find(&name) {
        Some(e) => e.clone(),
        None => {
            // First contact: auto-spawn if --provider supplied (create-on-first-contact,
            // matching Python cmd_ask semantics). No provider = actionable error.
            let provider = match provider_param {
                Some(p) => p,
                None => {
                    return Response::err(
                        req.id,
                        ErrorCode::InvalidParams,
                        format!(
                        "agent '{name}' not found; pass --provider to create it on first contact"
                    ),
                    )
                }
            };
            // See handle_spawn: the daemon's own cwd is not the caller's, so
            // fall back to a neutral temp dir rather than its start dir, and
            // emit so a /tmp launch is greppable. A well-behaved client
            // forwards cwd (client.rs ensure_request_cwd).
            let spawn_cwd = match cwd_param {
                Some(c) => c,
                None => {
                    let fallback = std::env::temp_dir();
                    let _ = ctx.emitter.emit(
                        "agent_spawn_cwd_fallback",
                        &json!({
                            "name": name,
                            "fallback": fallback.to_string_lossy(),
                            "via": "ask_first_contact",
                        }),
                    );
                    fallback
                }
            };
            // Build a synthetic spawn request and delegate to handle_spawn.
            let mut spawn_params = serde_json::Map::new();
            spawn_params.insert("name".into(), serde_json::Value::String(name.clone()));
            spawn_params.insert("provider".into(), serde_json::Value::String(provider));
            spawn_params.insert(
                "cwd".into(),
                serde_json::Value::String(spawn_cwd.to_str().unwrap_or(".").to_string()),
            );
            spawn_params.insert("message".into(), serde_json::Value::String(message.clone()));
            if let Some(ref fn_val) = from_name_param {
                spawn_params.insert(
                    "from_name".into(),
                    serde_json::Value::String(fn_val.clone()),
                );
            }
            if yolo_param {
                spawn_params.insert("yolo".into(), serde_json::Value::Bool(true));
            }
            let spawn_req = Request::new(
                req.id,
                "agent.spawn",
                serde_json::Value::Object(spawn_params),
            );
            let spawn_resp = handle_spawn(ctx, &spawn_req).await;
            return match spawn_resp.payload {
                crate::protocol::ResponsePayload::Ok(ref result) => {
                    let short_id = result
                        .get("short_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    Response::ok(req.id, json!({"created": true, "short_id": short_id}))
                }
                crate::protocol::ResponsePayload::Err(_) => spawn_resp,
            };
        }
    };
    if entry.status == AgentStatus::Orphaned {
        return Response::err(
            req.id,
            ErrorCode::InvalidStatus,
            format!("agent {name} is orphaned; use `fno agents reconcile` or `rm`"),
        );
    }

    let sock = ctx.home.worker_sock(&entry.short_id);
    let mut conn = match UnixStream::connect(&sock).await {
        Ok(c) => c,
        Err(_) => {
            return Response::err(
                req.id,
                ErrorCode::InvalidStatus,
                format!("worker for {name} is not reachable"),
            )
        }
    };

    // Send the message to the PTY stdin. The provider envelope wrapping for the
    // non-Claude PTY paths is applied by the verb's full wiring (Wave 5/6); the
    // Wave 3 daemon forwards the raw line so the transport is exercised.
    let mut payload = message.clone();
    if !payload.ends_with('\n') {
        payload.push('\n');
    }
    if write_request(
        &mut conn,
        &Request::new(1, "worker.write", json!({"data": payload})),
    )
    .await
    .is_err()
    {
        return Response::err(req.id, ErrorCode::Internal, "worker write failed");
    }
    // Inspect the worker's write-ack: an error response (e.g. PTY writer fault)
    // must surface to the caller, not be reported as a successful ask with an
    // empty reply (silent-failure #4).
    match crate::protocol::read_response(&mut conn).await {
        Ok(ack) if ack.is_err() => {
            let msg = ack
                .error()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "worker rejected the write".into());
            return Response::err(req.id, ErrorCode::Internal, msg);
        }
        Ok(_) => {}
        Err(_) => {
            return Response::err(req.id, ErrorCode::Internal, "no write-ack from worker");
        }
    }

    // Poll the worker snapshot through the per-provider readiness detector until
    // the CLI is idle at a prompt (settled reply), then return it. This replaces
    // the Wave 3 fixed 150 ms snapshot baseline (Task 1.1).
    let timeout_secs = req
        .params
        .get("timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(600);
    let detector = provider_readiness_detector(&entry.provider);
    let sock_path = sock.clone();
    let fetcher = move || {
        let p = sock_path.clone();
        async move { read_worker_snapshot(&p).await }
    };
    let reply = match poll_until_ready(
        fetcher,
        detector,
        Duration::from_millis(200),
        Duration::from_secs(timeout_secs),
    )
    .await
    {
        Ok(text) => text,
        Err(e) => {
            return Response::err(req.id, ErrorCode::Internal, e.to_string());
        }
    };

    let ask_name = name.clone();
    let _ = update_registry_offloaded(ctx.home.registry_json(), move |r| {
        if let Some(e) = r.find_mut(&ask_name) {
            e.last_message_at = Some(now_rfc3339_like());
        }
    })
    .await;
    let _ = ctx
        .emitter
        .emit("agent_ask_done", &json!({"name": name, "backend": "pty"}));

    Response::ok(req.id, json!({"reply": reply, "backend": "pty"}))
}

/// Maximum body size (bytes) accepted on the switchboard inject path. Mirrors
/// `MAX_FRAME_BYTES` from the protocol layer; an oversized body would produce
/// a worker-write frame too large for the framing layer to accept.
const MAX_INJECT_BODY_BYTES: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// handle_switchboard (agent.switchboard RPC) — Group 2, Task 3.1
// ---------------------------------------------------------------------------
//
// The session-to-session switchboard: `send A->B` where B is a held stream-json
// thread. The daemon writes a user turn to B's stdin (B's `stream.write_turn`
// RPC), polls B's frames until a `result` closes the turn, and — when A is also
// a held stream-json thread and the caller asked to mirror (the A2A default;
// Task 4.1 gates it by config) — writes B's reply back into A as a literal user
// turn. The `--replay-user-messages` echo (a `user_echo` frame) is a delivery
// RECEIPT, never re-counted as the reply (Invariant "mirror reply exactly once").

/// Per-turn ceiling for a switchboard drive. The first `--resume` turn rehydrates
/// the transcript, so this default is generous; the daemon never hangs unbounded.
const SWITCHBOARD_TURN_TIMEOUT_MS: u64 = 120_000;
/// How often the switchboard polls B's frame log while a turn is in flight.
const SWITCHBOARD_POLL_MS: u64 = 50;
/// Bound for the liveness probe (connect + stream.ping). A wedged worker must
/// not hang the daemon on the probe.
const STREAM_PROBE_TIMEOUT_S: u64 = 2;
/// Bound for a fire-and-forget mirror write (connect + write_turn + ack).
const SWITCHBOARD_MIRROR_TIMEOUT_S: u64 = 5;
/// Grace added over the per-turn deadline for the OUTER bound on a drive, so a
/// hung connect / probe / write / read (none individually deadline-checked) can
/// never hang the daemon past the turn budget.
const SWITCHBOARD_DRIVE_GRACE_S: u64 = 5;

/// Outcome of driving one turn against a held stream-json thread.
struct SwitchboardTurn {
    /// Concatenated assistant text — the reply to mirror into the peer.
    reply: String,
    /// `result.is_error` — the turn closed in an error state.
    is_error: bool,
    /// A `user_echo` (`--replay-user-messages`) frame was observed: the turn was
    /// delivered to B's stdin and B began processing it.
    saw_receipt: bool,
}

/// Is the worker at `sock` a LIVE stream-json thread? Connects and sends a
/// `stream.ping`; `true` only when it answers ok. A non-stream worker (the PTY
/// lane serves `worker.*`, not `stream.*`) answers `UnknownMethod` -> `false`; a
/// session with no worker at all has no socket -> connect fails -> `false`. This
/// is the authoritative "held stream thread" test (no registry marking needed,
/// so it works before Group 3's front door stamps `host_mode`).
async fn is_live_stream_thread(sock: &std::path::Path) -> bool {
    // Bound the whole probe: a wedged / SIGSTOP'd worker must NOT hang the daemon
    // on connect or read (gemini-review HIGH). A timeout -> treat as not-live.
    let probe = async {
        let mut conn = UnixStream::connect(sock).await.ok()?;
        write_request(&mut conn, &Request::new(1, "stream.ping", json!({})))
            .await
            .ok()?;
        let resp = crate::protocol::read_response(&mut conn).await.ok()?;
        Some(!resp.is_err())
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(STREAM_PROBE_TIMEOUT_S), probe).await,
        Ok(Some(true))
    )
}

/// Write `text` into the held stream-json thread at `worker_sock` and poll frames
/// until a `result` closes the turn (or the child dies / the deadline elapses).
/// Discriminates the `user_echo` receipt from the assistant reply so the returned
/// `reply` is the assistant text exactly once (never the echo; the `result` text
/// is a fallback only when no assistant block carried text).
async fn drive_stream_turn(
    worker_sock: &std::path::Path,
    text: &str,
    deadline: Duration,
) -> Result<SwitchboardTurn, String> {
    let mut conn = UnixStream::connect(worker_sock)
        .await
        .map_err(|e| format!("target not live (worker unreachable): {e}"))?;

    // Snapshot the log END before writing. The worker's frame log is append-only
    // across the WHOLE session (stream_worker::FrameLog), so a resumed / multi-turn
    // thread already holds prior turns' `result` frames. Polling from 0 would match
    // an OLD result and return a stale reply (a reply B never gave for THIS turn).
    // `read_frames` clamps cursor.min(end), so cursor=u64::MAX yields the current
    // end with an empty slice; we then only observe frames THIS turn produces.
    write_request(
        &mut conn,
        &Request::new(0, "stream.read_frames", json!({ "cursor": u64::MAX })),
    )
    .await
    .map_err(|e| format!("cursor probe send failed: {e}"))?;
    let probe = crate::protocol::read_response(&mut conn)
        .await
        .map_err(|e| format!("cursor probe recv failed: {e}"))?;
    let mut cursor = probe
        .result()
        .and_then(|r| r.get("next"))
        .and_then(|v| v.as_u64())
        .ok_or_else(|| "cursor probe returned no result".to_string())?;

    // Write the turn; a rejected/failed write fails fast (Errors: broken pipe).
    write_request(
        &mut conn,
        &Request::new(1, "stream.write_turn", json!({ "text": text })),
    )
    .await
    .map_err(|e| format!("write_turn send failed: {e}"))?;
    match crate::protocol::read_response(&mut conn).await {
        Ok(ack) if ack.is_err() => {
            return Err(format!(
                "write_turn rejected: {}",
                ack.error().map(|e| e.message.as_str()).unwrap_or("?")
            ))
        }
        Ok(_) => {}
        Err(e) => return Err(format!("no write_turn ack: {e}")),
    }

    // Poll frames until a result closes the turn (starting at the pre-write end).
    let start = Instant::now();
    let mut reply = String::new();
    let mut saw_receipt = false;
    let mut req_id = 100u64;
    loop {
        if start.elapsed() > deadline {
            return Err("turn timed out before result".into());
        }
        write_request(
            &mut conn,
            &Request::new(req_id, "stream.read_frames", json!({ "cursor": cursor })),
        )
        .await
        .map_err(|e| format!("read_frames send failed: {e}"))?;
        req_id += 1;
        let resp = crate::protocol::read_response(&mut conn)
            .await
            .map_err(|e| format!("read_frames recv failed: {e}"))?;
        let res = resp
            .result()
            .ok_or_else(|| "read_frames returned no result".to_string())?;
        if let Some(next) = res.get("next").and_then(|v| v.as_u64()) {
            cursor = next;
        }
        let child_alive = res
            .get("child_alive")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if let Some(frames) = res.get("frames").and_then(|v| v.as_array()) {
            for fr in frames {
                match fr.get("kind").and_then(|k| k.as_str()) {
                    Some("user_echo") => saw_receipt = true,
                    Some("assistant") => {
                        if let Some(t) = fr.get("text").and_then(|t| t.as_str()) {
                            reply.push_str(t);
                        }
                    }
                    Some("result") => {
                        let is_error = fr
                            .get("is_error")
                            .and_then(|e| e.as_bool())
                            .unwrap_or(false);
                        // The result text is a FALLBACK only: a `result` must not
                        // double-count the assistant message already collected.
                        if reply.is_empty() {
                            if let Some(r) = fr.get("result").and_then(|r| r.as_str()) {
                                reply.push_str(r);
                            }
                        }
                        return Ok(SwitchboardTurn {
                            reply,
                            is_error,
                            saw_receipt,
                        });
                    }
                    // Malformed frames are already logged at the worker; skip.
                    _ => {}
                }
            }
        }
        if !child_alive {
            return Err("target child exited before result (orphaned)".into());
        }
        tokio::time::sleep(Duration::from_millis(SWITCHBOARD_POLL_MS)).await;
    }
}

/// Mirror `text` into the held stream-json thread at `worker_sock` as one user
/// turn (fire-and-forget: we do not wait for the peer's reply here — the
/// autonomous A<->B relay + ceiling is Task 4.1). Returns the worker's ack error
/// as `Err` so the caller can report a half-mirror rather than hide it.
async fn mirror_into(worker_sock: &std::path::Path, text: &str) -> Result<(), String> {
    let inner = async {
        let mut conn = UnixStream::connect(worker_sock)
            .await
            .map_err(|e| format!("mirror target unreachable: {e}"))?;
        write_request(
            &mut conn,
            &Request::new(1, "stream.write_turn", json!({ "text": text })),
        )
        .await
        .map_err(|e| format!("mirror write failed: {e}"))?;
        match crate::protocol::read_response(&mut conn).await {
            Ok(ack) if ack.is_err() => Err(format!(
                "mirror rejected: {}",
                ack.error().map(|e| e.message.as_str()).unwrap_or("?")
            )),
            Ok(_) => Ok(()),
            Err(e) => Err(format!("no mirror ack: {e}")),
        }
    };
    // Bound the whole mirror so a wedged peer cannot hang the daemon.
    match tokio::time::timeout(Duration::from_secs(SWITCHBOARD_MIRROR_TIMEOUT_S), inner).await {
        Ok(r) => r,
        Err(_) => Err("mirror timed out".into()),
    }
}

/// Best-effort flip a registry row to `Orphaned` (the worker is gone and did not
/// self-stamp, e.g. it was SIGKILLed rather than hitting EOF). Offloaded so the
/// async runtime is not blocked on file I/O; failures are swallowed (the
/// reconcile sweep is the backstop).
async fn stamp_orphaned(home: &AgentsHome, short_id: &str) {
    let reg = home.registry_json();
    let sid = short_id.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = state::update_registry(&reg, |r| {
            if let Some(e) = r.entries.iter_mut().find(|e| e.short_id == sid) {
                // Only flip a still-Live row. Do NOT clobber a terminal status
                // the worker already set (a clean `Exited` from stream.shutdown,
                // or `Failed`): clobbering Exited->Orphaned would make a
                // deliberately-stopped session look adoptable (stream_worker.rs
                // documents this hazard).
                if e.status == AgentStatus::Live {
                    e.status = AgentStatus::Orphaned;
                }
            }
        });
    })
    .await;
}

/// Handle the `agent.switchboard` RPC (Group 2, Task 3.1).
///
/// Params: `{to: string, from: string, body: string, mirror?: bool,
/// timeout_ms?: u64}`.
///
/// Result (Ok unless `to` is unknown or params invalid):
/// - `{delivered: true, reply, is_error, mirrored, receipt, transport:
///   "switchboard"}` — the turn was driven against B and (when `mirror` and A is
///   a held stream thread) B's reply was written into A.
/// - `{delivered: false, reason: "not-a-live-stream-thread"}` — B is not a held
///   stream-json thread; the caller demotes to the durable/socket path.
/// - `{delivered: false, reason: "<drive error>"}` — B was a stream thread but
///   the turn failed (broken pipe / orphaned / timeout); B is stamped orphaned
///   and A is NOT touched (the exchange did not complete).
///
/// Errors: `AgentNotFound` (unknown `to`), `InvalidParams` (missing/oversized).
async fn handle_switchboard(ctx: &Ctx, req: &Request) -> Response {
    let to = match req.params.get("to").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return Response::err(req.id, ErrorCode::InvalidParams, "missing `to`"),
    };
    let from = req
        .params
        .get("from")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let body = match req.params.get("body").and_then(|v| v.as_str()) {
        Some(b) => b.to_string(),
        None => return Response::err(req.id, ErrorCode::InvalidParams, "missing `body`"),
    };
    let mirror = req
        .params
        .get("mirror")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let timeout_ms = req
        .params
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .unwrap_or(SWITCHBOARD_TURN_TIMEOUT_MS);

    if body.len() > MAX_INJECT_BODY_BYTES {
        return Response::err(
            req.id,
            ErrorCode::InvalidParams,
            format!(
                "body too large: {} bytes > {MAX_INJECT_BODY_BYTES}",
                body.len()
            ),
        );
    }

    let registry = load_registry_offloaded(ctx.home.registry_json()).await;
    let to_entry = match registry.find(&to) {
        Some(e) => e.clone(),
        None => {
            return Response::err(
                req.id,
                ErrorCode::AgentNotFound,
                format!("agent '{to}' not found"),
            )
        }
    };

    // B must be a held stream-json thread. A non-claude peer (PTY lane) or a
    // claude session with no live stream worker demotes to the durable path.
    let to_sock = ctx.home.worker_sock(&to_entry.short_id);
    if to_entry.provider != "claude" || !is_live_stream_thread(&to_sock).await {
        return Response::ok(
            req.id,
            json!({"delivered": false, "reason": "not-a-live-stream-thread"}),
        );
    }

    // Drive the turn against B. The OUTER timeout (turn budget + grace) is the
    // backstop: drive_stream_turn checks its deadline only at the poll-loop top,
    // so a hung connect / probe / write / read inside it is bounded here, never
    // hanging the daemon (gemini-review HIGH).
    let drive_deadline = Duration::from_millis(timeout_ms);
    let outer = drive_deadline + Duration::from_secs(SWITCHBOARD_DRIVE_GRACE_S);
    let drive_result =
        match tokio::time::timeout(outer, drive_stream_turn(&to_sock, &body, drive_deadline)).await
        {
            Ok(inner) => inner,
            Err(_) => Err("drive hung past the turn budget (timed out)".to_string()),
        };
    let outcome = match drive_result {
        Ok(o) => o,
        Err(reason) => {
            // B was a stream thread but the turn failed: the child is gone or the
            // pipe broke. Stamp B orphaned (AC2-ERR) and do NOT touch A — the
            // exchange did not complete, so A must not show a reply B never gave.
            stamp_orphaned(&ctx.home, &to_entry.short_id).await;
            let _ = ctx.emitter.emit(
                "agent_deliver_demoted",
                &json!({
                    "name": to,
                    "from_name": from,
                    "provider": "claude",
                    "transport": "switchboard",
                    "reason": reason,
                }),
            );
            return Response::ok(req.id, json!({"delivered": false, "reason": reason}));
        }
    };

    // Mirror B's reply into A when asked AND A is itself a held stream thread.
    // A one-way drive (A absent / not a stream thread) still counts as delivered.
    // Never mirror a self-send (from == to): it would queue B's own reply back
    // into B as a spurious extra turn.
    let mut mirrored = false;
    if mirror && from != to {
        // Re-load the registry: driving B can take up to the turn budget (~120s),
        // during which A may have been restarted with a new short_id. The pre-turn
        // snapshot could point at A's old socket (gemini-review HIGH).
        let fresh = load_registry_offloaded(ctx.home.registry_json()).await;
        if let Some(from_entry) = fresh.find(&from) {
            let from_sock = ctx.home.worker_sock(&from_entry.short_id);
            if from_entry.provider == "claude" && is_live_stream_thread(&from_sock).await {
                match mirror_into(&from_sock, &outcome.reply).await {
                    Ok(()) => mirrored = true,
                    Err(e) => {
                        // The turn completed but the mirror failed: surface it
                        // (the reply is still returned for the caller to record),
                        // never silently drop it.
                        let _ = ctx.emitter.emit(
                            "agent_deliver_demoted",
                            &json!({
                                "name": from,
                                "from_name": to,
                                "provider": "claude",
                                "transport": "switchboard-mirror",
                                "reason": e,
                            }),
                        );
                    }
                }
            }
        }
    }

    let _ = ctx.emitter.emit(
        "agent_deliver_injected",
        &json!({
            "name": to,
            "from_name": from,
            "provider": "claude",
            "transport": "switchboard",
            "mirrored": mirrored,
            "is_error": outcome.is_error,
        }),
    );

    Response::ok(
        req.id,
        json!({
            "delivered": true,
            "transport": "switchboard",
            "reply": outcome.reply,
            "is_error": outcome.is_error,
            "mirrored": mirrored,
            "receipt": outcome.saw_receipt,
        }),
    )
}

async fn read_worker_snapshot(sock: &std::path::Path) -> Option<String> {
    let mut conn = UnixStream::connect(sock).await.ok()?;
    write_request(&mut conn, &Request::new(2, "worker.snapshot", json!({})))
        .await
        .ok()?;
    let resp = crate::protocol::read_response(&mut conn).await.ok()?;
    resp.result()
        .and_then(|r| r.get("text").and_then(|t| t.as_str()).map(String::from))
}

/// Non-blocking reap of any exited worker child the daemon spawned, so a worker
/// that exits while the daemon lives never lingers as a `<defunct>` zombie. The
/// daemon spawns nothing but workers, so a `waitpid(-1, WNOHANG)` sweep is safe.
fn reap_zombies() {
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: waitpid with WNOHANG only reaps already-exited children and
        // returns 0 (none ready) or -1 (no children) without blocking.
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid <= 0 {
            break;
        }
    }
}

fn handle_list(ctx: &Ctx, req: &Request) -> Response {
    let all = req
        .params
        .get("all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Task 3.1: accept cwd/provider/status filters matching Python list_agents.
    // Legacy project_root filter still accepted for backward compat.
    let filter_cwd = req
        .params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(String::from);
    let filter_provider = req
        .params
        .get("provider")
        .and_then(|v| v.as_str())
        .map(String::from);
    let filter_status = req
        .params
        .get("status")
        .and_then(|v| v.as_str())
        .map(String::from);
    let cwd_project = req
        .params
        .get("project_root")
        .and_then(|v| v.as_str())
        .map(String::from);

    // Reject an invalid --status up front so a typo fails fast with exit 13
    // instead of silently returning zero rows + exit 0 (Codex P2 on PR #361).
    // Mirrors Python's AgentStatusFilter enum (live | orphaned), which Typer
    // rejects at parse time.
    if let Some(ref st) = filter_status {
        if st != "live" && st != "orphaned" {
            return Response::err(
                req.id,
                ErrorCode::InvalidStatus,
                format!("invalid --status '{st}' (expected: live | orphaned)"),
            );
        }
    }
    // Normalize the cwd filter so equivalent paths (`.` vs absolute, symlinks)
    // match, mirroring Python's `Path(cwd).resolve()` before filtering (Codex P2
    // on PR #361; this is the cwd half of cv-eeaad75d). canonicalize requires the
    // path to exist; fall back to the raw string when it can't resolve so a
    // non-existent filter still does an exact-string match rather than erroring.
    let norm_path = |p: &str| -> String {
        std::fs::canonicalize(p)
            .ok()
            .and_then(|pb| pb.to_str().map(String::from))
            .unwrap_or_else(|| p.to_string())
    };
    let filter_cwd_norm = filter_cwd.as_deref().map(&norm_path);

    let registry = state::load_registry(&ctx.home.registry_json()).unwrap_or_default();
    let entries: Vec<Value> = registry
        .entries
        .iter()
        .filter(|e| {
            // Legacy all/project_root filter
            if !all {
                if let Some(ref p) = cwd_project {
                    if &e.project_root != p {
                        return false;
                    }
                }
            }
            // Task 3.1 filters: cwd, provider, status (matching Python list_agents order)
            if let Some(ref cwd) = filter_cwd_norm {
                if &norm_path(&e.cwd) != cwd {
                    return false;
                }
            }
            if let Some(ref prov) = filter_provider {
                if &e.provider != prov {
                    return false;
                }
            }
            if let Some(ref st) = filter_status {
                if format!("{:?}", e.status).to_lowercase() != st.to_lowercase() {
                    return false;
                }
            }
            true
        })
        .map(|e| {
            // Task 3.1: return the full serialize_entry 10-key shape matching Python.
            // Fields present in RegistryEntry are mapped directly; fields absent from
            // the Rust registry are emitted as null with a NOTE citing the carveout.
            //
            // DECIDED cv-eeaad75d: live_status is always null. The daemon does NOT
            // replicate Python list's per-row `claude agents --json` live_status
            // augmentation, and will not: under the replace architecture (the
            // daemon is the sole agents backend; cv-d28b266a, docs/distribution.md)
            // PTY-worker/registry `status` IS the canonical liveness signal. A
            // `claude agents` shellout would both contradict that and be
            // impossible to do consistently here (the daemon does not PTY-manage
            // claude; ClaudeProvider.as_pty() is None). The field is kept (null)
            // to preserve the serialize_entry parity shape for JSON consumers.
            //
            // session_id: Python uses the provider-specific resume id (short_id
            // for claude since v9, codex_session_id for codex, gemini_session_id
            // for gemini). The Rust registry stores these in separate optional
            // fields; we replicate the Python resolution logic here.
            // Provider-specific resume id, falling back to the generic
            // `session_id` when the provider field is None (matches Python's
            // resolution + the resolve_session_id helper below; gemini-code-assist
            // medium on PR #361 — without the fallback a row with only the generic
            // session_id set would report null here).
            let resume_id: Option<String> = match e.provider.as_str() {
                "claude" => e
                    .transport_short()
                    .map(str::to_string)
                    .or_else(|| e.session_id.clone()),
                "codex" => e.codex_session_id.clone().or_else(|| e.session_id.clone()),
                "gemini" => e.gemini_session_id.clone().or_else(|| e.session_id.clone()),
                _ => e.session_id.clone(),
            };
            let session_id: Value = resume_id.map(Value::String).unwrap_or(Value::Null);
            let short_id: Value = e
                .transport_short()
                .map(|s| Value::String(s.to_string()))
                .unwrap_or(Value::Null);
            let log_path: Value = e
                .log_path
                .as_deref()
                .map(|s| Value::String(s.to_string()))
                .unwrap_or(Value::Null);
            json!({
                "name": e.name,
                "provider": e.provider,
                "short_id": short_id,
                "session_id": session_id,
                "cwd": e.cwd,
                "created_at": e.created_at,
                "last_message_at": e.last_message_at,
                "status": format!("{:?}", e.status).to_lowercase(),
                "live_status": null,  // DECIDED cv-eeaad75d: not replicated (see NOTE above)
                // Architecture C (plan ab-70faa65b): additive keys, never removing
                // live_status (Locked #4 back-compat). `pid` is the worker pid for
                // a PTY agent, null for a one-shot ask (no managed process). The
                // pid is cleared when a PTY row reconciles to exited (Locked #7),
                // so it never lingers as a misleading liveness signal.
                // `last_reconciled_at` is the raw RFC3339 of the last probe (null
                // when never reconciled); the client renders it as the CHECKED age.
                "pid": e.pid,
                "last_reconciled_at": e.last_reconciled_at,
                "log_path": log_path,
                // Superset of Python's serialize_entry: project_root is retained
                // as the daemon's native grouping key (existing daemon_e2e
                // contract) alongside the 10 Python parity fields. Python list
                // has no project_root; the extra key is a harmless superset.
                "project_root": e.project_root,
            })
        })
        .collect();
    // Echo the filters the daemon applied so `list --json` self-describes its
    // query, matching Python `read.list_agents`'s `filters_applied` (sigma-review:
    // the client previously always fell back to an all-null block because the
    // daemon omitted this field). `cwd` is the value the client sent; absolute
    // resolution to match Python's `Path(cwd).resolve()` is deferred (cv-eeaad75d).
    let filters_applied = json!({
        "cwd": filter_cwd_norm,
        "provider": filter_provider,
        "status": filter_status,
    });
    Response::ok(
        req.id,
        json!({"agents": entries, "filters_applied": filters_applied}),
    )
}

/// Daemon diagnostics in the locked `status-v1.json` shape (US6.10, LD35):
///
/// ```json
/// {
///   "schema_version": 1,
///   "daemon":   {"state", "pid", "uptime_secs", "version",
///                "exe_path", "exe_mtime", "exe_size", "pid_start_time"},
///   "agents":   {"total", "by_status": {"<status>": <count>, ...}},
///   "drives":   {"active": <controlling-driver count>},
///   "restarts": {"queue_depth", "consecutive_failures_max_seen"},
///   "channels": {"registered": <entries with an mcp_channel_id>}
/// }
/// ```
///
/// The shape is the contract Wave 7's `status-v1.json` schema + CI parity check
/// codify; keep additions backward-compatible. `daemon.state` is always
/// `serving` here because a served RPC implies the daemon got past recovery.
async fn handle_status(ctx: &Ctx, req: &Request) -> Response {
    // load_registry does blocking flock I/O; offload it from the async worker
    // thread (Gemini review). The drive-table read below stays async.
    let reg_path = ctx.home.registry_json();
    let registry = tokio::task::spawn_blocking(move || state::load_registry(&reg_path))
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let mut by_status: Map<String, Value> = Map::new();
    let mut restarting: u64 = 0;
    let mut channels_registered: u64 = 0;
    for e in &registry.entries {
        let key = format!("{:?}", e.status).to_lowercase();
        let n = by_status.get(&key).and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        by_status.insert(key, Value::Number(n.into()));
        if e.status == AgentStatus::Restarting {
            restarting += 1;
        }
        if e.mcp_channel_id.is_some() {
            channels_registered += 1;
        }
    }
    Response::ok(
        req.id,
        json!({
            "schema_version": 1,
            "daemon": {
                "state": DaemonState::Serving.as_str(),
                "pid": std::process::id(),
                "uptime_secs": ctx.started_at.elapsed().as_secs(),
                "version": env!("CARGO_PKG_VERSION"),
                // Drift signal (ab-1891cdff), additive. Null when the daemon
                // could not fingerprint itself; a client then reads Unknown.
                "exe_path": ctx
                    .exe_fingerprint
                    .as_ref()
                    .map(|f| f.path.to_string_lossy().into_owned()),
                "exe_mtime": ctx.exe_fingerprint.as_ref().map(|f| f.mtime_nanos),
                "exe_size": ctx.exe_fingerprint.as_ref().map(|f| f.size),
                // The daemon's own process start time, for the `restart`
                // pid-reuse guard.
                "pid_start_time": ctx.pid_start_time,
            },
            "agents": {
                "total": registry.entries.len(),
                "by_status": by_status,
            },
            "restarts": {
                // queue_depth tracks agents currently restarting; the full
                // restart queue + consecutive-failure history is not yet
                // surfaced in the served status (Wave 5), so the max-seen
                // counter reports 0 until that subsystem is wired into Ctx.
                "queue_depth": restarting,
                "consecutive_failures_max_seen": 0,
            },
            "channels": { "registered": channels_registered },
        }),
    )
}

/// Resolve a lifecycle token (name | 8-hex short | full session id) to the
/// canonical registry name via the shared resolver (x-1b1e), so the daemon
/// `stop`/`rm` handlers accept all three address forms like their Python
/// counterparts (`_canonical_agent_name`) instead of matching on name alone.
/// Falls back to the raw token on any miss (unknown/ambiguous/serialize error)
/// so the caller's familiar `agent {name} not found` path still fires.
fn canonical_name_in(registry: &state::Registry, token: &str) -> String {
    let Ok(Value::Array(rows)) = serde_json::to_value(&registry.entries) else {
        return token.to_string();
    };
    match crate::client_verbs::find_agent_entry(&rows, token) {
        Ok(e) => e
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(token)
            .to_string(),
        Err(_) => token.to_string(),
    }
}

async fn handle_stop(ctx: &Ctx, req: &Request) -> Response {
    let name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return Response::err(req.id, ErrorCode::InvalidParams, "missing `name`"),
    };
    let registry = load_registry_offloaded(ctx.home.registry_json()).await;
    let name = canonical_name_in(&registry, &name);
    let entry = match registry.find(&name) {
        Some(e) => e.clone(),
        None => {
            return Response::err(
                req.id,
                ErrorCode::AgentNotFound,
                format!("agent {name} not found"),
            )
        }
    };
    if entry.status == AgentStatus::Exited {
        // An exited agent needs no stop work. (Pre-G4 this also force-cleared a
        // lingering WebSocket driver; the drive surface was retired at G4.)
        return Response::ok(
            req.id,
            json!({"already_exited": true, "short_id": entry.short_id}),
        );
    }
    // Claude agents are not PTY-managed (LD8): there is no worker to shut down.
    // Shell out to the claude supervisor and propagate its outcome.
    if entry.provider == "claude" {
        return stop_claude(ctx, req, &name, &entry).await;
    }
    // A non-PTY row (empty short_id == Python-authored; the daemon's create path
    // always derives a non-empty short_id) for codex/gemini has no daemon worker
    // to stop. Mirror Python `stop_agent`: these providers are "synchronous
    // between asks (no persistent process to stop)" -- emit `agent_stopped` and
    // return cleanly, leaving the registry UNCHANGED. Falling through to the PTY
    // path would probe the agents-root `worker.sock` (absent -> "confirmed
    // down") and then write `status = Exited`, a status Python's loader rejects,
    // corrupting a Python-readable registry (Codex P1, PR #364).
    if entry.short_id.is_empty() {
        let _ = ctx.emitter.emit(
            "agent_stopped",
            &json!({"name": name, "provider": entry.provider, "claude_exit": Value::Null}),
        );
        return Response::ok(
            req.id,
            json!({"stopped": true, "provider": entry.provider, "no_op": true}),
        );
    }
    // Ask the worker to shut down its PTY child gracefully, then CONFIRM it
    // actually went away before reporting success: a swallowed shutdown
    // failure would mark the agent exited while the PTY keeps running (Codex
    // P1). A worker that shut down removes its socket and exits.
    if !stop_worker_confirmed(ctx, &entry).await {
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!("agent {name}: worker did not confirm shutdown; it may still be running"),
        );
    }
    // Surface a registry-write failure rather than reporting a clean stop while
    // the on-disk status still reads live: the worker is confirmed dead, but if
    // the status flip does not persist the registry diverges from reality
    // (silent-failure review). Mirrors handle_register_channel's house style.
    let stop_name = name.clone();
    if let Err(e) = update_registry_offloaded(ctx.home.registry_json(), move |r| {
        if let Some(e) = r.find_mut(&stop_name) {
            e.status = AgentStatus::Exited;
        }
    })
    .await
    {
        let _ = ctx.emitter.emit(
            "agent_stop_error",
            &json!({"name": name, "error": e.to_string()}),
        );
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!("agent {name}: worker stopped but registry write failed: {e}"),
        );
    }
    let _ = ctx.emitter.emit("agent_stopped", &json!({"name": name}));
    Response::ok(req.id, json!({"stopped": true, "short_id": entry.short_id}))
}

/// Fire-and-forget `worker.shutdown` to a worker that must not be left running
/// (a spawn that failed or lost a name race): connect, ask it to tear down, and
/// move on. Best-effort by design — the caller is already on an error path.
async fn best_effort_worker_shutdown(sock: &std::path::Path) {
    if let Ok(mut conn) = UnixStream::connect(sock).await {
        let _ = write_request(&mut conn, &Request::new(1, "worker.shutdown", json!({}))).await;
        let _ = crate::protocol::read_response(&mut conn).await;
    }
}

/// Graceful worker shutdown with SIGTERM -> SIGKILL escalation (US6.7), then
/// verify the worker process is actually gone. Returns true iff the worker is
/// confirmed down. A worker that never dies returns false so the caller can
/// refuse to claim a clean stop (a swallowed failure would mark the agent exited
/// while its PTY keeps running, Codex P1).
async fn stop_worker_confirmed(ctx: &Ctx, entry: &RegistryEntry) -> bool {
    let sock = ctx.home.worker_sock(&entry.short_id);
    // 1. Graceful: ask the worker to tear down its PTY child + exit.
    if let Ok(mut conn) = UnixStream::connect(&sock).await {
        let _ = write_request(&mut conn, &Request::new(1, "worker.shutdown", json!({}))).await;
        let _ = crate::protocol::read_response(&mut conn).await;
    }
    // 2. Up to the 5s grace for a clean exit. "Down" = the worker's SOCKET is
    //    unreachable, which is the authoritative, PID-reuse-immune liveness
    //    signal: the worker is identified by the socket it owns, not by a
    //    registry pid that can go stale after a crash (Codex P1).
    let mut down = worker_down_within(&sock, Duration::from_secs(5)).await;
    // 3. Escalate ONLY while the socket is still reachable, i.e. a worker is
    //    alive and ignoring shutdown. If the socket is already unreachable we
    //    are done and never signal a pid - this avoids SIGKILLing a stale or
    //    recycled pid when the real worker has already exited (Codex P1).
    //    Additionally, validate pid+create_time ownership before signaling
    //    (ab-d19e6458): if the recorded pid is alive but its start time no longer
    //    matches, the pid was recycled by an unrelated process and we must NOT
    //    SIGTERM/SIGKILL it. The socket-reachable worker (a restarted instance
    //    under a new pid) is left for the caller to report as not-confirmed.
    if !down {
        if let Some(pid) = entry.pid {
            if pid_is_ours(pid, entry.pid_start_time) {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
                down = worker_down_within(&sock, Duration::from_secs(5)).await;
                if !down && pid_is_ours(pid, entry.pid_start_time) {
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGKILL);
                    }
                    down = worker_down_within(&sock, Duration::from_secs(2)).await;
                }
            }
        }
    }
    // Only reap the socket file once the worker is confirmed unreachable, so we
    // never unlink a live worker's socket (Codex P1). A SIGKILLed worker cannot
    // remove its own socket; this reaps the stale file so a later reconcile /
    // list does not mistake it for a live worker.
    if down {
        let _ = std::fs::remove_file(&sock);
    }
    down
}

/// Probe whether the worker is still serving on its socket. PID-reuse-immune:
/// the worker is identified by the socket it owns (per `short_id`), so a
/// recycled unrelated pid never answers here (Codex P1).
async fn worker_socket_reachable(sock: &std::path::Path) -> bool {
    UnixStream::connect(sock).await.is_ok()
}

/// Poll until the worker's socket is unreachable (the worker is gone), or
/// `budget` elapses. Socket-based rather than pid-based so a stale / recycled
/// `entry.pid` can neither falsely report a live worker down nor cause a live
/// worker's socket to be unlinked (Codex P1).
async fn worker_down_within(sock: &std::path::Path, budget: Duration) -> bool {
    let start = Instant::now();
    loop {
        if !worker_socket_reachable(sock).await {
            return true;
        }
        if start.elapsed() >= budget {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Stop a Claude agent (AC7-EDGE). Claude is shellout-managed (LD8): there is no
/// worker PTY to signal, so the daemon shells out to the claude supervisor's
/// `stop` on the agent's short id and marks the registry row exited on success.
async fn stop_claude(ctx: &Ctx, req: &Request, name: &str, entry: &RegistryEntry) -> Response {
    let short = match entry
        .transport_short()
        .or(entry.session_id.as_deref())
        .filter(|s| !s.is_empty())
    {
        Some(s) => s.to_string(),
        None => {
            return Response::err(
                req.id,
                ErrorCode::InvalidStatus,
                format!("agent {name} is claude but has no short id to stop"),
            )
        }
    };
    match tokio::process::Command::new("claude")
        .arg("stop")
        .arg(&short)
        .output()
        .await
    {
        Ok(o) if o.status.success() => {
            // Surface a persist failure rather than reporting a clean stop while
            // the registry still reads live (silent-failure review).
            let claude_name = name.to_string();
            if let Err(e) = update_registry_offloaded(ctx.home.registry_json(), move |r| {
                if let Some(e) = r.find_mut(&claude_name) {
                    e.status = AgentStatus::Exited;
                }
            })
            .await
            {
                return Response::err(
                    req.id,
                    ErrorCode::Internal,
                    format!("claude {name} stopped but registry write failed: {e}"),
                );
            }
            let _ = ctx
                .emitter
                .emit("agent_stopped", &json!({"name": name, "backend": "claude"}));
            // Report the id we actually stopped with (`short`), not
            // `entry.short_id`: a row with only a generic session_id and an empty
            // short_id would otherwise print `stopped: <name> ()` and break the
            // stop output
            // contract for exactly the rows ab-e5a57efa makes readable (Codex P2).
            Response::ok(
                req.id,
                json!({"stopped": true, "backend": "claude", "short_id": short}),
            )
        }
        Ok(o) => Response::err(
            req.id,
            ErrorCode::Internal,
            format!(
                "claude stop {short} failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
        ),
        Err(e) => Response::err(
            req.id,
            ErrorCode::Internal,
            format!("could not exec `claude stop`: {e}"),
        ),
    }
}

async fn handle_rm(ctx: &Ctx, req: &Request) -> Response {
    let name = match req.params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return Response::err(req.id, ErrorCode::InvalidParams, "missing `name`"),
    };
    let force = req
        .params
        .get("force")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let registry = load_registry_offloaded(ctx.home.registry_json()).await;
    let name = canonical_name_in(&registry, &name);
    let entry = match registry.find(&name) {
        Some(e) => e.clone(),
        None => {
            return Response::err(
                req.id,
                ErrorCode::AgentNotFound,
                format!("agent {name} not found"),
            )
        }
    };
    if entry.status == AgentStatus::Live && !force {
        return Response::err(
            req.id,
            ErrorCode::Busy,
            format!("agent {name} is still live; use `stop` first or pass --force"),
        );
    }
    // Force-removing a live agent must stop its worker first, or it leaks a PTY
    // process that `list`/`stop` can no longer address by name (Codex P2).
    if entry.status == AgentStatus::Live && force && !stop_worker_confirmed(ctx, &entry).await {
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!("agent {name}: could not stop the worker before force-remove; refusing to orphan a live PTY"),
        );
    }
    // Orphaned entries are removed with no subprocess action (AC8-FR); the
    // distinction is surfaced in the event for the operator's audit trail.
    let was_orphaned = entry.status == AgentStatus::Orphaned;
    // Surface a removal-write failure rather than reporting removed:true while
    // the entry still persists (silent-failure review): a force-rm has already
    // killed the worker, so a swallowed write leaves a dangling row pointing at
    // a dead worker.
    let rm_name = name.clone();
    if let Err(e) = update_registry_offloaded(ctx.home.registry_json(), move |r| {
        r.entries.retain(|e| e.name != rm_name);
    })
    .await
    {
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!("agent {name}: removal did not persist: {e}"),
        );
    }
    let _ = ctx.emitter.emit(
        "agent_removed",
        &json!({"name": name, "was_orphaned": was_orphaned}),
    );
    Response::ok(
        req.id,
        json!({"removed": true, "was_orphaned": was_orphaned}),
    )
}

/// `reachability` per-call timeout (LD30): a single provider probe is bounded.
const RECONCILE_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
/// Total reconcile sweep budget (LD30): beyond it, remaining agents defer to the
/// next tick so a large registry never blocks the daemon for long.
const RECONCILE_SWEEP_BUDGET: Duration = Duration::from_secs(5);

/// A status change reconcile decided for one probed entry. `new_status: None`
/// means "probed, status unchanged" — its `last_reconciled_at` is still bumped
/// so the fairness ordering rotates.
struct ReconcileChange {
    name: String,
    new_status: Option<AgentStatus>,
}

/// What a reconcile sweep did, for the `reconcile_done` event and tests.
#[derive(Default, PartialEq, Debug)]
struct ReconcileOutcome {
    updated: Vec<String>,
    orphans: Vec<String>,
    recovered: Vec<String>,
    /// `(name, reason)` for entries whose probe was inconclusive (status
    /// preserved, never flipped).
    inconsistent: Vec<(String, String)>,
    /// Count of trailing entries not probed because the budget elapsed.
    deferred: usize,
}

/// Plan a reconcile sweep over `entries` (which the caller has ordered ASC by
/// `last_reconciled_at` for fairness). Pure of clock and I/O: `probe` answers
/// reachability tri-state per entry and `budget_exhausted` reports whether the
/// sweep budget has elapsed — both injected so the budget/fairness/tri-state
/// logic is deterministically unit-testable (the daemon wires the real provider
/// probe + a wall-clock deadline).
///
/// Transition rules (status-aware, design AC9):
/// - `Ok(true)` (reachable): recover an `Orphaned` entry to `Live`; leave any
///   other status (live-ish or terminal) unchanged.
/// - `Ok(false)` (unreachable): flip a live-ish entry to `Orphaned`; leave an
///   already-`Orphaned` or terminal (`Exited`/`PermanentDead`) entry unchanged.
/// - `Err` (inconclusive): preserve status, record an inconsistency. Never
///   orphan on a probe timeout (Failure Modes / Errors invariant).
fn plan_reconcile<P, D, L>(
    entries: &[RegistryEntry],
    mut probe: P,
    mut budget_exhausted: D,
    mut pid_live: L,
) -> (Vec<ReconcileChange>, ReconcileOutcome)
where
    P: FnMut(&RegistryEntry) -> Result<bool, crate::provider::ReachabilityProbeError>,
    D: FnMut() -> bool,
    L: FnMut(&RegistryEntry) -> bool,
{
    let mut changes = Vec::new();
    let mut out = ReconcileOutcome::default();
    for (i, entry) in entries.iter().enumerate() {
        if budget_exhausted() {
            out.deferred = entries.len() - i;
            break;
        }
        // A one-shot `ask` agent has no daemon-managed process, so its liveness is
        // decided by process-liveness alone (it has none): terminal `exited`.
        // Session-file reachability answers "resumable?" (surfaced via session_id),
        // never "running?" -- so a surviving session file must NOT keep an ask row
        // `live`. This is the actual cause of the reported stale-`live` rows: the
        // `probe` is skipped entirely here, so no provider reachability call can
        // decide an ask row's status. An already-terminal ask is left untouched.
        // [plan ab-70faa65b, Locked Decision #1]
        if entry.is_one_shot_ask() {
            let new_status = if is_non_terminal(entry.status) {
                out.updated.push(entry.name.clone());
                Some(AgentStatus::Exited)
            } else {
                None
            };
            changes.push(ReconcileChange {
                name: entry.name.clone(),
                new_status,
            });
            continue;
        }
        let new_status = match probe(entry) {
            Ok(true) => {
                if entry.status == AgentStatus::Orphaned {
                    out.recovered.push(entry.name.clone());
                    out.updated.push(entry.name.clone());
                    Some(AgentStatus::Live)
                } else {
                    None
                }
            }
            Ok(false) if entry.is_interactive() => {
                // host_mode=interactive (task 2.3 / US4): an interactive host is a
                // long-lived drivable TUI whose liveness is its PTY *process*, not
                // session-store membership. A live `codex resume`/`gemini -r` TUI
                // may not appear in the exec session index, so a store miss must
                // NOT orphan a healthy worker. But a genuinely dead interactive
                // worker must still be reaped DURING `reconcile` -- not only at
                // daemon restart via recover()'s pid sweep -- so check pid
                // liveness here and flip to Exited when the worker process is gone
                // ("unexpected exit is exited, not orphaned"; Codex P2, PR #373).
                if pid_live(entry) {
                    None
                } else {
                    out.updated.push(entry.name.clone());
                    Some(AgentStatus::Exited)
                }
            }
            Ok(false) => {
                // Only states that *should* have a live backend can go stale.
                // Restarting / Failed are intentionally excluded: the restart
                // supervisor owns those agents' lifecycle (backoff -> re-spawn
                // or permanent_dead), so reconcile must not race it by flipping
                // a mid-restart agent to orphaned. Terminal states (Exited /
                // PermanentDead) are likewise left alone.
                let live_ish = matches!(
                    entry.status,
                    AgentStatus::Live
                        | AgentStatus::Ready
                        | AgentStatus::Idle
                        | AgentStatus::Busy
                        | AgentStatus::Spawning
                );
                if live_ish {
                    out.orphans.push(entry.name.clone());
                    out.updated.push(entry.name.clone());
                    Some(AgentStatus::Orphaned)
                } else {
                    None
                }
            }
            Err(e) => {
                out.inconsistent
                    .push((entry.name.clone(), e.reason.clone()));
                None
            }
        };
        changes.push(ReconcileChange {
            name: entry.name.clone(),
            new_status,
        });
    }
    (changes, out)
}

/// Apply one planned reconcile change to its registry row. Always freshens
/// `last_reconciled_at` (the probe was *attempted*, so `CHECKED` rotates even on
/// an inconclusive/no-change probe). On a status change, sets the new status and
/// -- when it is terminal `Exited` -- nulls `pid`/`pid_start_time` so `list`/
/// `--json` never surfaces a pid that no longer belongs to the agent (Locked
/// Decision #7: a stale pid is exactly the misleading liveness signal this work
/// removes; forensics live in the event log, not a dangling registry pid). The
/// pid is cleared only on `Exited` (the lone terminal status reconcile produces)
/// -- an `Orphaned` row keeps its pid, which is still the live-but-unowned
/// process an operator may want to `ps`/signal while investigating the orphan.
fn apply_reconcile_change(e: &mut RegistryEntry, new_status: Option<AgentStatus>, now: &str) {
    e.last_reconciled_at = Some(now.to_string());
    if let Some(s) = new_status {
        e.status = s;
        if matches!(s, AgentStatus::Exited) {
            e.pid = None;
            e.pid_start_time = None;
            // Ordered exit teardown (E3.3, AC-X2-4): clear the inside-leg
            // authority on exit so a stale `working` never wins after the pane
            // is gone. The completion event is published by the caller BEFORE
            // this write (publish completion -> clear authority). A scraped
            // verdict dies with the pane for the same reason.
            e.inside_leg = None;
            e.screen_state = None;
        }
    }
}

/// Publish one inside-leg completion event for a row that is about to be marked
/// `Exited` (ordered exit teardown, E3.3 / AC-X2-4). Emitted BEFORE the registry
/// write clears [`RegistryEntry::inside_leg`], so `fno agents list` / waiters
/// observe the final state before the badge goes blank. A no-op for a row with
/// no report (a normal exit, nothing to tear down).
fn emit_inside_leg_completion(emitter: &EventEmitter, e: &RegistryEntry) {
    if let Some(rep) = &e.inside_leg {
        let _ = emitter.emit(
            "inside_leg_completed",
            &json!({
                "name": e.name,
                "session_id": e.session_id,
                "final_state": inside_leg_state_str(rep.state),
                "seq": rep.seq,
            }),
        );
    }
}

/// The lowercase wire label for an inside-leg state (matches herdr's
/// `report_agent` vocabulary). Allocation-free; the single source for the three
/// daemon-emitted inside-leg events.
fn inside_leg_state_str(state: state::InsideLegState) -> &'static str {
    match state {
        state::InsideLegState::Working => "working",
        state::InsideLegState::Blocked => "blocked",
        state::InsideLegState::Done => "done",
    }
}

/// Build the lean provider-probe projection from a registry row, preferring the
/// provider-specific session id over the generic one.
fn to_agent_entry(e: &RegistryEntry) -> crate::provider::AgentEntry {
    let session_id = match e.provider.as_str() {
        "codex" => e.codex_session_id.clone().or_else(|| e.session_id.clone()),
        "gemini" => e.gemini_session_id.clone().or_else(|| e.session_id.clone()),
        "claude" => e
            .transport_short()
            .map(str::to_string)
            .or_else(|| e.session_id.clone()),
        _ => e.session_id.clone(),
    };
    crate::provider::AgentEntry {
        name: e.name.clone(),
        provider: e.provider.clone(),
        session_id,
        cwd: PathBuf::from(&e.cwd),
    }
}

/// Everything the `reconcile` RPC needs to render its response, returned by
/// [`run_reconcile_sweep`] so the bounded sweep core is shared with the daemon's
/// startup pass (Architecture B, plan ab-70faa65b).
struct ReconcileSweepResult {
    /// Registry snapshot read at sweep start (per-name provider lookup).
    registry: crate::state::Registry,
    /// Entries in fairness order (ASC `last_reconciled_at`), as probed.
    entries: Vec<RegistryEntry>,
    outcome: ReconcileOutcome,
}

/// Run ONE bounded reconcile sweep and persist it: probe each agent
/// least-recently-reconciled-first (250ms/probe, 5s total budget), settle status
/// by process-liveness (Architecture A), then batch-write every change + freshen
/// `last_reconciled_at` under one registry lock. Emits the same
/// `agent_inconsistent` / `reconcile_deferred` / `reconcile_done` events as
/// before. Returns the snapshot + outcome on success, or an error string when
/// the registry write fails (the registry is then unchanged, so callers degrade
/// to serving last-recorded status rather than reporting a sweep that did not
/// apply -- Codex P1). Shared by the `reconcile` RPC and the startup sweep.
fn run_reconcile_sweep(
    home: &AgentsHome,
    emitter: &EventEmitter,
) -> Result<ReconcileSweepResult, String> {
    use crate::provider::ReachabilityProbeError;
    let registry = state::load_registry(&home.registry_json()).unwrap_or_default();

    // Fairness: probe least-recently-reconciled first (None < Some), so a
    // budget-exhausted sweep eventually covers every entry (finding #1).
    let mut entries = registry.entries.clone();
    entries.sort_by(|a, b| a.last_reconciled_at.cmp(&b.last_reconciled_at));

    let start = Instant::now();
    let probe = |e: &RegistryEntry| -> Result<bool, ReachabilityProbeError> {
        // Fast path: a reachable worker socket is authoritative, PID-reuse-immune
        // liveness for a PTY-managed agent — no provider probe (and no 250ms
        // cost) needed. A sync connect is fine: reconcile runs on the blocking
        // pool (Codex P1: do not trust a possibly-stale registry pid).
        if std::os::unix::net::UnixStream::connect(home.worker_sock(&e.short_id)).is_ok() {
            return Ok(true);
        }
        // No live worker: ask the provider's session store (tri-state).
        match crate::provider::for_name(&e.provider) {
            Some(p) => p.reachability(&to_agent_entry(e), RECONCILE_PROBE_TIMEOUT),
            None => Err(ReachabilityProbeError::new(
                &e.provider,
                "unknown provider; cannot probe reachability",
            )),
        }
    };
    // pid-liveness for interactive hosts (Codex P2): a row with a recorded pid
    // that is no longer OUR live worker is a dead interactive host to reap to
    // Exited. A row with no pid is left alone (mirrors recover()'s sweep, which
    // only acts on entries that carry a pid).
    let pid_live = |e: &RegistryEntry| -> bool {
        e.pid.map_or(true, |pid| pid_is_ours(pid, e.pid_start_time))
    };
    let (changes, outcome) = plan_reconcile(
        &entries,
        probe,
        || start.elapsed() >= RECONCILE_SWEEP_BUDGET,
        pid_live,
    );

    // Ordered exit teardown (E3.3, AC-X2-4): for every row transitioning to
    // Exited that still carries an inside-leg report, publish its completion
    // BEFORE the write below clears the report. Publishing first is the
    // contract: list/waiters see the final state before the badge goes blank.
    for ch in &changes {
        if matches!(ch.new_status, Some(AgentStatus::Exited)) {
            if let Some(e) = registry.entries.iter().find(|e| e.name == ch.name) {
                emit_inside_leg_completion(emitter, e);
            }
        }
    }

    // Single batched write (US4-gemini pattern): apply all status changes and
    // bump last_reconciled_at for every probed entry in one lock window.
    let now = now_rfc3339_like();
    // Surface a persistence failure rather than emitting reconcile_done and
    // returning updated/orphans/recovered as if the sweep applied (Codex P1): on
    // a lock/IO failure the registry is unchanged, so reporting success would
    // mislead automation and hide stale lifecycle state.
    if let Err(err) = state::update_registry(&home.registry_json(), |r| {
        for ch in &changes {
            if let Some(e) = r.find_mut(&ch.name) {
                apply_reconcile_change(e, ch.new_status, &now);
            }
        }
    }) {
        let _ = emitter.emit("reconcile_error", &json!({"error": err.to_string()}));
        return Err(format!(
            "reconcile computed {} change(s) but the registry write failed: {err}",
            changes.len()
        ));
    }

    for (name, reason) in &outcome.inconsistent {
        let _ = emitter.emit(
            "agent_inconsistent",
            &json!({"name": name, "reason": reason}),
        );
    }
    if outcome.deferred > 0 {
        let _ = emitter.emit(
            "reconcile_deferred",
            &json!({"remaining_count": outcome.deferred}),
        );
    }
    let _ = emitter.emit(
        "reconcile_done",
        &json!({
            "updated": outcome.updated.len(),
            "orphans": outcome.orphans.len(),
            "recovered": outcome.recovered.len(),
        }),
    );
    Ok(ReconcileSweepResult {
        registry,
        entries,
        outcome,
    })
}

fn handle_reconcile(ctx: &Ctx, req: &Request) -> Response {
    let ReconcileSweepResult {
        registry,
        entries,
        outcome,
    } = match run_reconcile_sweep(&ctx.home, &ctx.emitter) {
        Ok(r) => r,
        Err(msg) => return Response::err(req.id, ErrorCode::Internal, msg),
    };
    // Task 3.1: emit the Python ReconcileResult JSON shape so the Rust client
    // can render --json output matching Python's cmd_reconcile contract:
    //   scanned, orphaned[], recovered[], skipped[], errors[]
    //
    // Mapping from internal outcome fields:
    //   scanned = total entries (matches Python `scanned=len(entries)`)
    //   orphaned = outcome.orphans wrapped as [{name, provider}] dicts
    //   recovered = outcome.recovered wrapped as [{name, provider}] dicts
    //   skipped = deferred entries, wrapped as [{name, provider}] dicts
    //   errors = inconsistent probes wrapped as [{name, reason}] dicts
    //
    // Legacy fields (updated, orphans, inconsistent, deferred) are preserved for
    // backward compat with any existing callers reading the raw daemon response.
    //
    // Python reports `scanned=len(entries)` (all entries, including the deferred
    // tail) and `skipped` as a separate list of the deferred entries; skipped is
    // a subset of scanned, not subtracted from it. The daemon previously reported
    // `scanned = entries - deferred`, a count-only divergence (cv-5b1a4164).
    let scanned = entries.len();
    // plan_reconcile probes the (least-recently-reconciled-first) sorted entries
    // in order and defers the tail when the sweep budget is exhausted, so the
    // deferred entries are exactly entries[probed..]. `probed` is the boundary,
    // distinct from the reported `scanned` count above (gemini-code-assist medium
    // on PR #361; closes carveout cv-5b1a4164's skipped half).
    let probed = entries.len() - outcome.deferred;
    let skipped_py: Vec<Value> = entries
        .iter()
        .skip(probed)
        .map(|e| json!({"name": e.name, "provider": e.provider}))
        .collect();
    let orphaned_py: Vec<Value> = outcome
        .orphans
        .iter()
        .map(|n| {
            let prov = registry
                .entries
                .iter()
                .find(|e| &e.name == n)
                .map(|e| e.provider.as_str())
                .unwrap_or("unknown");
            json!({"name": n, "provider": prov})
        })
        .collect();
    let recovered_py: Vec<Value> = outcome
        .recovered
        .iter()
        .map(|n| {
            let prov = registry
                .entries
                .iter()
                .find(|e| &e.name == n)
                .map(|e| e.provider.as_str())
                .unwrap_or("unknown");
            json!({"name": n, "provider": prov})
        })
        .collect();
    let errors_py: Vec<Value> = outcome
        .inconsistent
        .iter()
        .map(|(n, reason)| json!({"name": n, "reason": reason}))
        .collect();
    Response::ok(
        req.id,
        json!({
            // Python-matching keys (Task 3.1 parity contract)
            "scanned": scanned,
            "orphaned": orphaned_py,
            "recovered": recovered_py,
            "skipped": skipped_py,
            "errors": errors_py,
            // Legacy internal keys (backward compat)
            "updated": outcome.updated,
            "orphans": outcome.orphans,
            "inconsistent": outcome.inconsistent.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            "deferred": outcome.deferred,
        }),
    )
}

/// `agent.report` — the inside-leg state push (inside-out E3.2). A per-turn hook
/// calls `fno agents report --session-id <uuid> --seq <n> --state
/// working|blocked|done [--reason ...] [--ttl-ms <n>]`; the daemon stamps
/// `received_at` and STORES the report on the matching registry row's
/// [`RegistryEntry::inside_leg`] field (contract v2 / X2). Storage-only: the
/// seq-drop (a `seq <= last_seq` is rejected so a reordered/duplicate report
/// cannot clobber a newer one, AC-X2-1) and the unknown-session drop (no phantom
/// row, AC-X2-5) live here; TTL-aging, the 3-tier render authority, and the
/// ordered exit teardown are E3.3. The row is matched by the daemon-pinned
/// session id via [`entry_holds_session`], so a claude pane reports under the
/// same UUID E1 recorded. A DROP is non-fatal: an unregistered session (the row
/// not up yet) or a stale seq returns `ok` with `stored:false`, so the hook stays
/// fire-and-forget and never reds a turn.
/// Outcome of trying to buffer an early-push inside-leg report (E3.3).
enum BufferOutcome {
    /// Held in the pending buffer until the row registers.
    Buffered,
    /// A reordered/duplicate early push (`seq <= buffered seq`); dropped.
    StaleSeq { last: u64 },
    /// The buffer is at cap and this is a new session; dropped (logged).
    Full,
}

/// Insert an early-push report into the bounded pending buffer, highest-seq-wins
/// per session (a reorder cannot regress a buffered report, the same seq rule the
/// registered path enforces). Pure over the map so it is unit-testable without a
/// daemon (inside-out E3.3, buffer-on-early-push).
fn buffer_pending_report(
    map: &mut std::collections::HashMap<String, state::InsideLegReport>,
    session_id: &str,
    report: state::InsideLegReport,
) -> BufferOutcome {
    if let Some(prev) = map.get(session_id) {
        if report.seq <= prev.seq {
            return BufferOutcome::StaleSeq { last: prev.seq };
        }
        map.insert(session_id.to_string(), report);
        return BufferOutcome::Buffered;
    }
    if map.len() >= PENDING_INSIDE_LEG_CAP {
        return BufferOutcome::Full;
    }
    map.insert(session_id.to_string(), report);
    BufferOutcome::Buffered
}

/// Flush a buffered early-push report onto its session's row AFTER the row is
/// registered (E3.3 flush).
///
/// Called only on a winning insert with the row's pinned claude session uuid.
/// Takes the buffered report out of the pending map (highest-seq, since
/// `buffer_pending_report` keeps only the newest) and applies it to the row
/// under a seq gate, so a report that raced in on the row's *store* path between
/// insert and this drain is never regressed (codex P2: highest-seq-wins must
/// survive the flush). Draining strictly after the insert closes the
/// peek-then-commit window where a newer buffered report could be deleted by an
/// unconditional remove. A no-op for a row with no buffered report; a poisoned
/// lock leaves the report buffered.
fn flush_buffered_inside_leg(ctx: &Ctx, session_uuid: &str, name: &str) {
    let rep = match ctx.pending_inside_leg.lock() {
        Ok(mut buf) => buf.remove(session_uuid),
        Err(_) => None,
    };
    let Some(rep) = rep else {
        return;
    };
    let (seq, state_str) = (rep.seq, inside_leg_state_str(rep.state));
    // Badge-transition notify intent (x-dd84): an early-push report is the row's
    // first, so an initial `blocked`/`done` is an episode entry too. Captured
    // before `rep` moves into the row; fired after the write.
    let (rep_state, rep_reason) = (rep.state, rep.reason.clone());
    let mut notify: Option<(String, String, bool)> = None;
    // Apply under the seq gate: a store-path report that landed on the row after
    // it became visible (but before this drain) set a >= seq; never regress it.
    let _ = state::update_registry(&ctx.home.registry_json(), |r| {
        if let Some(e) = r
            .entries
            .iter_mut()
            .find(|e| entry_holds_session(e, session_uuid))
        {
            let newer = e.inside_leg.as_ref().is_none_or(|cur| rep.seq > cur.seq);
            if newer {
                let prev_state = e.inside_leg.as_ref().map(|r| r.state);
                let body = rep_reason.clone().unwrap_or_else(|| state_str.to_string());
                if state::enters(prev_state, rep_state, state::InsideLegState::Blocked) {
                    notify = Some((name.to_string(), body, false));
                } else if state::enters(prev_state, rep_state, state::InsideLegState::Done) {
                    notify = Some((name.to_string(), body, true));
                }
                e.inside_leg = Some(rep);
                // Capability flip (see handle_report): hook beats scrape.
                e.screen_state = None;
            }
        }
    });
    if let Some((title, body, is_done)) = notify {
        let want = if is_done {
            ctx.opts.notify_on_done
        } else {
            ctx.opts.notify_on_blocked
        };
        if want {
            notify_transition(title, body);
        }
    }
    let _ = ctx.emitter.emit(
        "inside_leg_buffer_flushed",
        &json!({"name": name, "session_id": session_uuid, "state": state_str, "seq": seq}),
    );
}

/// Fire a fire-and-forget OS notification for a badge transition (x-dd84).
///
/// Detached to its own thread so a missing or slow `fno notify` can never stall
/// the registry write that observed the transition - the same bounded/fail-open
/// discipline as the external claim-status writer that once froze admit
/// (memory project_grid_rail_drive_freeze). `FNO_BIN` selects the binary
/// (default `fno`); a spawn failure (notifier not on PATH) logs one warn and is
/// dropped, and the registry write that called this has already succeeded.
pub(crate) fn notify_transition(title: String, body: String) {
    // var_os (not var) so a non-UTF-8 FNO_BIN passes through to Command
    // unmangled, matching scrape::fno_bin (gemini MEDIUM on #161).
    let fno = std::env::var_os("FNO_BIN").unwrap_or_else(|| std::ffi::OsString::from("fno"));
    // ponytail: reap on the detached thread; `fno notify` is a sub-second
    // osascript/notify-send call, so waiting on it here cannot realistically leak.
    std::thread::spawn(move || {
        match std::process::Command::new(&fno)
            .args(["notify", &title, &body])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                let _ = child.wait();
            }
            Err(e) => eprintln!(
                "fno-agents-daemon: badge notify skipped ({} notify): {e}",
                fno.to_string_lossy()
            ),
        }
    });
}

/// Which null-uuid row (if any) should adopt a full session uuid seen on an
/// inside-leg report (x-c393).
enum UuidBackfill {
    None,
    One(usize),
    Ambiguous,
}

/// Find the `claude --bg` row awaiting its full session uuid. A bg spawn writes
/// the row with the 8-hex jobId in `short_id` (v9) but `claude_session_uuid:
/// null` -- the full uuid only arrives on the first inside-leg report, so until
/// it is backfilled `entry_holds_session` never matches and every report is
/// buffered-then-lost (x-c393). Match a null-uuid claude row whose short-id is
/// the leading hex group of `full_uuid` (`3228ccad` -> `3228ccad-c078-...`).
/// Two rows sharing that short-id is ambiguous -> refuse rather than backfill
/// the wrong row (AC1-ERR).
fn find_uuid_backfill_row(entries: &[RegistryEntry], full_uuid: &str) -> UuidBackfill {
    let mut found = None;
    for (i, e) in entries.iter().enumerate() {
        // Only a claude bg row owns a jobId + uuid identity; skip any other
        // provider so a malformed foreign row can't adopt a claude uuid.
        if e.provider != "claude" || e.claude_session_uuid.is_some() {
            continue;
        }
        let Some(short) = e.transport_short() else {
            continue;
        };
        // Require the group boundary (`<short>-`) so a short cannot match a
        // longer hex run it merely prefixes.
        if short.is_empty()
            || !full_uuid
                .strip_prefix(short)
                .is_some_and(|rest| rest.starts_with('-'))
        {
            continue;
        }
        if found.is_some() {
            return UuidBackfill::Ambiguous;
        }
        found = Some(i);
    }
    found.map_or(UuidBackfill::None, UuidBackfill::One)
}

fn handle_report(ctx: &Ctx, req: &Request) -> Response {
    let session_id = match req.params.get("session_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return Response::err(req.id, ErrorCode::InvalidParams, "missing `session_id`"),
    };
    let seq = match req.params.get("seq").and_then(|v| v.as_u64()) {
        Some(n) => n,
        None => {
            return Response::err(
                req.id,
                ErrorCode::InvalidParams,
                "missing or non-integer `seq`",
            )
        }
    };
    // Validate against the wire vocabulary; keep the label for the event payload
    // and map to the typed enum for storage.
    let state_label = match req.params.get("state").and_then(|v| v.as_str()) {
        Some(s @ ("working" | "blocked" | "done")) => s.to_string(),
        _ => {
            return Response::err(
                req.id,
                ErrorCode::InvalidParams,
                "`state` must be working|blocked|done",
            )
        }
    };
    let state = match state_label.as_str() {
        "working" => state::InsideLegState::Working,
        "blocked" => state::InsideLegState::Blocked,
        _ => state::InsideLegState::Done,
    };
    let reason = req
        .params
        .get("reason")
        .and_then(|v| v.as_str())
        .map(String::from);
    let ttl_ms = req.params.get("ttl_ms").and_then(|v| v.as_u64());

    // Build the report once; a clone moves into the locked store path, the
    // original is reused for the early-push buffer when no row exists yet.
    let report = state::InsideLegReport {
        state,
        seq,
        reason,
        received_at: now_rfc3339_like(),
        ttl_ms,
    };
    let report_for_store = report.clone();

    // The store/drop decision is made UNDER the registry flock so two concurrent
    // reporters on one session id can't both pass the seq gate.
    enum Outcome {
        Stored,
        StaleSeq { last: u64 },
        Unknown,
    }
    let mut outcome = Outcome::Unknown;
    // Badge-transition notify intent (x-dd84): (title, body, is_done). Captured
    // UNDER the flock from prev-vs-new state; fired AFTER the write so a slow
    // notifier can never stall ingestion.
    let mut notify: Option<(String, String, bool)> = None;
    if let Err(e) = state::update_registry(&ctx.home.registry_json(), |r| {
        // Match by the pinned session id (fast path). If nothing holds it, a
        // `claude --bg` row may still be waiting for its uuid: backfill it by
        // short-id prefix so the report can store on it AND ask/mail/push route
        // to it (x-c393). Ambiguous prefix -> no backfill (AC1-ERR).
        let idx = match r
            .entries
            .iter()
            .position(|e| entry_holds_session(e, &session_id))
        {
            Some(i) => Some(i),
            None => match find_uuid_backfill_row(&r.entries, &session_id) {
                UuidBackfill::One(i) => {
                    r.entries[i].claude_session_uuid = Some(session_id.clone());
                    Some(i)
                }
                UuidBackfill::None | UuidBackfill::Ambiguous => None,
            },
        };
        let Some(idx) = idx else {
            outcome = Outcome::Unknown;
            return;
        };
        let entry = &mut r.entries[idx];
        if let Some(prev) = &entry.inside_leg {
            if seq <= prev.seq {
                outcome = Outcome::StaleSeq { last: prev.seq };
                return;
            }
        }
        let prev_state = entry.inside_leg.as_ref().map(|r| r.state);
        if state::enters(prev_state, state, state::InsideLegState::Blocked) {
            let body = report_for_store
                .reason
                .clone()
                .unwrap_or_else(|| state_label.clone());
            notify = Some((entry.name.clone(), body, false));
        } else if state::enters(prev_state, state, state::InsideLegState::Done) {
            let body = report_for_store
                .reason
                .clone()
                .unwrap_or_else(|| state_label.clone());
            notify = Some((entry.name.clone(), body, true));
        }
        entry.inside_leg = Some(report_for_store);
        // Capability flip: the hook now owns this row's signal; a stale
        // scrape verdict must never shadow it (per-capability arbitration).
        entry.screen_state = None;
        outcome = Outcome::Stored;
    }) {
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!("registry write failed during inside-leg report: {e}"),
        );
    }

    match outcome {
        Outcome::Stored => {
            let _ = ctx.emitter.emit(
                "inside_leg_report",
                &json!({"session_id": session_id, "seq": seq, "state": state_label}),
            );
            if let Some((title, body, is_done)) = notify {
                let want = if is_done {
                    ctx.opts.notify_on_done
                } else {
                    ctx.opts.notify_on_blocked
                };
                if want {
                    notify_transition(title, body);
                }
            }
            Response::ok(req.id, json!({"stored": true, "seq": seq}))
        }
        Outcome::StaleSeq { last } => {
            let _ = ctx.emitter.emit(
                "inside_leg_report_dropped",
                &json!({"session_id": session_id, "seq": seq, "last_seq": last, "reason": "stale_seq"}),
            );
            Response::ok(
                req.id,
                json!({"stored": false, "dropped": "stale_seq", "last_seq": last}),
            )
        }
        // E3.3 buffer-on-early-push: the row is not up yet (the hook fired before
        // the daemon registered the pane). Hold the report in the bounded buffer
        // instead of dropping it; the spawn path flushes it onto the row at
        // creation. Still fire-and-forget: every branch returns `ok`. The lock is
        // scoped to the buffer op (released before the emit) via `.map(..).ok()`;
        // a poisoned lock -> `None` -> the old hard-drop degrade.
        Outcome::Unknown => {
            let buffered = ctx
                .pending_inside_leg
                .lock()
                .map(|mut buf| buffer_pending_report(&mut buf, &session_id, report))
                .ok();
            match buffered {
                Some(BufferOutcome::Buffered) => {
                    let _ = ctx.emitter.emit(
                        "inside_leg_report_buffered",
                        &json!({"session_id": session_id, "seq": seq, "state": state_label}),
                    );
                    Response::ok(
                        req.id,
                        json!({"stored": false, "buffered": true, "seq": seq}),
                    )
                }
                Some(BufferOutcome::StaleSeq { last }) => {
                    let _ = ctx.emitter.emit(
                        "inside_leg_report_dropped",
                        &json!({"session_id": session_id, "seq": seq, "last_seq": last, "reason": "stale_seq"}),
                    );
                    Response::ok(
                        req.id,
                        json!({"stored": false, "dropped": "stale_seq", "last_seq": last}),
                    )
                }
                Some(BufferOutcome::Full) => {
                    let _ = ctx.emitter.emit(
                        "inside_leg_report_dropped",
                        &json!({"session_id": session_id, "seq": seq, "reason": "buffer_full"}),
                    );
                    Response::ok(req.id, json!({"stored": false, "dropped": "buffer_full"}))
                }
                // Poisoned buffer lock: degrade to the old hard-drop rather than
                // panicking a fire-and-forget hook.
                None => {
                    let _ = ctx.emitter.emit(
                        "inside_leg_report_dropped",
                        &json!({"session_id": session_id, "seq": seq, "reason": "unknown_session"}),
                    );
                    Response::ok(
                        req.id,
                        json!({"stored": false, "dropped": "unknown_session"}),
                    )
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// channel.* (Phase 5 integration point; minimal Wave 3 surface).
// ---------------------------------------------------------------------------

async fn dispatch_channel(ctx: &Arc<Ctx>, req: &Request) -> Response {
    // All channel handlers are pure flock + CPU; run on the blocking pool.
    match Namespace::verb(&req.method) {
        Some("register_channel") => run_blocking(ctx, req, handle_register_channel).await,
        Some("unregister_channel") => run_blocking(ctx, req, handle_unregister_channel).await,
        Some("push_to_channel") => run_blocking(ctx, req, handle_push_to_channel).await,
        _ => Response::err(
            req.id,
            ErrorCode::UnknownMethod,
            format!("unknown channel verb in `{}`", req.method),
        ),
    }
}

fn handle_register_channel(ctx: &Ctx, req: &Request) -> Response {
    let cc_session_id = match req.params.get("cc_session_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return Response::err(req.id, ErrorCode::InvalidParams, "missing `cc_session_id`"),
    };
    // Resolve the target agent: by name if given, else by matching cc_session_id.
    let name = req
        .params
        .get("name")
        .and_then(|v| v.as_str())
        .map(String::from);
    let channel_id = uuid_v4();
    let mut matched = false;
    // Surface a persist failure: without this, `matched` could be set in the
    // closure and the handler would return a successful mcp_channel_id even
    // though the mapping never hit disk, causing immediate routing drift
    // (Codex P1).
    if let Err(e) = state::update_registry(&ctx.home.registry_json(), |r| {
        let target = match &name {
            Some(n) => r.find_mut(n),
            None => r
                .entries
                .iter_mut()
                .find(|e| e.cc_session_id.as_deref() == Some(&cc_session_id)),
        };
        if let Some(e) = target {
            e.cc_session_id = Some(cc_session_id.clone());
            e.mcp_channel_id = Some(channel_id.clone());
            matched = true;
        }
    }) {
        return Response::err(
            req.id,
            ErrorCode::Internal,
            format!("registry write failed during channel registration: {e}"),
        );
    }
    if !matched {
        return Response::err(
            req.id,
            ErrorCode::ChannelUnknown,
            "no agent matched cc_session_id/name for registration",
        );
    }
    let _ = ctx
        .emitter
        .emit("channel_registered", &json!({"mcp_channel_id": channel_id}));
    Response::ok(req.id, json!({"mcp_channel_id": channel_id}))
}

fn handle_unregister_channel(ctx: &Ctx, req: &Request) -> Response {
    let channel_id = match req.params.get("mcp_channel_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return Response::err(req.id, ErrorCode::InvalidParams, "missing `mcp_channel_id`"),
    };
    let mut cleared = false;
    let _ = state::update_registry(&ctx.home.registry_json(), |r| {
        for e in r.entries.iter_mut() {
            if e.mcp_channel_id.as_deref() == Some(&channel_id) {
                e.mcp_channel_id = None;
                cleared = true;
            }
        }
    });
    if !cleared {
        return Response::err(req.id, ErrorCode::ChannelUnknown, "unknown channel id");
    }
    Response::ok(req.id, json!({"unregistered": true}))
}

fn handle_push_to_channel(ctx: &Ctx, req: &Request) -> Response {
    let channel_id = match req.params.get("mcp_channel_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return Response::err(req.id, ErrorCode::InvalidParams, "missing `mcp_channel_id`"),
    };
    let registry = state::load_registry(&ctx.home.registry_json()).unwrap_or_default();
    let found = registry
        .entries
        .iter()
        .any(|e| e.mcp_channel_id.as_deref() == Some(&channel_id));
    if !found {
        return Response::err(
            req.id,
            ErrorCode::ChannelUnknown,
            "channel id not registered (channel server should re-register)",
        );
    }
    // Routing the poke to the CC session's child pipe is the channel server's
    // job; the daemon confirms the route exists.
    Response::ok(req.id, json!({"routed": true}))
}

// ---------------------------------------------------------------------------
// Small helpers.
// ---------------------------------------------------------------------------

fn json_obj(pairs: &[(&str, Value)]) -> Map<String, Value> {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

/// Compact UTC timestamp for filesystem names (`20260524T023300Z`).
fn now_compact() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// RFC3339-like timestamp for the registry's `created_at` / `last_message_at`.
pub(crate) fn now_rfc3339_like() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    );
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, hh, mm, ss)
}

/// Generate a RFC 4122 v4 UUID from OS randomness (`getentropy`/urandom via
/// libc). No `uuid` crate dependency; the daemon needs exactly one generator.
fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    fill_random(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
        b[14], b[15]
    )
}

fn fill_random(buf: &mut [u8]) {
    // Read from /dev/urandom; if unavailable, fall back to a time+pid mix (the
    // mcp_channel_id uniqueness invariant tolerates this degraded path because
    // collisions across one daemon's lifetime are astronomically unlikely).
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        if f.read_exact(buf).is_ok() {
            return;
        }
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
        ^ (std::process::id() as u64).rotate_left(17);
    let mut x = seed | 1;
    for byte in buf.iter_mut() {
        // xorshift64
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *byte = (x & 0xff) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AgentState, DriveWindow, PtyState};

    fn tmp_home(tag: &str) -> AgentsHome {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fno-agents-daemon-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let home = AgentsHome::at(&p);
        home.ensure_root().unwrap();
        home
    }

    fn read_events(home: &AgentsHome) -> Vec<Value> {
        std::fs::read_to_string(home.events_jsonl())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    }

    // One-shot ask row (empty short_id + no pid): terminal, reapable on grace
    // alone (owns no worktree). `exited_at` controls the grace clock.
    fn ask_row(name: &str, exited_at: Option<&str>) -> RegistryEntry {
        RegistryEntry {
            name: name.into(),
            short_id: String::new(),
            provider: "claude".into(),
            harness: None,
            harness_session_id: None,
            cwd: "/tmp".into(),
            project_root: String::new(),
            session_id: None,
            legacy_claude_short_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: None,
            gemini_session_id: None,
            mcp_channel_id: None,
            cc_session_id: None,
            host_mode: None,
            status: AgentStatus::Exited,
            last_message_at: None,
            created_at: "2020-01-01T00:00:00Z".into(),
            pid: None,
            pid_start_time: None,
            log_path: None,
            last_reconciled_at: None,
            inside_leg: None,
            exited_at: exited_at.map(str::to_string),
            mux: None,
            screen_state: None,
        }
    }

    #[test]
    fn gc_sweep_reaps_stamped_stamps_unstamped_keeps_live() {
        let home = tmp_home("gc-sweep");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

        state::update_registry(&home.registry_json(), |r| {
            // Stamped long ago -> past grace -> reaped (AC1-HP; ask row skips the
            // worktree probe).
            r.entries
                .push(ask_row("ask-old", Some("2020-01-01T00:00:00Z")));
            // Terminal but never observed dead before -> stamped, not reaped.
            r.entries.push(ask_row("ask-new", None));
            // A live worker (our own pid, no start time -> bare-existence live) is
            // never touched (AC1-FR).
            let mut live = ask_row("live", None);
            live.name = "live".into();
            live.short_id = "wkL".into();
            live.status = AgentStatus::Live;
            live.pid = Some(std::process::id());
            r.entries.push(live);
        })
        .unwrap();

        let summary = gc_sweep(&home, &emitter, Duration::from_secs(3600));

        assert_eq!(summary.reaped, vec!["ask-old".to_string()]);

        let reg = state::load_registry(&home.registry_json()).unwrap();
        let names: Vec<&str> = reg.entries.iter().map(|e| e.name.as_str()).collect();
        assert!(!names.contains(&"ask-old"), "ask-old should be reaped");
        assert!(
            names.contains(&"ask-new"),
            "ask-new should be kept (in grace)"
        );
        assert!(names.contains(&"live"), "live row must never be reaped");

        // ask-new got its exit stamp; the live row stayed unstamped.
        let new = reg.entries.iter().find(|e| e.name == "ask-new").unwrap();
        assert!(
            new.exited_at.is_some(),
            "ask-new should be stamped this pass"
        );
        let live = reg.entries.iter().find(|e| e.name == "live").unwrap();
        assert!(live.exited_at.is_none());

        // The removal emitted exactly one agent_row_reaped for ask-old.
        let events = read_events(&home);
        let reaped: Vec<&Value> = events
            .iter()
            .filter(|e| e.get("type").and_then(Value::as_str) == Some("agent_row_reaped"))
            .collect();
        assert_eq!(reaped.len(), 1);
        assert_eq!(
            reaped[0]
                .get("data")
                .and_then(|d| d.get("name"))
                .and_then(Value::as_str),
            Some("ask-old")
        );
    }

    #[test]
    fn gc_sweep_empty_registry_is_noop() {
        let home = tmp_home("gc-empty");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let summary = gc_sweep(&home, &emitter, Duration::from_secs(3600));
        assert!(summary.reaped.is_empty());
        assert!(summary.kept_dirty.is_empty());
    }

    #[test]
    fn recovery_emits_drive_crashed_before_clearing_window() {
        let home = tmp_home("recover-drive");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

        // Registry entry + state.json with a stale active drive window.
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(RegistryEntry {
                name: "worker-A".into(),
                short_id: "wkA".into(),
                provider: "codex".into(),
                harness: None,
                harness_session_id: None,
                cwd: "/tmp".into(),
                project_root: "/tmp".into(),
                session_id: None,
                legacy_claude_short_id: None,
                claude_session_uuid: None,
                messaging_socket_path: None,
                codex_session_id: None,
                gemini_session_id: None,
                mcp_channel_id: None,
                cc_session_id: None,
                host_mode: None,
                status: AgentStatus::Live,
                last_message_at: None,
                created_at: "2026-05-24T00:00:00Z".into(),
                pid: Some(std::process::id()), // alive -> not reaped
                pid_start_time: None,
                log_path: None,
                last_reconciled_at: None,
                inside_leg: None,
                exited_at: None,
                mux: None,
                screen_state: None,
            });
        })
        .unwrap();
        let mut st = AgentState::new_pty("wkA");
        st.status = AgentStatus::Live;
        st.pty = Some(PtyState {
            active: true,
            drive: Some(DriveWindow {
                session_id: Some("drive-xyz".into()),
                mode: Some("interactive".into()),
                last_heartbeat_at_monotonic_ns: Some(123),
            }),
        });
        state::write_state_atomic(&home.state_json("wkA"), &st).unwrap();

        let report = recover(&home, &emitter);
        assert_eq!(report.recovered_drives, vec!["wkA".to_string()]);

        // drive_crashed emitted, carrying the session id (proves read-before-clear).
        let events = read_events(&home);
        let crashed = events
            .iter()
            .find(|e| e["type"] == "drive_crashed")
            .expect("drive_crashed emitted");
        assert_eq!(crashed["data"]["session_id"], "drive-xyz");
        assert_eq!(crashed["data"]["reason"], "daemon_restart");

        // The on-disk state has the window cleared after recovery.
        let after = state::load_state(&home.state_json("wkA")).unwrap().unwrap();
        let pty = after.pty.unwrap();
        assert!(pty.drive.is_none());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn recovery_marks_missing_state_inconsistent() {
        let home = tmp_home("recover-missing");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(RegistryEntry {
                name: "ghost".into(),
                short_id: "ghost".into(),
                provider: "codex".into(),
                harness: None,
                harness_session_id: None,
                cwd: "/tmp".into(),
                project_root: "/tmp".into(),
                session_id: None,
                legacy_claude_short_id: None,
                claude_session_uuid: None,
                messaging_socket_path: None,
                codex_session_id: None,
                gemini_session_id: None,
                mcp_channel_id: None,
                cc_session_id: None,
                host_mode: None,
                status: AgentStatus::Live,
                last_message_at: None,
                created_at: "2026-05-24T00:00:00Z".into(),
                pid: None,
                pid_start_time: None,
                log_path: None,
                last_reconciled_at: None,
                inside_leg: None,
                exited_at: None,
                mux: None,
                screen_state: None,
            });
        })
        .unwrap();
        // No state.json written for "ghost".
        let report = recover(&home, &emitter);
        assert_eq!(
            report.inconsistent,
            vec![("ghost".to_string(), InconsistencyReason::MissingStateJson)]
        );
        let events = read_events(&home);
        assert!(events
            .iter()
            .any(|e| e["type"] == "agent_inconsistent"
                && e["data"]["reason"] == "missing_state_json"));
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn recovery_skips_claude_shellout_rows_no_spurious_inconsistent() {
        // x-1b1e regression: v9 gives a claude `--bg`/`ask` row a non-empty
        // short_id (the jobId), and an adopted row keeps its external pid. Neither
        // has an fno state.json (their process is claude's, not a daemon PTY), so
        // recover() must NOT probe state_json(jobId) and emit a spurious
        // agent_inconsistent -- the empty-short_id proxy no longer catches them.
        let home = tmp_home("recover-claude-shellout");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        state::update_registry(&home.registry_json(), |r| {
            // bg/ask: host_mode exec (None), pid None.
            let mut bg = bg_claude_row("bg-ask", "7c5dcf5d");
            bg.host_mode = None;
            r.entries.push(bg);
            // adopted: host_mode attached, external pid set.
            let mut adopted = bg_claude_row("cc-adopt", "deadbeef");
            adopted.host_mode = Some(crate::state::HOST_MODE_ATTACHED.into());
            adopted.pid = Some(4242);
            r.entries.push(adopted);
        })
        .unwrap();
        // No state.json written for either row.
        let report = recover(&home, &emitter);
        assert!(
            report.inconsistent.is_empty(),
            "claude shellout/adopted rows must not be flagged inconsistent: {:?}",
            report.inconsistent
        );
        let events = read_events(&home);
        assert!(
            !events.iter().any(|e| e["type"] == "agent_inconsistent"),
            "no agent_inconsistent event for claude shellout rows"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn canonical_name_in_resolves_all_three_address_forms() {
        // x-1b1e regression: the daemon stop/rm handlers must accept name |
        // 8-hex short | full session id (parity with Python `_canonical_agent_name`),
        // not just the name. A miss falls back to the raw token so the familiar
        // `agent {name} not found` still fires.
        let full = "aabbccdd-1111-2222-3333-444455556666";
        let mut row = rentry("billing", AgentStatus::Live, None);
        row.short_id = "a1b2c3d4".into();
        row.harness_session_id = Some(full.into());
        let reg = crate::state::Registry {
            schema_version: crate::state::REGISTRY_SCHEMA_VERSION,
            entries: vec![row],
        };
        assert_eq!(canonical_name_in(&reg, "billing"), "billing"); // by name
        assert_eq!(canonical_name_in(&reg, "a1b2c3d4"), "billing"); // by stored short
        assert_eq!(canonical_name_in(&reg, full), "billing"); // by full session id
        assert_eq!(
            canonical_name_in(&reg, "AABBCCDD-1111-2222-3333-444455556666"),
            "billing"
        ); // case-insensitive
           // Unknown token -> unchanged, so the caller's not-found path fires.
        assert_eq!(canonical_name_in(&reg, "nope"), "nope");
    }

    #[test]
    fn recovery_reaps_dead_pid() {
        let home = tmp_home("recover-reap");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(RegistryEntry {
                name: "dead".into(),
                short_id: "dead".into(),
                provider: "codex".into(),
                harness: None,
                harness_session_id: None,
                cwd: "/tmp".into(),
                project_root: "/tmp".into(),
                session_id: None,
                legacy_claude_short_id: None,
                claude_session_uuid: None,
                messaging_socket_path: None,
                codex_session_id: None,
                gemini_session_id: None,
                mcp_channel_id: None,
                cc_session_id: None,
                host_mode: None,
                status: AgentStatus::Live,
                last_message_at: None,
                created_at: "2026-05-24T00:00:00Z".into(),
                // PID 2^31-ish: almost certainly not a live process.
                pid: Some(0x7fff_fff0),
                pid_start_time: None,
                log_path: None,
                last_reconciled_at: None,
                inside_leg: None,
                exited_at: None,
                mux: None,
                screen_state: None,
            });
        })
        .unwrap();
        // Give it a state.json so it isn't flagged inconsistent.
        let mut st = AgentState::new_pty("dead");
        st.status = AgentStatus::Live;
        state::write_state_atomic(&home.state_json("dead"), &st).unwrap();

        let report = recover(&home, &emitter);
        assert_eq!(report.reaped_pids, vec![0x7fff_fff0]);
        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(reg.find("dead").unwrap().status, AgentStatus::Exited);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn recovery_marks_dead_interactive_exited_and_preserves_host_mode() {
        // AC2-FR (task 2.3): a genuinely dead interactive worker is reaped to
        // Exited (the design's "unexpected exit is exited, not orphaned"), and
        // its host_mode="interactive" round-trips through recovery unchanged so
        // a daemon restart that rediscovers it keeps the field.
        let home = tmp_home("recover-interactive");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        state::update_registry(&home.registry_json(), |r| {
            let mut e = rentry("hosted", AgentStatus::Live, None);
            e.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.to_string());
            e.pid = Some(0x7fff_fff0); // not a live process
            r.entries.push(e);
        })
        .unwrap();
        let mut st = AgentState::new_pty("hosted");
        st.status = AgentStatus::Live;
        state::write_state_atomic(&home.state_json("hosted"), &st).unwrap();

        let _ = recover(&home, &emitter);
        let reg = state::load_registry(&home.registry_json()).unwrap();
        let row = reg.find("hosted").unwrap();
        assert_eq!(
            row.status,
            AgentStatus::Exited,
            "a dead interactive worker is exited, never orphaned"
        );
        assert_eq!(
            row.host_mode_or_default(),
            crate::state::HOST_MODE_INTERACTIVE,
            "host_mode must survive recovery"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn pid_is_ours_distinguishes_recycled_pid() {
        // ab-d19e6458: a live pid whose start time no longer matches the recorded
        // one is a recycled pid, not our worker.
        let me = std::process::id();
        let Some(st) = process_start_time(me) else {
            return; // platform without start-time support; nothing to assert
        };
        assert!(pid_is_ours(me, Some(st)), "correct start time -> ours");
        assert!(
            !pid_is_ours(me, Some(st.wrapping_add(1))),
            "alive but mismatched start time -> recycled, not ours"
        );
        assert!(
            !pid_is_ours(0x7fff_fff0, Some(st)),
            "dead pid is never ours"
        );
        assert!(
            pid_is_ours(me, None),
            "no recorded start time -> fall back to bare liveness (legacy)"
        );
    }

    #[test]
    fn recovery_reaps_recycled_pid() {
        // ab-d19e6458: the recorded pid is ALIVE (our own), but its start time
        // does not match — the original worker died and the pid was reused by an
        // unrelated process. The reap must fire on the start-time mismatch, not
        // be fooled by bare liveness.
        let home = tmp_home("recover-recycled");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let me = std::process::id();
        if process_start_time(me).is_none() {
            std::fs::remove_dir_all(home.root()).ok();
            return; // start-time unsupported here; reuse detection N/A
        }
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(RegistryEntry {
                name: "recycled".into(),
                short_id: "recycled".into(),
                provider: "codex".into(),
                harness: None,
                harness_session_id: None,
                cwd: "/tmp".into(),
                project_root: "/tmp".into(),
                session_id: None,
                legacy_claude_short_id: None,
                claude_session_uuid: None,
                messaging_socket_path: None,
                codex_session_id: None,
                gemini_session_id: None,
                mcp_channel_id: None,
                cc_session_id: None,
                host_mode: None,
                status: AgentStatus::Live,
                last_message_at: None,
                created_at: "2026-05-24T00:00:00Z".into(),
                pid: Some(me),
                // Bogus start time -> mismatch against our real one -> not ours.
                pid_start_time: Some(1),
                log_path: None,
                last_reconciled_at: None,
                inside_leg: None,
                exited_at: None,
                mux: None,
                screen_state: None,
            });
        })
        .unwrap();
        let mut st = AgentState::new_pty("recycled");
        st.status = AgentStatus::Live;
        state::write_state_atomic(&home.state_json("recycled"), &st).unwrap();

        let report = recover(&home, &emitter);
        assert_eq!(report.reaped_pids, vec![me]);
        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(reg.find("recycled").unwrap().status, AgentStatus::Exited);
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn recovery_archives_orphan_state_dir() {
        let home = tmp_home("recover-orphan");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        // A state dir with no registry entry.
        let mut st = AgentState::new_pty("loner");
        st.status = AgentStatus::Live;
        state::write_state_atomic(&home.state_json("loner"), &st).unwrap();

        let report = recover(&home, &emitter);
        assert_eq!(report.archived_orphans, vec!["loner".to_string()]);
        assert!(!home.agent_dir("loner").exists(), "orphan dir moved aside");
        assert!(home.orphaned_dir().exists());
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn agent_name_validation() {
        assert!(valid_agent_name("worker-A_1"));
        assert!(!valid_agent_name(""));
        assert!(!valid_agent_name(&"x".repeat(65)));
        assert!(!valid_agent_name("has space"));
        assert!(!valid_agent_name("inject;rm"));
    }

    #[test]
    fn uuid_v4_shape_and_uniqueness() {
        let a = uuid_v4();
        let b = uuid_v4();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        // version nibble is 4; variant nibble is 8/9/a/b.
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn short_id_derivation_dedups() {
        let mut reg = state::Registry::default();
        assert_eq!(derive_short_id("worker-A", &reg), "workerA");
        reg.entries.push(RegistryEntry {
            name: "x".into(),
            short_id: "workerA".into(),
            provider: "codex".into(),
            harness: None,
            harness_session_id: None,
            cwd: "/".into(),
            project_root: "/".into(),
            session_id: None,
            legacy_claude_short_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: None,
            gemini_session_id: None,
            mcp_channel_id: None,
            cc_session_id: None,
            host_mode: None,
            status: AgentStatus::Live,
            last_message_at: None,
            created_at: "t".into(),
            pid: None,
            pid_start_time: None,
            log_path: None,
            last_reconciled_at: None,
            inside_leg: None,
            exited_at: None,
            mux: None,
            screen_state: None,
        });
        assert_eq!(derive_short_id("worker-A", &reg), "workerA1");
    }

    // --- plan_reconcile (US6.9): tri-state, status-aware transitions, budget ---

    fn rentry(name: &str, status: AgentStatus, last_reconciled: Option<&str>) -> RegistryEntry {
        RegistryEntry {
            name: name.into(),
            short_id: name.into(),
            provider: "codex".into(),
            harness: None,
            harness_session_id: None,
            cwd: "/tmp".into(),
            project_root: "/tmp".into(),
            session_id: Some("sid".into()),
            legacy_claude_short_id: None,
            claude_session_uuid: None,
            messaging_socket_path: None,
            codex_session_id: None,
            gemini_session_id: None,
            mcp_channel_id: None,
            host_mode: None,
            cc_session_id: None,
            status,
            last_message_at: None,
            created_at: "t".into(),
            pid: None,
            pid_start_time: None,
            log_path: None,
            last_reconciled_at: last_reconciled.map(String::from),
            inside_leg: None,
            exited_at: None,
            mux: None,
            screen_state: None,
        }
    }

    fn probe_err() -> crate::provider::ReachabilityProbeError {
        crate::provider::ReachabilityProbeError::new("codex", "store unavailable")
    }

    // --- find_uuid_backfill_row (x-c393): backfill a null-uuid bg row ---------

    /// A `claude --bg` row: jobId in `short_id`, `claude_session_uuid` null.
    fn bg_claude_row(name: &str, short_id: &str) -> RegistryEntry {
        let mut e = rentry(name, AgentStatus::Live, None);
        e.provider = "claude".into();
        e.short_id = short_id.into();
        e.claude_session_uuid = None;
        e
    }

    #[test]
    fn find_uuid_backfill_row_matches_null_uuid_by_short_prefix() {
        // AC1-HP: the full uuid's leading hex group is the row's short-id.
        let rows = vec![bg_claude_row("w", "3228ccad")];
        assert!(matches!(
            find_uuid_backfill_row(&rows, "3228ccad-c078-4b53-a8c9-7199b831eae4"),
            UuidBackfill::One(0)
        ));
    }

    #[test]
    fn find_uuid_backfill_row_refuses_ambiguous_short_collision() {
        // AC1-ERR: two null-uuid rows share the short-id -> refuse, don't guess.
        let rows = vec![
            bg_claude_row("w1", "3228ccad"),
            bg_claude_row("w2", "3228ccad"),
        ];
        assert!(matches!(
            find_uuid_backfill_row(&rows, "3228ccad-c078-4b53-a8c9-7199b831eae4"),
            UuidBackfill::Ambiguous
        ));
    }

    #[test]
    fn find_uuid_backfill_row_skips_rows_that_already_have_a_uuid() {
        // Idempotent: a row already carrying its uuid is matched by the fast
        // path, never backfilled here.
        let mut row = bg_claude_row("w", "3228ccad");
        row.claude_session_uuid = Some("3228ccad-c078-4b53-a8c9-7199b831eae4".into());
        assert!(matches!(
            find_uuid_backfill_row(&[row], "3228ccad-c078-4b53-a8c9-7199b831eae4"),
            UuidBackfill::None
        ));
    }

    #[test]
    fn find_uuid_backfill_row_skips_non_claude_rows() {
        // codex P2: a foreign-provider row carrying a short must not
        // adopt a claude uuid.
        let mut row = bg_claude_row("w", "3228ccad");
        row.provider = "codex".into();
        assert!(matches!(
            find_uuid_backfill_row(&[row], "3228ccad-c078-4b53-a8c9-7199b831eae4"),
            UuidBackfill::None
        ));
    }

    #[test]
    fn find_uuid_backfill_row_requires_group_boundary() {
        // A short must not match a longer hex run it merely prefixes: `3228ccad`
        // is not the leading group of `3228ccadd-...` (no `-` at the boundary).
        let rows = vec![bg_claude_row("w", "3228ccad")];
        assert!(matches!(
            find_uuid_backfill_row(&rows, "3228ccadd-c078-4b53-a8c9-7199b831eae4"),
            UuidBackfill::None
        ));
    }

    #[test]
    fn concurrent_spawn_name_reservation_inserts_once() {
        // Codex P1 (PR #365): two concurrent agent.spawn calls for the same name
        // both pass the lock-free collision check, then race to push. The
        // reservation closure runs inside update_registry's exclusive flock, which
        // serializes the two, so the second observes the first's row and must NOT
        // duplicate it. update_registry's flock makes sequential calls here a
        // faithful stand-in for the serialized concurrent ones.
        let home = tmp_home("spawn-reserve");
        let path = home.registry_json();
        let reserve = |entry: RegistryEntry| -> bool {
            state::update_registry(&path, move |r| {
                if r.entries.iter().any(|e| e.name == entry.name) {
                    return false;
                }
                r.entries.push(entry);
                true
            })
            .unwrap()
        };
        assert!(
            reserve(rentry("dup", AgentStatus::Live, None)),
            "first wins"
        );
        assert!(
            !reserve(rentry("dup", AgentStatus::Live, None)),
            "second loses the race -> no insert"
        );
        let reg = state::load_registry(&path).unwrap();
        assert_eq!(
            reg.entries.iter().filter(|e| e.name == "dup").count(),
            1,
            "exactly one row for the contended name"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn reconcile_flips_unreachable_live_to_orphaned_and_recovers_orphaned() {
        let entries = vec![
            rentry("live-but-gone", AgentStatus::Live, None),
            rentry("back-from-dead", AgentStatus::Orphaned, None),
        ];
        let (changes, out) = plan_reconcile(
            &entries,
            |e| match e.name.as_str() {
                "live-but-gone" => Ok(false), // unreachable
                _ => Ok(true),                // reachable
            },
            || false,
            |_| true,
        );
        assert_eq!(out.orphans, vec!["live-but-gone".to_string()]);
        assert_eq!(out.recovered, vec!["back-from-dead".to_string()]);
        assert_eq!(out.updated.len(), 2);
        // Both probed -> both get a status change recorded.
        assert_eq!(
            changes[0].new_status,
            Some(AgentStatus::Orphaned),
            "unreachable live agent should orphan"
        );
        assert_eq!(changes[1].new_status, Some(AgentStatus::Live));
    }

    #[test]
    fn reconcile_does_not_orphan_a_live_interactive_host_on_store_miss() {
        // US4 (task 2.3): an interactive host whose session-store probe returns
        // unreachable (a live `codex resume`/`gemini -r` TUI may not appear in
        // the exec session index) must NOT be orphaned -- its liveness is the PTY
        // process, governed by the pid-liveness sweep. An exec sibling with the
        // same probe result IS still orphaned, so the branch is host_mode-scoped.
        let mut interactive = rentry("hosted-tui", AgentStatus::Live, None);
        interactive.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.to_string());
        let exec = rentry("one-shot", AgentStatus::Live, None);
        let entries = vec![interactive, exec];
        let (changes, out) = plan_reconcile(&entries, |_| Ok(false), || false, |_| true);
        assert_eq!(
            changes[0].new_status, None,
            "a live interactive host must not be orphaned on a session-store miss"
        );
        assert_eq!(
            changes[1].new_status,
            Some(AgentStatus::Orphaned),
            "an exec sibling with the same probe result is still orphaned"
        );
        assert_eq!(out.orphans, vec!["one-shot".to_string()]);
    }

    #[test]
    fn reconcile_reaps_a_dead_interactive_host_to_exited() {
        // Codex P2 (PR #373): a genuinely dead interactive worker (store-miss AND
        // pid no longer live) must be reaped to Exited DURING reconcile, not left
        // Live until a daemon restart. A live interactive host (pid_live) on the
        // same store-miss stays Live.
        let mut dead = rentry("dead-tui", AgentStatus::Live, None);
        dead.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.to_string());
        let mut live = rentry("live-tui", AgentStatus::Live, None);
        live.host_mode = Some(crate::state::HOST_MODE_INTERACTIVE.to_string());
        let entries = vec![dead, live];
        let (changes, out) = plan_reconcile(
            &entries,
            |_| Ok(false), // both store-miss
            || false,
            |e| e.name == "live-tui", // only live-tui's worker pid is alive
        );
        assert_eq!(
            changes[0].new_status,
            Some(AgentStatus::Exited),
            "a dead interactive host is reaped to Exited during reconcile"
        );
        assert_eq!(
            changes[1].new_status, None,
            "a live interactive host is left untouched"
        );
        // Reaped to Exited, never orphaned.
        assert!(out.orphans.is_empty());
        assert_eq!(out.updated, vec!["dead-tui".to_string()]);
    }

    #[test]
    fn reconcile_inconclusive_preserves_status() {
        let entries = vec![rentry("flaky", AgentStatus::Live, None)];
        let (changes, out) = plan_reconcile(&entries, |_| Err(probe_err()), || false, |_| true);
        assert_eq!(changes[0].new_status, None, "must NOT flip on inconclusive");
        assert!(out.orphans.is_empty());
        assert_eq!(out.inconsistent.len(), 1);
        assert_eq!(out.inconsistent[0].0, "flaky");
    }

    #[test]
    fn reconcile_leaves_terminal_states_untouched() {
        // An exited entry that probes unreachable must NOT become orphaned, and a
        // reachable exited entry must NOT be resurrected to live.
        let entries = vec![
            rentry("done", AgentStatus::Exited, None),
            rentry("dead", AgentStatus::PermanentDead, None),
        ];
        let (changes, out) = plan_reconcile(&entries, |_| Ok(false), || false, |_| true);
        assert!(changes.iter().all(|c| c.new_status.is_none()));
        assert!(out.orphans.is_empty() && out.updated.is_empty());
    }

    /// One-shot `ask` shape: empty short_id + no pid (the discriminator
    /// `is_one_shot_ask` keys on), host_mode exec, a resumable provider session.
    fn ask_entry(name: &str, status: AgentStatus) -> RegistryEntry {
        let mut e = rentry(name, status, None);
        e.short_id = String::new();
        e.pid = None;
        e.codex_session_id = Some("resume-uuid".into());
        e.session_id = None;
        e
    }

    #[test]
    fn reconcile_one_shot_ask_settles_to_exited_even_when_reachable() {
        // AC3-HP: a finished `ask` row settles to Exited regardless of whether its
        // provider session file still exists. The probe here returns Ok(true)
        // (reachable == session file present == "resumable"); the ask branch must
        // ignore it and settle to Exited by process-liveness alone. If the probe
        // were (wrongly) consulted for status, this Live row would stay Live.
        let entries = vec![ask_entry("codex-ask", AgentStatus::Live)];
        let (changes, out) = plan_reconcile(
            &entries,
            |_| Ok(true), // reachable: session file exists -> resumable, NOT running
            || false,
            |_| true,
        );
        assert_eq!(
            changes[0].new_status,
            Some(AgentStatus::Exited),
            "a finished ask settles to exited even when its session file is reachable"
        );
        assert_eq!(out.updated, vec!["codex-ask".to_string()]);
        assert!(out.orphans.is_empty(), "an ask is exited, never orphaned");
        // AC3-EDGE independence: the row's resumable session id is untouched by the
        // status settle (status == liveness; session_id == resumability, separate).
        assert_eq!(entries[0].codex_session_id.as_deref(), Some("resume-uuid"));
    }

    #[test]
    fn reconcile_one_shot_ask_already_terminal_is_untouched() {
        // An ask already Exited must not be re-flagged as updated (idempotent).
        let entries = vec![ask_entry("done-ask", AgentStatus::Exited)];
        let (changes, out) = plan_reconcile(&entries, |_| Ok(true), || false, |_| true);
        assert_eq!(changes[0].new_status, None);
        assert!(out.updated.is_empty());
    }

    #[test]
    fn apply_reconcile_change_clears_pid_only_on_exited() {
        // Locked Decision #7: a row reconciled to Exited drops its pid; any other
        // transition keeps it. Every applied change freshens last_reconciled_at.
        let mut to_exited = rentry("x", AgentStatus::Live, None);
        to_exited.pid = Some(4242);
        to_exited.pid_start_time = Some(99);
        to_exited.inside_leg = Some(state::InsideLegReport {
            state: state::InsideLegState::Working,
            seq: 3,
            reason: None,
            received_at: "2026-06-27T00:00:00Z".into(),
            ttl_ms: None,
        });
        apply_reconcile_change(&mut to_exited, Some(AgentStatus::Exited), "T1");
        assert_eq!(to_exited.status, AgentStatus::Exited);
        assert_eq!(to_exited.pid, None, "exited row must drop its pid");
        assert_eq!(to_exited.pid_start_time, None);
        assert_eq!(
            to_exited.inside_leg, None,
            "exited row must clear the inside-leg authority (E3.3 / AC-X2-4)"
        );
        assert_eq!(to_exited.last_reconciled_at.as_deref(), Some("T1"));

        let mut to_orphaned = rentry("y", AgentStatus::Live, None);
        to_orphaned.pid = Some(4242);
        to_orphaned.inside_leg = Some(state::InsideLegReport {
            state: state::InsideLegState::Working,
            seq: 1,
            reason: None,
            received_at: "2026-06-27T00:00:00Z".into(),
            ttl_ms: None,
        });
        apply_reconcile_change(&mut to_orphaned, Some(AgentStatus::Orphaned), "T2");
        assert_eq!(to_orphaned.status, AgentStatus::Orphaned);
        assert_eq!(
            to_orphaned.pid,
            Some(4242),
            "non-exited transition keeps pid"
        );
        assert!(
            to_orphaned.inside_leg.is_some(),
            "a non-exit transition keeps the inside-leg report (only exit tears it down)"
        );

        // No status change: status held, but CHECKED still freshens (AC2-FR).
        let mut no_change = rentry("z", AgentStatus::Live, Some("OLD"));
        no_change.pid = Some(4242);
        apply_reconcile_change(&mut no_change, None, "T3");
        assert_eq!(no_change.status, AgentStatus::Live);
        assert_eq!(no_change.pid, Some(4242));
        assert_eq!(no_change.last_reconciled_at.as_deref(), Some("T3"));
    }

    #[test]
    fn emit_inside_leg_completion_publishes_only_for_report_bearing_rows() {
        // AC-X2-4: the ordered teardown publishes one completion event carrying
        // the final state for a row that has an inside-leg report, and is a no-op
        // for a plain row (a normal exit with nothing to tear down).
        let home = tmp_home("inside-leg-completion");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");

        let mut with_report = rentry("pane", AgentStatus::Live, None);
        with_report.session_id = Some("sess-uuid".into());
        with_report.inside_leg = Some(state::InsideLegReport {
            state: state::InsideLegState::Working,
            seq: 9,
            reason: Some("running tests".into()),
            received_at: "2026-06-27T00:00:00Z".into(),
            ttl_ms: Some(5000),
        });
        emit_inside_leg_completion(&emitter, &with_report);
        emit_inside_leg_completion(&emitter, &rentry("plain", AgentStatus::Live, None));

        let log = std::fs::read_to_string(home.events_jsonl()).unwrap_or_default();
        let events: Vec<serde_json::Value> = log
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .filter(|v: &serde_json::Value| v["type"] == "inside_leg_completed")
            .collect();
        assert_eq!(
            events.len(),
            1,
            "exactly one completion, only for the report-bearing row"
        );
        let ev = &events[0];
        assert_eq!(ev["data"]["name"], "pane");
        assert_eq!(ev["data"]["session_id"], "sess-uuid");
        assert_eq!(ev["data"]["final_state"], "working");
        assert_eq!(ev["data"]["seq"], 9);

        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn buffer_pending_report_highest_seq_wins_and_is_bounded() {
        use std::collections::HashMap;
        let rep = |seq| state::InsideLegReport {
            state: state::InsideLegState::Working,
            seq,
            reason: None,
            received_at: "2026-06-27T00:00:00Z".into(),
            ttl_ms: None,
        };
        let mut map: HashMap<String, state::InsideLegReport> = HashMap::new();

        // First buffer for a session: stored.
        assert!(matches!(
            buffer_pending_report(&mut map, "s1", rep(2)),
            BufferOutcome::Buffered
        ));
        assert_eq!(map["s1"].seq, 2);

        // A reordered/duplicate early push (seq <= buffered) is dropped, buffer unchanged.
        assert!(matches!(
            buffer_pending_report(&mut map, "s1", rep(1)),
            BufferOutcome::StaleSeq { last: 2 }
        ));
        assert_eq!(
            map["s1"].seq, 2,
            "stale early push must not regress the buffer"
        );

        // A newer push for the same session advances it.
        assert!(matches!(
            buffer_pending_report(&mut map, "s1", rep(5)),
            BufferOutcome::Buffered
        ));
        assert_eq!(map["s1"].seq, 5);

        // Fill to cap with distinct sessions, then a NEW session is dropped (Full),
        // while an existing session still advances.
        for i in 0..PENDING_INSIDE_LEG_CAP {
            buffer_pending_report(&mut map, &format!("fill{i}"), rep(1));
        }
        assert!(map.len() >= PENDING_INSIDE_LEG_CAP);
        assert!(matches!(
            buffer_pending_report(&mut map, "brand-new", rep(1)),
            BufferOutcome::Full
        ));
        assert!(!map.contains_key("brand-new"));
        assert!(
            matches!(
                buffer_pending_report(&mut map, "s1", rep(9)),
                BufferOutcome::Buffered
            ),
            "an already-buffered session advances even at cap (no new key)"
        );
    }

    #[test]
    fn flush_buffered_inside_leg_drains_onto_row_under_seq_gate() {
        // E3.3 flush (race-free): after a row registers, the buffered early-push
        // report is drained onto it and removed from the buffer, with a logged
        // event. A newer report that raced onto the row's store path first is NOT
        // regressed (codex P2: highest-seq-wins survives the flush).
        let home = tmp_home("inside-leg-flush");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("fno-agents-worker"));
        let report = |seq| state::InsideLegReport {
            state: state::InsideLegState::Working,
            seq,
            reason: None,
            received_at: "2026-06-27T00:00:00Z".into(),
            ttl_ms: Some(5000),
        };

        // A registered claude row (inside_leg None) + a buffered report for it.
        let mut row = rentry("pane", AgentStatus::Live, None);
        row.provider = "claude".into();
        row.claude_session_uuid = Some("uuid-x".into());
        state::update_registry(&home.registry_json(), |r| r.entries.push(row)).unwrap();
        ctx.pending_inside_leg
            .lock()
            .unwrap()
            .insert("uuid-x".into(), report(4));

        flush_buffered_inside_leg(&ctx, "uuid-x", "pane");

        // Buffer drained; row carries the report; event logged.
        assert!(!ctx
            .pending_inside_leg
            .lock()
            .unwrap()
            .contains_key("uuid-x"));
        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(reg.entries[0].inside_leg.as_ref().map(|r| r.seq), Some(4));
        let events = read_events(&home);
        assert!(events
            .iter()
            .any(|e| e["type"] == "inside_leg_buffer_flushed"
                && e["data"]["name"] == "pane"
                && e["data"]["session_id"] == "uuid-x"
                && e["data"]["seq"] == 4));

        // Seq gate: a NEWER report already on the row (seq 10) is not regressed by
        // a stale buffered report (seq 7).
        state::update_registry(&home.registry_json(), |r| {
            r.entries[0].inside_leg = Some(report(10));
        })
        .unwrap();
        ctx.pending_inside_leg
            .lock()
            .unwrap()
            .insert("uuid-x".into(), report(7));
        flush_buffered_inside_leg(&ctx, "uuid-x", "pane");
        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert_eq!(
            reg.entries[0].inside_leg.as_ref().map(|r| r.seq),
            Some(10),
            "a stale buffered report must not regress a newer row state"
        );

        std::fs::remove_dir_all(home.root()).ok();
    }

    #[test]
    fn reconcile_defers_remaining_when_budget_exhausted() {
        let entries = vec![
            rentry("a", AgentStatus::Live, None),
            rentry("b", AgentStatus::Live, None),
            rentry("c", AgentStatus::Live, None),
        ];
        // Budget allows exactly one probe, then reports exhausted.
        let mut probes = 0;
        let (changes, out) = plan_reconcile(
            &entries,
            |_| {
                probes += 1;
                Ok(true)
            },
            {
                let mut checked = 0;
                move || {
                    let exhausted = checked >= 1;
                    checked += 1;
                    exhausted
                }
            },
            |_| true,
        );
        assert_eq!(out.deferred, 2, "two trailing entries should defer");
        assert_eq!(changes.len(), 1, "only one entry probed before budget");
    }

    #[test]
    fn run_reconcile_sweep_empty_registry_is_noop() {
        // Boundaries (Architecture B): an empty registry sweeps cleanly -- no
        // entries, no changes -- the startup-path no-op case. Exercises the shared
        // sweep core (load -> sort -> write -> emit) directly.
        let home = tmp_home("sweep-empty");
        let emitter = EventEmitter::new(home.events_jsonl(), "daemon");
        let result = run_reconcile_sweep(&home, &emitter).expect("empty sweep ok");
        assert!(result.entries.is_empty());
        assert_eq!(result.outcome, ReconcileOutcome::default());
        std::fs::remove_dir_all(home.root()).ok();
    }

    // ---------------------------------------------------------------------------
    // poll_until_ready unit tests (Task 1.1: readiness-detector wiring)
    // ---------------------------------------------------------------------------

    /// A detector that reports ready as soon as the visible text ends with "❯".
    struct PromptDetector;
    impl crate::readiness::ReadinessDetector for PromptDetector {
        fn provider_name(&self) -> &str {
            "test-cli"
        }
        fn is_ready(
            &self,
            screen: &crate::readiness::ScreenView,
        ) -> Result<bool, crate::readiness::ReadinessError> {
            Ok(screen.visible_text.trim_end().ends_with('\u{276f}'))
        }
    }

    /// A detector that always returns not-ready (simulates a hung CLI).
    struct NeverReadyDetector;
    impl crate::readiness::ReadinessDetector for NeverReadyDetector {
        fn provider_name(&self) -> &str {
            "never"
        }
        fn is_ready(
            &self,
            _screen: &crate::readiness::ScreenView,
        ) -> Result<bool, crate::readiness::ReadinessError> {
            Ok(false)
        }
    }

    /// AC1-HP: poll_until_ready returns the settled screen text once the
    /// detector reports ready. The reply must come from the ready snapshot,
    /// NOT from an intermediate partial snapshot.
    #[tokio::test(flavor = "current_thread")]
    async fn poll_until_ready_returns_settled_reply_on_ready_prompt() {
        // Three snapshots: two "not ready" then one showing the idle prompt.
        let snapshots: &[&str] = &["loading...", "still loading...", "done \u{276f}"];
        let idx = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let idx2 = idx.clone();
        let fetcher = move || {
            let i = idx2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let text = snapshots[i.min(snapshots.len() - 1)].to_string();
            std::future::ready(Some(text))
        };
        let result = poll_until_ready(
            fetcher,
            Box::new(PromptDetector),
            Duration::from_millis(1),
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        let reply = result.unwrap();
        assert_eq!(
            reply, "done \u{276f}",
            "reply must be the settled snapshot text, got {reply:?}"
        );
    }

    /// AC2-ERR: poll_until_ready returns Err when the timeout elapses before
    /// the detector ever reports ready. It must NOT silently return an empty or
    /// partial reply.
    #[tokio::test(flavor = "current_thread")]
    async fn poll_until_ready_returns_error_on_timeout() {
        let fetcher = || std::future::ready(Some("still thinking...".to_string()));
        let result = poll_until_ready(
            fetcher,
            Box::new(NeverReadyDetector),
            Duration::from_millis(10),
            Duration::from_millis(40), // very short timeout
        )
        .await;
        assert!(
            result.is_err(),
            "expected Err on timeout, got Ok({:?})",
            result.ok()
        );
    }

    /// AC3-EDGE: a settled screen with no reply content returns an empty string,
    /// not fabricated text. (Matches Python `result.reply or ""`.)
    #[tokio::test(flavor = "current_thread")]
    async fn poll_until_ready_empty_settled_screen_returns_empty_string() {
        // The screen text is just the prompt glyph with nothing before it.
        let fetcher = || std::future::ready(Some("\u{276f}".to_string()));
        let result = poll_until_ready(
            fetcher,
            Box::new(PromptDetector),
            Duration::from_millis(1),
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        // The reply is the raw screen text at the settled state. An empty/glyph-only
        // screen is fine — callers use `reply or ""` to handle it.
        let reply = result.unwrap();
        assert!(!reply.contains("fabricated"), "must not fabricate content");
    }

    // -----------------------------------------------------------------------
    // -----------------------------------------------------------------------

    /// E1 fix: the locked one-host re-check matches an interactive claude row by
    /// its `claude_session_uuid`, so a second writer on the same pinned session id
    /// is refused even when the file claim is unavailable (fail-open backstop).
    #[test]
    fn entry_holds_session_matches_claude_session_uuid() {
        let row = build_claude_stream_entry(
            "peer",
            "ab12cd34",
            std::path::Path::new("/work"),
            "sess-uuid-9",
            4242,
            None,
            PathBuf::from("/tmp/log.jsonl"),
        );
        assert!(
            entry_holds_session(&row, "sess-uuid-9"),
            "a claude row must be matched by its claude_session_uuid"
        );
        assert!(!entry_holds_session(&row, "other-uuid"));
    }

    fn test_ctx(home: AgentsHome, worker_bin: PathBuf) -> Ctx {
        Ctx {
            home,
            emitter: EventEmitter::new(std::path::PathBuf::from("/dev/null"), "daemon"),
            opts: DaemonOptions {
                idle_exit: Duration::from_secs(1800),
                worker_bin,
                reconcile_on_start: true,
                dead_row_grace: Duration::from_secs(3600),
                // Off in tests: a unit test must never spawn a real `fno notify`.
                notify_on_blocked: false,
                notify_on_done: false,
            },
            started_at: std::time::Instant::now(),
            exe_fingerprint: crate::drift::ExeFingerprint::current(),
            pid_start_time: process_start_time(std::process::id()),
            pending_inside_leg: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Like `test_ctx` but wires the emitter to `home.events_jsonl()` so
    /// that tests checking emitted events can read them back with `read_events`.
    fn test_ctx_with_events(home: AgentsHome, worker_bin: PathBuf) -> Ctx {
        let events_path = home.events_jsonl();
        Ctx {
            home,
            emitter: EventEmitter::new(events_path, "daemon"),
            opts: DaemonOptions {
                idle_exit: Duration::from_secs(1800),
                worker_bin,
                reconcile_on_start: true,
                dead_row_grace: Duration::from_secs(3600),
                // Off in tests: a unit test must never spawn a real `fno notify`.
                notify_on_blocked: false,
                notify_on_done: false,
            },
            started_at: std::time::Instant::now(),
            exe_fingerprint: crate::drift::ExeFingerprint::current(),
            pid_start_time: process_start_time(std::process::id()),
            pending_inside_leg: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    // ---- Group 2, Task 3.1: switchboard tests --------------------------
    //
    // A fake stream-json emitter (NEVER a real `claude -p`): for each user turn
    // it reads on stdin it emits the canonical sequence (user-echo receipt, a
    // partial, the assistant reply, a result). Mirrors the stream_worker harness.

    const FAKE_STREAM_EMITTER: &str = r#"
printf '%s\n' '{"type":"system","subtype":"init","session_id":"s1"}'
while IFS= read -r line; do
  printf '%s\n' '{"type":"user","message":{"role":"user"}}'
  printf '%s\n' '{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"par"}}}'
  printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"reply-text"}]}}'
  printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"reply-text"}'
done
"#;

    /// A SHORT-path agents home under `/tmp` (not the long `/var/folders` temp
    /// dir): a worker's `<root>/<short_id>/worker.sock` must fit in SUN_LEN
    /// (~104 chars on macOS), so switchboard tests that bind real worker sockets
    /// need a short root. Mirrors the stream_worker test harness.
    fn short_home(tag: &str) -> AgentsHome {
        use std::sync::atomic::{AtomicU32, Ordering};
        static C: AtomicU32 = AtomicU32::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let p = PathBuf::from(format!("/tmp/abisb{tag}{}_{n}", std::process::id()));
        let home = AgentsHome::at(&p);
        home.ensure_root().unwrap();
        home
    }

    /// Seed a held-stream-thread registry row (claude + full UUID + Live).
    fn seed_stream_row(home: &AgentsHome, name: &str, short_id: &str) {
        state::update_registry(&home.registry_json(), |r| {
            r.entries.push(RegistryEntry {
                name: name.into(),
                short_id: short_id.into(),
                provider: "claude".into(),
                harness: None,
                harness_session_id: None,
                cwd: "/tmp".into(),
                project_root: "/tmp".into(),
                session_id: None,
                legacy_claude_short_id: None,
                claude_session_uuid: Some(format!("uuid-{short_id}")),
                messaging_socket_path: None,
                codex_session_id: None,
                gemini_session_id: None,
                mcp_channel_id: None,
                cc_session_id: None,
                host_mode: None,
                status: AgentStatus::Live,
                last_message_at: None,
                created_at: "2026-06-09T00:00:00Z".into(),
                pid: None,
                pid_start_time: None,
                log_path: None,
                last_reconciled_at: None,
                inside_leg: None,
                exited_at: None,
                mux: None,
                screen_state: None,
            });
        })
        .unwrap();
    }

    /// Start a real stream worker (fake emitter child) on `home.worker_sock(id)`
    /// via the PUBLIC `stream_worker::run`; wait for its socket to appear.
    async fn start_stream_worker(home: &AgentsHome, short_id: &str, script: &str) -> PathBuf {
        let cfg = crate::stream_worker::StreamWorkerConfig::new(
            short_id,
            home.root().to_path_buf(),
            std::env::temp_dir(),
            vec!["bash".into(), "-c".into(), script.into()],
        );
        let short_id_dbg = short_id.to_string();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                if let Err(e) = crate::stream_worker::run(cfg).await {
                    eprintln!("STREAM WORKER RUN ERROR ({short_id_dbg}): {e}");
                }
            });
        });
        let sock = home.worker_sock(short_id);
        let start = std::time::Instant::now();
        while !sock.exists() && start.elapsed() < Duration::from_secs(20) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            sock.exists(),
            "stream worker socket never appeared for {short_id}"
        );
        sock
    }

    /// Locate the cargo-built `fno-agents-worker` next to the test binary
    /// (target/debug/deps/<test> -> target/debug/fno-agents-worker). `None` if it
    /// is not built, so the e2e adopt test SKIPS rather than failing in an
    /// environment where only the lib test target was compiled.
    fn built_worker_bin() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?.parent()?; // deps -> debug
        let cand = dir.join("fno-agents-worker");
        cand.exists().then_some(cand)
    }

    // ---- Group 3 (ab-734fcd6c): claude stream-json front door --------------

    #[test]
    fn stream_claim_holder_is_short_id_scoped() {
        assert_eq!(stream_claim_holder("sw7"), "stream:sw7");
    }

    /// E1 (codex P2): interactive claude resolves to a real readiness detector,
    /// not the fail-loud NoSignalDetector, so `agent.ask` against it does not
    /// time out with "no readiness signal".
    #[test]
    fn provider_readiness_detector_handles_claude() {
        let d = provider_readiness_detector("claude");
        assert_eq!(d.provider_name(), "claude");
        // A truly unknown provider still gets the NoSignalDetector (name
        // carried). opencode graduated to a real match arm (x-51f6) - using
        // it here would coincidentally still pass (both paths report
        // provider_name() == "opencode") while silently testing the wrong
        // thing, so aider (still genuinely unhosted) is the example now.
        assert_eq!(
            provider_readiness_detector("aider").provider_name(),
            "aider"
        );
    }

    #[test]
    fn is_live_writer_excludes_orphaned_and_terminal() {
        // Live-ish: a real writer holds the session -> one-host refuses a re-adopt.
        for s in [
            AgentStatus::Live,
            AgentStatus::Ready,
            AgentStatus::Idle,
            AgentStatus::Busy,
            AgentStatus::Spawning,
            AgentStatus::Restarting,
        ] {
            assert!(is_live_writer(s), "{s:?} should count as a live writer");
        }
        // Dead-but-non-terminal + terminal: the session is re-adoptable (AC1-FR).
        for s in [
            AgentStatus::Orphaned,
            AgentStatus::Failed,
            AgentStatus::Exited,
            AgentStatus::PermanentDead,
        ] {
            assert!(!is_live_writer(s), "{s:?} must NOT block re-adoption");
        }
    }

    #[test]
    fn acquire_session_claim_maps_native_outcomes() {
        let td = tempfile::tempdir().unwrap();
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("FNO_CLAIMS_ROOT", td.path());
        // Fresh acquire -> Acquired.
        assert!(matches!(
            acquire_session_claim("U-1", "stream:sw1"),
            ClaimOutcome::Acquired
        ));
        // Same holder re-acquire -> still Acquired (idempotent).
        assert!(matches!(
            acquire_session_claim("U-1", "stream:sw1"),
            ClaimOutcome::Acquired
        ));
        // A different holder against a LIVE claim -> HeldByOther naming the
        // incumbent (the claim is pinned to this live test process).
        match acquire_session_claim("U-1", "stream:other") {
            ClaimOutcome::HeldByOther(who) => assert_eq!(who, "stream:sw1"),
            other => panic!("expected HeldByOther, got {other:?}"),
        }
        std::env::remove_var("FNO_CLAIMS_ROOT");
    }

    #[test]
    fn claude_stream_worker_args_carry_stream_flags_and_child_argv() {
        let child = crate::provider::claude_stream_json_resume_argv("U-9");
        let args = claude_stream_worker_args(
            "sw9",
            std::path::Path::new("/home/agents"),
            std::path::Path::new("/work"),
            "U-9",
            "stream:sw9",
            &child,
        );
        // Selector + claim pair are present, the child argv follows `--`, and the
        // resume target is the FULL uuid (never the jobId).
        assert!(args.contains(&"--stream".to_string()));
        assert_eq!(
            args.iter()
                .position(|a| a == "--session-uuid")
                .map(|i| &args[i + 1]),
            Some(&"U-9".to_string())
        );
        assert_eq!(
            args.iter()
                .position(|a| a == "--holder")
                .map(|i| &args[i + 1]),
            Some(&"stream:sw9".to_string())
        );
        let sep = args
            .iter()
            .position(|a| a == "--")
            .expect("missing -- separator");
        assert_eq!(&args[sep + 1..], child.as_slice());
        assert_eq!(child[0], "claude");
        assert!(child.contains(&"--resume".to_string()) && child.contains(&"U-9".to_string()));
    }

    #[test]
    fn build_claude_stream_entry_marks_interactive_claude_with_full_uuid() {
        let e = build_claude_stream_entry(
            "adopted",
            "sw3",
            std::path::Path::new("/proj"),
            "FULL-UUID-3",
            4242,
            Some(99),
            PathBuf::from("/proj/.fno/agents/sw3/timeline.jsonl"),
        );
        assert_eq!(e.provider, "claude");
        assert_eq!(
            e.host_mode.as_deref(),
            Some(crate::state::HOST_MODE_INTERACTIVE)
        );
        assert!(
            e.is_interactive(),
            "stream thread must read as interactive for reconcile"
        );
        assert_eq!(e.claude_session_uuid.as_deref(), Some("FULL-UUID-3"));
        assert_eq!(e.status, AgentStatus::Live);
        assert_eq!(e.pid, Some(4242));
        // The resume key lives in claude_session_uuid; a stream thread carries
        // its worker short in short_id ("sw3"), not the removed jobId field.
        assert_eq!(e.short_id, "sw3");
    }

    /// AC1-ERR / front-door routing: a fresh `host --provider claude` with no
    /// `--from` has nothing to resume; it is rejected (before any claim/spawn)
    /// with a pointer to the adopt verb, proving claude routed to the stream lane
    /// (not the codex/gemini PTY "only codex or gemini" gate).
    #[tokio::test(flavor = "current_thread")]
    async fn host_claude_without_from_rejected_with_adopt_pointer() {
        let home = short_home("clnofrom");
        let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));
        let req = Request::new(
            1,
            "agent.spawn",
            json!({"name": "cl", "provider": "claude", "host_mode": "interactive"}),
        );
        let resp = handle_spawn(&ctx, &req).await;
        match &resp.payload {
            crate::protocol::ResponsePayload::Err(e) => {
                assert_eq!(e.code, ErrorCode::InvalidParams);
                assert!(
                    e.message.contains("promote") && e.message.contains("--from"),
                    "claude host without --from must point at the adopt verb; got: {}",
                    e.message
                );
            }
            _ => panic!("expected error for claude host without --from"),
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// AC1-EDGE single-writer: a second adopt of a session already held by a live
    /// claude thread is refused (one writer per session), before any spawn. Uses
    /// the lock-free one-host pre-check so it is hermetic (no worker, no claim).
    #[tokio::test(flavor = "current_thread")]
    async fn promote_claude_duplicate_session_refused() {
        let home = short_home("cldup");
        seed_stream_row(&home, "first", "swDup"); // claude_session_uuid = uuid-swDup, Live
        let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));
        let req = Request::new(
            1,
            "agent.spawn",
            json!({
                "name": "second", "provider": "claude", "host_mode": "interactive",
                "resume_id": "uuid-swDup"
            }),
        );
        let resp = handle_spawn(&ctx, &req).await;
        match &resp.payload {
            crate::protocol::ResponsePayload::Err(e) => {
                assert_eq!(e.code, ErrorCode::InvalidParams);
                assert!(
                    e.message.contains("already hosted") && e.message.contains("first"),
                    "duplicate adopt must name the existing host; got: {}",
                    e.message
                );
            }
            _ => panic!("expected single-writer refusal for duplicate adopt"),
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// AC1-HP end-to-end: `promote --provider claude --from <uuid>` adopts an idle
    /// session by spawning the real `--stream` worker (with a FAKE emitter child,
    /// never a real `claude -p`) and registering it `live`. The row carries
    /// provider=claude + host_mode=interactive + the FULL uuid, and the worker
    /// serves the stream protocol. Skips when the worker binary is not built.
    #[tokio::test(flavor = "current_thread")]
    async fn promote_claude_spawns_live_stream_thread() {
        let Some(worker_bin) = built_worker_bin() else {
            eprintln!("skip promote_claude_spawns_live_stream_thread: worker bin not built");
            return;
        };
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = short_home("cle2e");
        // Hermetic claims: point `fno claim` at the test home so the real
        // acquire (daemon, this process) AND the worker child (inherits this env)
        // write `session:uuid-e2e` under /tmp, never the canonical, shared
        // ~/.fno/claims. A panic before teardown then leaks at worst into a
        // throwaway /tmp dir. Only this test exercises claims, so the process-wide
        // env set does not race the claim-free tests. (Edition 2021: set_var safe.)
        std::env::set_var("FNO_CLAIMS_ROOT", home.root());
        let ctx = test_ctx(home.clone(), worker_bin);
        let req = Request::new(
            1,
            "agent.spawn",
            json!({
                "name": "cl", "provider": "claude", "host_mode": "interactive",
                "resume_id": "uuid-e2e", "cwd": "/tmp",
                // Test escape hatch: a fake stream emitter stands in for `claude -p`.
                "argv": ["bash", "-c", FAKE_STREAM_EMITTER]
            }),
        );
        let resp = handle_spawn(&ctx, &req).await;
        let res = resp.result().expect("claude adopt errored");
        assert_eq!(res["provider"], "claude");
        assert_eq!(res["status"], "live");
        assert_eq!(res["lane"], "stream");

        let reg = load_registry_offloaded(home.registry_json()).await;
        let row = reg.find("cl").expect("adopted row missing");
        assert_eq!(row.provider, "claude");
        assert_eq!(row.host_mode.as_deref(), Some("interactive"));
        assert_eq!(row.claude_session_uuid.as_deref(), Some("uuid-e2e"));
        assert_eq!(row.status, AgentStatus::Live);

        let sock = home.worker_sock(&row.short_id);
        assert!(
            is_live_stream_thread(&sock).await,
            "adopted thread must serve the stream protocol"
        );

        // Teardown: shut the worker down (its RAII guard releases the claim), then
        // drop the test home (which holds the redirected claims dir) and clear the
        // env override so later tests see the default claims root.
        best_effort_worker_shutdown(&sock).await;
        std::fs::remove_dir_all(home.root()).ok();
        std::env::remove_var("FNO_CLAIMS_ROOT");
    }

    /// AC1-ERR (codex review P2): a dead-on-arrival `claude -p --resume` (bad uuid
    /// / auth fail, here a child that exits immediately) must NOT register live.
    /// The worker still binds + answers stream.ping, but stream.status.child_alive
    /// is false, so adopt is rejected and no row is created.
    #[tokio::test(flavor = "current_thread")]
    async fn promote_claude_dead_on_arrival_resume_rejected() {
        let Some(worker_bin) = built_worker_bin() else {
            eprintln!("skip promote_claude_dead_on_arrival_resume_rejected: worker bin not built");
            return;
        };
        let _guard = crate::claims::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = short_home("cldoa");
        std::env::set_var("FNO_CLAIMS_ROOT", home.root());
        let ctx = test_ctx(home.clone(), worker_bin);
        let req = Request::new(
            1,
            "agent.spawn",
            json!({
                "name": "cl", "provider": "claude", "host_mode": "interactive",
                "resume_id": "uuid-doa", "cwd": "/tmp",
                // Child exits immediately -> stands in for a bad/expired --resume id.
                "argv": ["bash", "-c", "exit 1"]
            }),
        );
        let resp = handle_spawn(&ctx, &req).await;
        assert!(
            resp.is_err(),
            "DOA resume child must be rejected, not registered"
        );
        assert_eq!(resp.error().unwrap().code, ErrorCode::SpawnFailed);
        let reg = load_registry_offloaded(home.registry_json()).await;
        assert!(
            reg.find("cl").is_none(),
            "no row may be registered for a DOA adopt"
        );
        std::fs::remove_dir_all(home.root()).ok();
        std::env::remove_var("FNO_CLAIMS_ROOT");
    }

    /// AC2-HP: `send A->B` between two held stream threads drives B, discriminates
    /// the user-echo receipt from the reply, and mirrors B's reply into A.
    #[tokio::test(flavor = "current_thread")]
    async fn switchboard_drives_b_and_mirrors_into_a() {
        let home = short_home("hp");
        seed_stream_row(&home, "A", "swA");
        seed_stream_row(&home, "B", "swB");
        let _a = start_stream_worker(&home, "swA", FAKE_STREAM_EMITTER).await;
        let _b = start_stream_worker(&home, "swB", FAKE_STREAM_EMITTER).await;
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("/nonexistent-worker"));

        let req = Request::new(
            1,
            "agent.switchboard",
            json!({"to": "B", "from": "A", "body": "hello"}),
        );
        let resp = handle_switchboard(&ctx, &req).await;
        let res = resp.result().expect("switchboard errored");
        assert_eq!(res["delivered"], true, "not delivered: {res:?}");
        assert_eq!(res["reply"], "reply-text");
        assert_eq!(res["is_error"], false);
        assert_eq!(res["receipt"], true, "user-echo receipt not observed");
        assert_eq!(res["mirrored"], true, "B's reply was not mirrored into A");

        // The injected-event reuse carries the switchboard transport discriminator.
        let events = read_events(&home);
        assert!(
            events.iter().any(|e| e["type"] == "agent_deliver_injected"
                && e["data"]["transport"] == "switchboard"
                && e["data"]["mirrored"] == true),
            "switchboard injected event missing: {events:?}"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// A second turn against the SAME persistent worker must return the SECOND
    /// turn's reply, not the stale first result still in the append-only frame
    /// log. Regression for the cursor=0 bug: the emitter tags each reply with a
    /// per-turn counter so a stale read is detectable.
    #[tokio::test(flavor = "current_thread")]
    async fn switchboard_second_turn_returns_fresh_reply() {
        const COUNTING_EMITTER: &str = r#"
printf '%s\n' '{"type":"system","subtype":"init","session_id":"s1"}'
n=0
while IFS= read -r line; do
  n=$((n+1))
  printf '%s\n' '{"type":"user","message":{"role":"user"}}'
  printf '%s\n' "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"reply-$n\"}]}}"
  printf '%s\n' "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"reply-$n\"}"
done
"#;
        let home = short_home("fresh");
        seed_stream_row(&home, "B", "swB");
        let _b = start_stream_worker(&home, "swB", COUNTING_EMITTER).await;
        let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));

        let r1 = handle_switchboard(
            &ctx,
            &Request::new(
                1,
                "agent.switchboard",
                json!({"to": "B", "from": "ghost", "body": "first"}),
            ),
        )
        .await;
        assert_eq!(r1.result().expect("hop1")["reply"], "reply-1");

        let r2 = handle_switchboard(
            &ctx,
            &Request::new(
                2,
                "agent.switchboard",
                json!({"to": "B", "from": "ghost", "body": "second"}),
            ),
        )
        .await;
        assert_eq!(
            r2.result().expect("hop2")["reply"],
            "reply-2",
            "second drive returned a STALE reply (cursor not advanced past the prior turn)"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// Routing: a claude peer with no live stream worker demotes (the caller
    /// falls back to the durable/socket path), not an error.
    #[tokio::test(flavor = "current_thread")]
    async fn switchboard_demotes_when_b_not_a_live_stream_thread() {
        let home = short_home("demote");
        seed_stream_row(&home, "B", "swB"); // registered, but NO worker started
        let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));

        let req = Request::new(
            1,
            "agent.switchboard",
            json!({"to": "B", "from": "A", "body": "hi"}),
        );
        let resp = handle_switchboard(&ctx, &req).await;
        let res = resp
            .result()
            .expect("should be Ok-demote, not an RPC error");
        assert_eq!(res["delivered"], false);
        assert_eq!(res["reason"], "not-a-live-stream-thread");
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// Degenerate one-way drive: B is a held stream thread but A is not (absent),
    /// so the turn delivers to B with no mirror.
    #[tokio::test(flavor = "current_thread")]
    async fn switchboard_one_way_when_peer_absent() {
        let home = short_home("oneway");
        seed_stream_row(&home, "B", "swB");
        let _b = start_stream_worker(&home, "swB", FAKE_STREAM_EMITTER).await;
        let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));

        let req = Request::new(
            1,
            "agent.switchboard",
            json!({"to": "B", "from": "ghost", "body": "hi"}),
        );
        let resp = handle_switchboard(&ctx, &req).await;
        let res = resp.result().expect("switchboard errored");
        assert_eq!(res["delivered"], true);
        assert_eq!(res["reply"], "reply-text");
        assert_eq!(res["mirrored"], false, "no peer to mirror into");
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// An unknown `to` is an RPC error (AgentNotFound), not a silent no-op.
    #[tokio::test(flavor = "current_thread")]
    async fn switchboard_unknown_target_is_not_found() {
        let home = short_home("404");
        let ctx = test_ctx(home.clone(), PathBuf::from("/nonexistent-worker"));
        let req = Request::new(
            1,
            "agent.switchboard",
            json!({"to": "nope", "from": "A", "body": "hi"}),
        );
        let resp = handle_switchboard(&ctx, &req).await;
        if let crate::protocol::ResponsePayload::Err(ref e) = resp.payload {
            assert_eq!(e.code, ErrorCode::AgentNotFound);
        } else {
            panic!("expected AgentNotFound, got {resp:?}");
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// Post-G4 (x-f54c): a codex spawn (interactive PTY hosting) is retired -- the
    /// daemon serves only the claude stream-json adopt lane, so any other spawn
    /// returns the mux-pointer InvalidParams error.
    #[tokio::test(flavor = "current_thread")]
    async fn handle_spawn_codex_pty_hosting_retired_returns_pointer() {
        let home = tmp_home("spawn-provider-argv");
        let ctx = test_ctx(
            home.clone(),
            PathBuf::from("/nonexistent/fno-agents-worker"),
        );
        let req = Request::new(
            1,
            "agent.spawn",
            json!({"name": "test-agent", "provider": "codex"}),
        );
        let resp = handle_spawn(&ctx, &req).await;
        match &resp.payload {
            crate::protocol::ResponsePayload::Err(e) => {
                assert_eq!(e.code, ErrorCode::InvalidParams);
                assert!(
                    e.message.contains("retired at G4"),
                    "codex spawn must point at the mux; got: {}",
                    e.message
                );
            }
            _ => panic!("expected the G4 retirement error for a codex PTY spawn"),
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// AC3-ERR: handle_spawn with an unknown/non-PTY provider and no argv returns InvalidParams.
    #[tokio::test(flavor = "current_thread")]
    async fn handle_spawn_unknown_provider_no_argv_returns_invalid_params() {
        let home = tmp_home("spawn-unknown-provider");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(
            1,
            "agent.spawn",
            json!({"name": "test-agent", "provider": "nonexistent-provider"}),
        );
        let resp = handle_spawn(&ctx, &req).await;
        match &resp.payload {
            crate::protocol::ResponsePayload::Err(e) => {
                assert_eq!(
                    e.code,
                    ErrorCode::InvalidParams,
                    "unknown provider without argv must return InvalidParams"
                );
            }
            _ => panic!("expected error response for unknown provider"),
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// AC4-HP: handle_ask on AgentNotFound with a provider param routes into the
    /// first-contact spawn branch (does NOT short-circuit AgentNotFound). Post-G4
    /// (x-f54c) that spawn is the retired codex PTY-hosting path, so the daemon
    /// surfaces the mux pointer rather than auto-creating a worker; the point of
    /// the test is that first-contact attempted a spawn (not AgentNotFound).
    #[tokio::test(flavor = "current_thread")]
    async fn handle_ask_first_contact_with_provider_routes_into_spawn() {
        let home = tmp_home("ask-first-contact");
        let ctx = test_ctx(
            home.clone(),
            PathBuf::from("/nonexistent/fno-agents-worker"),
        );
        // Agent does not exist yet; provider="codex" is provided.
        let req = Request::new(
            1,
            "agent.ask",
            json!({"name": "new-agent", "message": "hello", "provider": "codex"}),
        );
        let resp = handle_ask(&ctx, &req).await;
        match &resp.payload {
            crate::protocol::ResponsePayload::Err(e) => {
                assert_ne!(
                    e.code,
                    ErrorCode::AgentNotFound,
                    "first-contact ask with --provider must route into the spawn branch, not short-circuit AgentNotFound; got: {}",
                    e.message
                );
                // Post-G4 the codex spawn is retired -> the mux pointer.
                assert!(
                    e.message.contains("retired at G4"),
                    "first-contact codex spawn must surface the G4 mux pointer; got: {}",
                    e.message
                );
            }
            crate::protocol::ResponsePayload::Ok(v) => {
                panic!("post-G4 a codex first-contact spawn must fail, got Ok: {v}")
            }
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// AC5-ERR: handle_ask on AgentNotFound WITHOUT a provider returns InvalidParams
    /// (mirrors Python requiring --provider on first contact).
    #[tokio::test(flavor = "current_thread")]
    async fn handle_ask_first_contact_without_provider_returns_invalid_params() {
        let home = tmp_home("ask-no-provider");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        // Agent does not exist; NO provider param.
        let req = Request::new(
            1,
            "agent.ask",
            json!({"name": "ghost-agent", "message": "hello"}),
        );
        let resp = handle_ask(&ctx, &req).await;
        match &resp.payload {
            crate::protocol::ResponsePayload::Err(e) => {
                assert_eq!(
                    e.code,
                    ErrorCode::InvalidParams,
                    "first-contact ask without --provider must return InvalidParams; got: {}",
                    e.message
                );
                assert!(
                    e.message.contains("provider"),
                    "error message must mention 'provider', got: {}",
                    e.message
                );
            }
            _ => panic!("expected error for first-contact ask without provider"),
        }
        std::fs::remove_dir_all(home.root()).ok();
    }

    // ── gate record tests (Task 2.3) ─────────────────────────────────────────

    // ---- inside-leg report (E3.2) ------------------------------------------

    /// AC-X2 store: a report for a registered claude session lands on the row's
    /// `inside_leg` field with the daemon-stamped `received_at`, and emits
    /// `inside_leg_report`.
    #[test]
    fn handle_report_stores_on_matching_row() {
        let home = tmp_home("report-store");
        seed_stream_row(&home, "worker-A", "repA"); // claude_session_uuid = uuid-repA
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("fno-agents-worker"));
        let req = Request::new(
            1,
            "agent.report",
            json!({"session_id": "uuid-repA", "seq": 3, "state": "working", "reason": "running tests"}),
        );
        let resp = handle_report(&ctx, &req);
        assert!(!resp.is_err(), "report must return Ok: {resp:?}");
        assert_eq!(resp.result().unwrap()["stored"], true);

        let reg = state::load_registry(&home.registry_json()).unwrap();
        let rep = reg.entries[0]
            .inside_leg
            .as_ref()
            .expect("inside_leg stored");
        assert_eq!(rep.state, state::InsideLegState::Working);
        assert_eq!(rep.seq, 3);
        assert_eq!(rep.reason.as_deref(), Some("running tests"));
        assert!(!rep.received_at.is_empty(), "daemon stamps received_at");

        let events = read_events(&home);
        assert!(
            events.iter().any(|e| e["type"] == "inside_leg_report"),
            "inside_leg_report not emitted: {events:?}"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// Capability flip (screen-manifest fallback authority): the row's FIRST
    /// inside-leg report makes the hook the sole authority - a stored scrape
    /// verdict is cleared in the same registry write, so it can never shadow
    /// the hook.
    #[test]
    fn handle_report_capability_flip_clears_screen_state() {
        let home = tmp_home("report-flip-clears-scrape");
        seed_stream_row(&home, "worker-A", "repF");
        state::update_registry(&home.registry_json(), |r| {
            r.entries[0].screen_state = Some(state::ScreenStateReport {
                state: "idle".into(),
                rule: "idle_prompt".into(),
                seq: 4,
                at: "2026-07-02T00:00:00Z".into(),
                ttl_ms: Some(120_000),
                answerable: None,
            });
        })
        .unwrap();
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("fno-agents-worker"));
        let resp = handle_report(
            &ctx,
            &Request::new(
                1,
                "agent.report",
                json!({"session_id": "uuid-repF", "seq": 1, "state": "working"}),
            ),
        );
        assert_eq!(resp.result().unwrap()["stored"], true);
        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert!(reg.entries[0].inside_leg.is_some());
        assert_eq!(
            reg.entries[0].screen_state, None,
            "capability flip must clear the scrape verdict"
        );
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// AC-X2-1 seq: a `seq <= last_seq` is dropped (the newer report wins) and
    /// emits `inside_leg_report_dropped`.
    #[test]
    fn handle_report_drops_stale_seq() {
        let home = tmp_home("report-stale");
        seed_stream_row(&home, "worker-A", "repB");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("fno-agents-worker"));
        // seq=2 stored, then a reordered seq=1 arrives.
        let _ = handle_report(
            &ctx,
            &Request::new(
                1,
                "agent.report",
                json!({"session_id": "uuid-repB", "seq": 2, "state": "working"}),
            ),
        );
        let resp = handle_report(
            &ctx,
            &Request::new(
                2,
                "agent.report",
                json!({"session_id": "uuid-repB", "seq": 1, "state": "done"}),
            ),
        );
        assert!(!resp.is_err());
        assert_eq!(resp.result().unwrap()["stored"], false);
        assert_eq!(resp.result().unwrap()["dropped"], "stale_seq");

        // The badge still reflects seq=2/working, not the late seq=1/done.
        let reg = state::load_registry(&home.registry_json()).unwrap();
        let rep = reg.entries[0].inside_leg.as_ref().unwrap();
        assert_eq!(rep.seq, 2);
        assert_eq!(rep.state, state::InsideLegState::Working);

        let events = read_events(&home);
        assert!(events.iter().any(
            |e| e["type"] == "inside_leg_report_dropped" && e["data"]["reason"] == "stale_seq"
        ));
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// AC-X2-5 + E3.3 buffer-on-early-push: a push for an unregistered session id
    /// is BUFFERED (no longer hard-dropped) with a logged event and adds no
    /// phantom row. The buffered report is flushed onto the row at creation.
    #[test]
    fn handle_report_buffers_early_push_for_unknown_session() {
        let home = tmp_home("report-unknown");
        let ctx = test_ctx_with_events(home.clone(), PathBuf::from("fno-agents-worker"));
        let resp = handle_report(
            &ctx,
            &Request::new(
                1,
                "agent.report",
                json!({"session_id": "uuid-nope", "seq": 1, "state": "working"}),
            ),
        );
        assert!(!resp.is_err());
        assert_eq!(resp.result().unwrap()["stored"], false);
        assert_eq!(
            resp.result().unwrap()["buffered"],
            true,
            "an early push is held, not dropped (E3.3)"
        );

        let reg = state::load_registry(&home.registry_json()).unwrap();
        assert!(reg.entries.is_empty(), "no phantom row created");
        // The report is held in the pending buffer keyed by session_id.
        assert_eq!(
            ctx.pending_inside_leg
                .lock()
                .unwrap()
                .get("uuid-nope")
                .map(|r| r.seq),
            Some(1)
        );

        let events = read_events(&home);
        assert!(events
            .iter()
            .any(|e| e["type"] == "inside_leg_report_buffered"
                && e["data"]["session_id"] == "uuid-nope"));
        std::fs::remove_dir_all(home.root()).ok();
    }

    /// Missing/invalid params fail closed with InvalidParams (no registry write).
    #[test]
    fn handle_report_rejects_bad_params() {
        let home = tmp_home("report-bad");
        let ctx = test_ctx(home.clone(), PathBuf::from("fno-agents-worker"));
        for params in [
            json!({"seq": 1, "state": "working"}),          // no session_id
            json!({"session_id": "x", "state": "working"}), // no seq
            json!({"session_id": "x", "seq": 1}),           // no state
            json!({"session_id": "x", "seq": 1, "state": "idle"}), // bad state
        ] {
            let resp = handle_report(&ctx, &Request::new(1, "agent.report", params.clone()));
            assert!(resp.is_err(), "expected InvalidParams for {params}");
        }
        std::fs::remove_dir_all(home.root()).ok();
    }
}
